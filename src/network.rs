//! NetworkManager Wi-Fi Direct transport. No subprocess or libnm dependency.
use anyhow::{Context, Result, bail};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};
use zbus::{
    blocking::{Connection, Proxy},
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

const NM: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const P2P: &str = "org.freedesktop.NetworkManager.Device.WifiP2P";
const PEER: &str = "org.freedesktop.NetworkManager.WifiP2PPeer";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
pub const LOCAL_ADDRESS: &str = "192.168.49.10";
pub const TV_ADDRESS: &str = "192.168.49.1";
const WFD_SOURCE_R2: [u8; 14] = [0, 0, 6, 0, 0x90, 0x1c, 0x44, 0, 0xc8, 0x0b, 0, 2, 0, 0];

#[derive(Debug, Clone, serde::Serialize)]
pub struct Peer {
    pub name: String,
    pub mac: String,
    pub device: OwnedObjectPath,
    pub peer: OwnedObjectPath,
}

pub struct Network {
    bus: Connection,
}

impl Network {
    pub fn new() -> Result<Self> {
        Ok(Self {
            bus: Connection::system().context("Cannot connect to the system D-Bus")?,
        })
    }

    fn proxy<'a>(&'a self, path: &'a str, interface: &'a str) -> Result<Proxy<'a>> {
        Ok(Proxy::new(&self.bus, NM, path, interface)?)
    }

    /// Cancellation closes this instance's D-Bus connection, interrupting even
    /// an in-flight method call. The instance cannot be reused after cancellation.
    pub fn discover_with_cancel(&self, timeout: Duration, stop: &AtomicBool) -> Result<Vec<Peer>> {
        self.with_cancel(stop, || self.discover_inner(timeout, stop))
    }

    pub fn connect_with_cancel(
        &self,
        peer: &Peer,
        timeout: Duration,
        stop: &AtomicBool,
    ) -> Result<ActiveConnection> {
        self.with_cancel(stop, || self.connect_inner(peer, timeout, stop))
    }

    fn with_cancel<T>(
        &self,
        stop: &AtomicBool,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        cancelled(stop)?;
        let finished = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                while !finished.load(Ordering::Acquire) {
                    if stop.load(Ordering::Acquire) {
                        // All clones share the same socket. NM releases only the
                        // activation bound to this client's unique bus name.
                        let _ = self.bus.clone().close();
                        return;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            });
            let result = operation();
            finished.store(true, Ordering::Release);
            cancelled(stop)?;
            result
        })
    }

    fn discover_inner(&self, timeout: Duration, stop: &AtomicBool) -> Result<Vec<Peer>> {
        let seconds = timeout.as_secs().clamp(1, 600) as i32;
        let devices: Vec<OwnedObjectPath> = self.proxy(ROOT, NM)?.call("GetDevices", &())?;
        let mut p2p_devices = Vec::new();
        for device in devices {
            cancelled(stop)?;
            if self
                .proxy(device.as_str(), DEVICE)?
                .get_property::<u32>("DeviceType")?
                == 30
            {
                let options = HashMap::from([("timeout", Value::from(seconds))]);
                self.proxy(device.as_str(), P2P)?
                    .call::<_, _, ()>("StartFind", &(options,))?;
                p2p_devices.push(device);
            }
        }
        if p2p_devices.is_empty() {
            bail!(
                "NetworkManager has no Wi-Fi Direct device. Check the Wi-Fi adapter and wpa_supplicant."
            );
        }
        let deadline = Instant::now() + Duration::from_secs(seconds as u64);
        let mut found = HashMap::new();
        loop {
            cancelled(stop)?;
            for device in &p2p_devices {
                let peers: Vec<OwnedObjectPath> =
                    self.proxy(device.as_str(), P2P)?.get_property("Peers")?;
                for peer in peers {
                    cancelled(stop)?;
                    // A peer can disappear between Peers and its property reads.
                    if let Ok(Some(display)) = self.read_peer(device, &peer) {
                        found.insert(display.mac.clone(), display);
                    }
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let mut peers: Vec<_> = found.into_values().collect();
        peers.sort_by(|a, b| a.name.cmp(&b.name).then(a.mac.cmp(&b.mac)));
        Ok(peers)
    }

    fn read_peer(&self, device: &OwnedObjectPath, path: &OwnedObjectPath) -> Result<Option<Peer>> {
        let proxy = self.proxy(path.as_str(), PEER)?;
        let ies: Vec<u8> = proxy.get_property("WfdIEs")?;
        if !is_display(&ies) {
            return Ok(None);
        }
        Ok(Some(Peer {
            name: proxy.get_property("Name")?,
            mac: proxy.get_property("HwAddress")?,
            device: device.clone(),
            peer: path.clone(),
        }))
    }

    fn connect_inner(
        &self,
        peer: &Peer,
        timeout: Duration,
        stop: &AtomicBool,
    ) -> Result<ActiveConnection> {
        cancelled(stop)?;
        self.ensure_p2p_available(peer)?;
        cancelled(stop)?;
        let settings = connection_settings(&peer.mac);
        let options = HashMap::from([
            ("persist", Value::from("volatile")),
            ("bind-activation", Value::from("dbus-client")),
        ]);
        let (_, path, _): (
            OwnedObjectPath,
            OwnedObjectPath,
            HashMap<String, OwnedValue>,
        ) = self
            .proxy(ROOT, NM)?
            .call(
                "AddAndActivateConnection2",
                &(settings, &peer.device, &peer.peer, options),
            )
            .context("NetworkManager could not create the Wi-Fi Direct connection")?;
        // Construct the guard before waiting: errors and timeouts also release this session.
        let mut active = ActiveConnection {
            bus: self.bus.clone(),
            path: Some(path),
            interface: String::new(),
        };
        active.wait_activated(timeout, stop)?;
        Ok(active)
    }

    pub fn ensure_p2p_available(&self, peer: &Peer) -> Result<()> {
        let device_active: OwnedObjectPath = self
            .proxy(peer.device.as_str(), DEVICE)?
            .get_property("ActiveConnection")?;
        if device_active.as_str() != "/" {
            bail!(
                "Wi-Fi Direct device already has an active connection ({device_active}); stop that session before mirroring."
            );
        }
        let paths: Vec<OwnedObjectPath> =
            self.proxy(ROOT, NM)?.get_property("ActiveConnections")?;
        for path in paths {
            let proxy = self.proxy(path.as_str(), ACTIVE)?;
            let kind: String = proxy.get_property("Type")?;
            let state: u32 = proxy.get_property("State")?;
            if kind == "wifi-p2p" && state != 4 {
                bail!(
                    "An existing Wi-Fi Direct connection ({path}) is still active; stop that session before mirroring."
                );
            }
        }
        Ok(())
    }
}

fn cancelled(stop: &AtomicBool) -> Result<()> {
    if stop.load(Ordering::Acquire) {
        bail!("Operation cancelled");
    }
    Ok(())
}

type Settings = HashMap<&'static str, HashMap<&'static str, Value<'static>>>;

fn connection_settings(mac: &str) -> Settings {
    HashMap::from([
        (
            "connection",
            HashMap::from([
                (
                    "id",
                    Value::from(format!("remote-screen-{}", std::process::id())),
                ),
                ("type", Value::from("wifi-p2p")),
                ("autoconnect", Value::from(false)),
            ]),
        ),
        (
            "wifi-p2p",
            HashMap::from([
                ("peer", Value::from(mac.to_owned())),
                ("wfd-ies", Value::from(WFD_SOURCE_R2.to_vec())),
            ]),
        ),
        (
            "ipv4",
            HashMap::from([
                ("method", Value::from("auto")),
                ("never-default", Value::from(true)),
            ]),
        ),
        (
            "ipv6",
            HashMap::from([
                ("method", Value::from("auto")),
                ("never-default", Value::from(true)),
                ("may-fail", Value::from(true)),
            ]),
        ),
    ])
}

/// Owns only the active connection created by this process. Never disconnects
/// a device or enumerates/deletes other NetworkManager connection profiles.
pub struct ActiveConnection {
    bus: Connection,
    path: Option<OwnedObjectPath>,
    pub interface: String,
}

impl ActiveConnection {
    fn wait_activated(&mut self, timeout: Duration, stop: &AtomicBool) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let path = self.path.as_ref().context("Connection has been closed")?;
        loop {
            cancelled(stop)?;
            // Create fresh proxies to avoid relying on a cached State property.
            let proxy = Proxy::new(&self.bus, NM, path.as_str(), ACTIVE)?;
            let state: u32 = proxy
                .get_property("State")
                .context("Wi-Fi Direct activation disappeared")?;
            if state == 2 {
                let devices: Vec<OwnedObjectPath> = proxy.get_property("Devices")?;
                for device in devices {
                    let device_proxy = Proxy::new(&self.bus, NM, device.as_str(), DEVICE)?;
                    let ip_path: OwnedObjectPath = device_proxy.get_property("Ip4Config")?;
                    if ip_path.as_str() == "/" {
                        continue;
                    }
                    let ip = Proxy::new(
                        &self.bus,
                        NM,
                        ip_path.as_str(),
                        "org.freedesktop.NetworkManager.IP4Config",
                    )?;
                    let addresses: Vec<HashMap<String, OwnedValue>> =
                        ip.get_property("AddressData")?;
                    let has_local = addresses.iter().any(|entry| {
                        entry.get("address").and_then(|v| <&str>::try_from(v).ok())
                            == Some(LOCAL_ADDRESS)
                    });
                    if !has_local && !addresses.is_empty() {
                        let assigned: Vec<_> = addresses
                            .iter()
                            .filter_map(|entry| {
                                entry.get("address").and_then(|v| <&str>::try_from(v).ok())
                            })
                            .collect();
                        bail!(
                            "Wi-Fi Direct DHCP assigned {assigned:?}, but the current firewall permits {LOCAL_ADDRESS}. The firewall was not changed."
                        );
                    }
                    if has_local {
                        self.interface = device_proxy.get_property("IpInterface")?;
                        if self.interface.is_empty() {
                            self.interface = device_proxy.get_property("Interface")?;
                        }
                        return Ok(());
                    }
                }
            }
            if state == 3 || state == 4 {
                bail!("Wi-Fi Direct activation failed (active connection state {state})");
            }
            if Instant::now() >= deadline {
                bail!("Timed out waiting for Wi-Fi Direct activation and {LOCAL_ADDRESS}/24");
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn disconnect(&mut self) -> Result<()> {
        if let Some(path) = self.path.as_ref() {
            Proxy::new(&self.bus, NM, ROOT, NM)?
                .call::<_, _, ()>("DeactivateConnection", &(path,))?;
            self.path = None;
        }
        Ok(())
    }
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        if let Err(error) = self.disconnect() {
            eprintln!("Could not release Wi-Fi Direct session: {error}");
        }
    }
}

pub fn select_tv<'a>(devices: &'a [Peer], target: &str) -> Result<&'a Peer> {
    let target = target.to_lowercase();
    let matches: Vec<_> = devices
        .iter()
        .filter(|p| p.name.to_lowercase().contains(&target) || p.mac.eq_ignore_ascii_case(&target))
        .collect();
    if matches.len() != 1 {
        bail!(
            "Found {} matching Miracast displays; use discover and --tv NAME_OR_MAC.",
            matches.len()
        );
    }
    Ok(matches[0])
}

pub fn is_display(data: &[u8]) -> bool {
    let mut offset = 0;
    while offset + 3 <= data.len() {
        let tag = data[offset];
        let size = u16::from_be_bytes([data[offset + 1], data[offset + 2]]) as usize;
        offset += 3;
        if size > data.len() - offset {
            return false;
        }
        if tag == 0 && size >= 6 {
            return matches!(data[offset + 1] & 3, 1..=3);
        }
        offset += size;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receiver_ie_roles_and_truncation() {
        assert!(!is_display(&WFD_SOURCE_R2));
        for role in 1..=3 {
            let mut ie = WFD_SOURCE_R2;
            ie[4] |= role;
            assert!(is_display(&ie));
            for end in 0..9 {
                assert!(!is_display(&ie[..end]));
            }
        }
        assert!(!is_display(&[0, 0xff, 0xff]));
        assert!(is_display(&[7, 0, 1, 0, 0, 0, 6, 0, 1, 0, 0, 0, 0]));
    }

    #[test]
    fn selection_requires_one_name_or_mac_match() {
        let peer = Peer {
            name: "LG C9".into(),
            mac: "AA:BB:CC:DD:EE:FF".into(),
            device: OwnedObjectPath::try_from("/device").unwrap(),
            peer: OwnedObjectPath::try_from("/peer").unwrap(),
        };
        let peers = vec![peer.clone()];
        assert_eq!(select_tv(&peers, "lg c9").unwrap().mac, peer.mac);
        assert!(select_tv(&peers, "aa:bb:cc:dd:ee:ff").is_ok());
        assert!(select_tv(&peers, "absent").is_err());
        assert!(select_tv(&[peer.clone(), peer], "LG").is_err());
    }

    #[test]
    fn settings_are_r2_source_and_preserve_default_route() {
        let settings = connection_settings("AA:BB:CC:DD:EE:FF");
        assert_eq!(
            settings["wifi-p2p"]["wfd-ies"],
            Value::from(WFD_SOURCE_R2.to_vec())
        );
        assert_eq!(settings["ipv4"]["never-default"], Value::from(true));
        assert_eq!(settings["connection"]["autoconnect"], Value::from(false));
        assert_eq!(settings["ipv4"]["method"], Value::from("auto"));
        assert!(!settings["ipv4"].contains_key("address-data"));
        assert_eq!(settings["ipv6"]["method"], Value::from("auto"));
        assert_eq!(settings["ipv6"]["never-default"], Value::from(true));
        assert_eq!(settings["ipv6"]["may-fail"], Value::from(true));
    }
}
