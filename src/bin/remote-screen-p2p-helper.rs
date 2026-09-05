//! Socket-activated, narrowly scoped privileged Wi-Fi Direct pairing helper.
use futures_lite::{StreamExt, future::race};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use zbus::{
    Connection, Proxy,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};
const SERVICE: &str = "fi.w1.wpa_supplicant1";
const ROOT: &str = "/fi/w1/wpa_supplicant1";
const IFACE: &str = "fi.w1.wpa_supplicant1.Interface";
const P2P: &str = "fi.w1.wpa_supplicant1.Interface.P2PDevice";
const STORE: &str = "/var/lib/remote-screen/pairings";
const WFD: [u8; 14] = [0, 0, 6, 0, 0x90, 0x1c, 0x44, 0, 0xc8, 0x0b, 0, 2, 0, 0];
type Result<T> = std::result::Result<T, &'static str>;
type Dict = HashMap<String, OwnedValue>;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    peer: String,
    timeout_secs: u64,
    #[serde(default)]
    allow_pairing: bool,
    operation: Operation,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Operation {
    Connect,
    Inspect,
}
#[derive(Serialize, Default)]
struct Response {
    event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reused: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<usize>,
}
fn emit(socket: &mut UnixStream, response: Response) -> Result<()> {
    serde_json::to_writer(&mut *socket, &response).map_err(|_| "response write failed")?;
    socket.write_all(b"\n").map_err(|_| "response write failed")
}
fn mac(input: &str) -> Result<String> {
    let b = input.as_bytes();
    if b.len() != 17
        || !(0..17).all(|i| {
            if i % 3 == 2 {
                b[i] == b':'
            } else {
                b[i].is_ascii_hexdigit()
            }
        })
    {
        return Err("invalid peer MAC");
    }
    let value = input.to_ascii_lowercase();
    if value == "00:00:00:00:00:00" || u8::from_str_radix(&value[..2], 16).unwrap() & 1 != 0 {
        return Err("invalid peer MAC");
    }
    Ok(value)
}
fn parse(line: &str) -> Result<Request> {
    let mut r: Request = serde_json::from_str(line).map_err(|_| "invalid request")?;
    r.peer = mac(&r.peer)?;
    if !(1..=600).contains(&r.timeout_secs) {
        return Err("timeout must be 1..600 seconds");
    }
    Ok(r)
}
fn secure_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|_| "cannot create pairing directory")?;
    let m = fs::symlink_metadata(dir).map_err(|_| "cannot inspect pairing directory")?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } {
        return Err("unsafe pairing directory");
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(|_| "cannot protect pairing directory")
}
fn save_at(dir: &Path, peer: &str, props: &HashMap<String, String>) -> Result<()> {
    let peer = mac(peer)?;
    secure_dir(dir)?;
    let target = dir.join(format!("{}.json", peer.replace(':', "")));
    let temp = dir.join(format!(
        ".{}.{}.tmp",
        peer.replace(':', ""),
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)
            .map_err(|_| "cannot create pairing file")?;
        serde_json::to_writer(&mut file, props).map_err(|_| "cannot save pairing")?;
        file.sync_all().map_err(|_| "cannot sync pairing")?;
        fs::rename(&temp, &target).map_err(|_| "cannot publish pairing")?;
        File::open(dir)
            .and_then(|f| f.sync_all())
            .map_err(|_| "cannot sync pairing directory")
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
fn load(peer: &str) -> Result<Option<HashMap<String, String>>> {
    let path = Path::new(STORE).join(format!("{}.json", mac(peer)?.replace(':', "")));
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("cannot open saved pairing"),
    };
    let m = file
        .metadata()
        .map_err(|_| "cannot inspect saved pairing")?;
    if !m.is_file() || m.uid() != 0 || m.permissions().mode() & 0o077 != 0 || m.len() > 65536 {
        return Err("unsafe saved pairing");
    }
    let props: HashMap<String, String> =
        serde_json::from_reader(file.take(65537)).map_err(|_| "invalid saved pairing")?;
    if !matches_peer(&props, peer) {
        return Err("saved pairing belongs to another peer");
    }
    Ok(Some(props))
}
fn matches_peer(props: &HashMap<String, String>, peer: &str) -> bool {
    props.get("bssid").and_then(|s| mac(s).ok()).as_deref() == Some(peer)
        && props.get("mode").is_none_or(|s| s == "0")
}
// PersistentGroup.Properties uses wpa_config_write_string: quoted ASCII or hex.
fn ssid_bytes(value: &str) -> Option<Vec<u8>> {
    let result = if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        inner.as_bytes().to_vec()
    } else {
        if !value.len().is_multiple_of(2) || !value.bytes().all(|v| v.is_ascii_hexdigit()) {
            return None;
        }
        (0..value.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&value[i..i + 2], 16).ok())
            .collect::<Option<Vec<_>>>()?
    };
    (1..=32).contains(&result.len()).then_some(result)
}
/// Get(Properties) returns config-file strings; AddPersistentGroup expects typed
/// D-Bus input and adds its own quotes to strings. Only restore client credentials.
fn restore_properties(
    props: &HashMap<String, String>,
) -> Result<HashMap<&'static str, Value<'static>>> {
    let ssid = props
        .get("ssid")
        .and_then(|s| ssid_bytes(s))
        .ok_or("saved pairing has invalid SSID")?;
    let bssid = mac(props
        .get("bssid")
        .ok_or("saved pairing has no peer address")?)?;
    if props.get("mode").is_some_and(|mode| mode != "0") {
        return Err("saved pairing is not a client group");
    }
    let psk = props.get("psk").ok_or("saved pairing has no key")?;
    let key = if let Some(passphrase) = psk.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        if !(8..=63).contains(&passphrase.len()) || passphrase.as_bytes().contains(&0) {
            return Err("saved pairing has invalid passphrase");
        }
        Value::from(passphrase.to_owned())
    } else {
        if psk.len() != 64 || !psk.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("saved pairing has invalid raw key");
        }
        let bytes = (0..64)
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&psk[i..i + 2], 16)
                    .map_err(|_| "saved pairing has invalid raw key")
            })
            .collect::<Result<Vec<_>>>()?;
        Value::from(bytes)
    };
    let mut values = HashMap::from([
        ("ssid", Value::from(ssid)),
        ("psk", key),
        ("bssid", Value::from(bssid)),
        ("mode", Value::from(0u32)),
        ("disabled", Value::from(2u32)),
    ]);
    // These fields are explicitly in supplicant's dont_quote[] table.
    for field in ["key_mgmt", "proto", "pairwise", "group", "auth_alg"] {
        if let Some(value) = props.get(field) {
            values.insert(field, Value::from(value.clone()));
        }
    }
    // Preserve negotiated protected-management-frame policy when present.
    if let Some(value) = props.get("ieee80211w") {
        let mode = value
            .parse::<u32>()
            .ok()
            .filter(|v| *v <= 3)
            .ok_or("saved pairing has invalid management-frame policy")?;
        // 3 is the internal default sentinel; it is not an explicit config value.
        if mode < 3 {
            values.insert("ieee80211w", Value::from(mode));
        }
    }
    Ok(values)
}
fn same_ssid(a: &HashMap<String, String>, b: &HashMap<String, String>) -> bool {
    match (
        a.get("ssid").and_then(|v| ssid_bytes(v)),
        b.get("ssid").and_then(|v| ssid_bytes(v)),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}
fn group_number(path: &OwnedObjectPath) -> u64 {
    path.as_str()
        .rsplit('/')
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}
fn choose_group(
    groups: &[(OwnedObjectPath, HashMap<String, String>)],
    saved: Option<&HashMap<String, String>>,
) -> Option<usize> {
    (0..groups.len()).max_by_key(|&i| {
        (
            saved.is_some_and(|s| {
                same_ssid(&groups[i].1, s) && groups[i].1.get("bssid") == s.get("bssid")
            }),
            group_number(&groups[i].0),
        )
    })
}
fn verified_ip(info: &Dict) -> bool {
    [
        ("IpAddrGo", [192, 168, 49, 1]),
        ("IpAddr", [192, 168, 49, 10]),
        ("IpAddrMask", [255, 255, 255, 0]),
    ]
    .iter()
    .all(|(key, expected)| {
        info.get(*key)
            .and_then(|v| Vec::<u8>::try_from(v.try_clone().ok()?).ok())
            .as_deref()
            == Some(&expected[..])
    })
}
async fn proxy<'a>(c: &'a Connection, path: &'a str, iface: &'a str) -> Result<Proxy<'a>> {
    zbus::proxy::Builder::new(c)
        .destination(SERVICE)
        .and_then(|b| b.path(path))
        .and_then(|b| b.interface(iface))
        .map_err(|_| "supplicant proxy failed")?
        // Supplicant group lists can change without invalidating a cached property.
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|_| "supplicant proxy failed")
}
async fn matching(
    c: &Connection,
    p: &Proxy<'_>,
    peer: &str,
) -> Result<Vec<(OwnedObjectPath, HashMap<String, String>)>> {
    let paths: Vec<OwnedObjectPath> = p
        .get_property("PersistentGroups")
        .await
        .map_err(|_| "cannot list persistent groups")?;
    let mut result = Vec::new();
    for path in paths {
        let g = proxy(c, path.as_str(), "fi.w1.wpa_supplicant1.PersistentGroup").await?;
        let values: Dict = g
            .get_property("Properties")
            .await
            .map_err(|_| "cannot read persistent group")?;
        let props: HashMap<String, String> = values
            .iter()
            .filter_map(|(k, v)| <&str>::try_from(v).ok().map(|s| (k.clone(), s.to_owned())))
            .collect();
        if matches_peer(&props, peer) {
            result.push((path, props));
        }
    }
    Ok(result)
}
#[derive(Default)]
struct Cleanup {
    parent: Option<String>,
    group: Option<String>,
    wfd: Option<Vec<u8>>,
    initiated: bool,
    peer: Option<String>,
    previous_interfaces: Vec<OwnedObjectPath>,
}
impl Cleanup {
    async fn run(&self, c: &Connection) {
        if let Some(path) = &self.group {
            if let Ok(p) = proxy(c, path, P2P).await {
                let _: zbus::Result<()> = p.call("Disconnect", &()).await;
            }
        } else if self.initiated
            && let Some(path) = &self.parent
            && let Ok(p) = proxy(c, path, P2P).await
        {
            let _: zbus::Result<()> = p.call("Cancel", &()).await;
            // A group can form just before the event future is cancelled. Only
            // remove a newly created interface explicitly linked to our peer.
            if let Some(peer) = &self.peer
                && let Ok(pp) = proxy(c, peer, "fi.w1.wpa_supplicant1.Peer").await
                && let Ok(groups) = pp.get_property::<Vec<OwnedObjectPath>>("Groups").await
                && let Ok(root) = proxy(c, ROOT, SERVICE).await
                && let Ok(interfaces) = root
                    .get_property::<Vec<OwnedObjectPath>>("Interfaces")
                    .await
            {
                for interface in interfaces {
                    if self.previous_interfaces.contains(&interface) {
                        continue;
                    }
                    if let Ok(q) = proxy(c, interface.as_str(), P2P).await
                        && let Ok(group) = q.get_property::<OwnedObjectPath>("Group").await
                        && groups.contains(&group)
                        && let Ok(i) = proxy(c, interface.as_str(), IFACE).await
                        && i.get_property::<String>("Ifname")
                            .await
                            .is_ok_and(|n| n.starts_with("p2p-"))
                    {
                        let _: zbus::Result<()> = q.call("Disconnect", &()).await;
                    }
                }
            }
        }
        if let Some(old) = &self.wfd
            && let Ok(p) = proxy(c, ROOT, SERVICE).await
            && p.get_property::<Vec<u8>>("WFDIEs").await.ok().as_deref() == Some(&WFD[..])
        {
            let _ = p.set_property("WFDIEs", old.clone()).await;
        }
    }
}
async fn perform(
    c: &Connection,
    r: &Request,
    socket: &mut UnixStream,
    cleanup: &mut Cleanup,
    ready: &AtomicBool,
) -> Result<()> {
    let root = proxy(c, ROOT, SERVICE).await?;
    let paths: Vec<OwnedObjectPath> = root
        .get_property("Interfaces")
        .await
        .map_err(|_| "cannot list supplicant interfaces")?;
    let mut selected = None;
    for path in paths {
        let i = proxy(c, path.as_str(), IFACE).await?;
        if i.get_property::<String>("Ifname").await.ok().as_deref() != Some("wlo1") {
            continue;
        }
        let p = proxy(c, path.as_str(), P2P).await?;
        let peers: Vec<OwnedObjectPath> = p
            .get_property("Peers")
            .await
            .map_err(|_| "cannot list P2P peers")?;
        for peer in peers {
            let pp = proxy(c, peer.as_str(), "fi.w1.wpa_supplicant1.Peer").await?;
            let address: Vec<u8> = pp
                .get_property("DeviceAddress")
                .await
                .map_err(|_| "cannot read peer address")?;
            let text = address
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(":");
            if text == r.peer {
                selected = Some((path.clone(), peer));
                break;
            }
        }
    }
    let (parent, peer) = selected.ok_or("target peer not discovered on wlo1")?;
    let p = proxy(c, parent.as_str(), P2P).await?;
    let mut groups = matching(c, &p, &r.peer).await?;
    if matches!(r.operation, Operation::Inspect) {
        return emit(
            socket,
            Response {
                event: "inspection",
                count: Some(
                    groups.len() + usize::from(groups.is_empty() && load(&r.peer)?.is_some()),
                ),
                ..Default::default()
            },
        );
    }
    let interfaces: Vec<OwnedObjectPath> = root
        .get_property("Interfaces")
        .await
        .map_err(|_| "cannot check active groups")?;
    cleanup.previous_interfaces = interfaces.clone();
    cleanup.peer = Some(peer.to_string());
    for path in interfaces {
        let q = proxy(c, path.as_str(), P2P).await?;
        if let Ok(group) = q.get_property::<OwnedObjectPath>("Group").await
            && group.as_str() != "/"
        {
            return Err("an active P2P group already exists");
        }
    }
    let saved_pairing = load(&r.peer)?;
    if groups.is_empty()
        && let Some(props) = saved_pairing.clone()
    {
        let values = restore_properties(&props)?;
        let path: OwnedObjectPath = p
            .call("AddPersistentGroup", &(values,))
            .await
            .map_err(|_| "cannot restore saved persistent group")?;
        emit(
            socket,
            Response {
                event: "status",
                message: Some("restoring saved pairing from disk"),
                reused: Some(true),
                ..Default::default()
            },
        )?;
        groups.push((path, props));
    }
    if groups.is_empty() && !r.allow_pairing {
        return Err("no saved pairing; automatic pairing is disabled");
    }
    let reused = !groups.is_empty();
    let mut signals = p
        .receive_signal("GroupStarted")
        .await
        .map_err(|_| "cannot subscribe to group events")?;
    cleanup.parent = Some(parent.to_string());
    cleanup.wfd = Some(
        root.get_property("WFDIEs")
            .await
            .map_err(|_| "cannot read WFD advertisement")?,
    );
    root.set_property("WFDIEs", WFD.to_vec())
        .await
        .map_err(|_| "cannot advertise display source")?;
    let mut args: HashMap<&str, Value<'_>> = HashMap::new();
    args.insert("peer", Value::from(peer.as_ref()));
    cleanup.initiated = true;
    if let Some(index) = choose_group(&groups, saved_pairing.as_ref()) {
        let (group, _) = &groups[index];
        emit(
            socket,
            Response {
                event: "status",
                message: Some("reinviting saved persistent group"),
                reused: Some(true),
                ..Default::default()
            },
        )?;
        args.insert("persistent_group_object", Value::from(group.as_ref()));
        let _: () = p
            .call("Invite", &(args,))
            .await
            .map_err(|_| "persistent invitation failed; pairing was retained")?;
    } else {
        emit(
            socket,
            Response {
                event: "status",
                message: Some("first persistent pairing; confirm Wi-Fi Direct on TV if requested"),
                reused: Some(false),
                ..Default::default()
            },
        )?;
        args.insert("persistent", Value::from(true));
        args.insert("wps_method", Value::from("pbc"));
        args.insert("go_intent", Value::from(7i32));
        let _: String = p
            .call("Connect", &(args,))
            .await
            .map_err(|_| "persistent pairing request failed")?;
    }
    let message = signals.next().await.ok_or("group event stream ended")?;
    let info: Dict = message
        .body()
        .deserialize()
        .map_err(|_| "invalid group event")?;
    let interface = info
        .get("interface_object")
        .and_then(|v| <&zbus::zvariant::ObjectPath<'_>>::try_from(v).ok())
        .ok_or("group event has no interface")?
        .to_string();
    cleanup.group = Some(interface.clone());
    if info.get("role").and_then(|v| <&str>::try_from(v).ok()) != Some("client") {
        return Err("TV did not become group owner");
    }
    let ip = proxy(c, &interface, IFACE).await?;
    let name: String = ip
        .get_property("Ifname")
        .await
        .map_err(|_| "cannot read group interface")?;
    let group = info
        .get("group_object")
        .and_then(|v| <&zbus::zvariant::ObjectPath<'_>>::try_from(v).ok())
        .ok_or("group object missing")?;
    let g = proxy(c, group.as_str(), "fi.w1.wpa_supplicant1.Group").await?;
    let bssid: Vec<u8> = g
        .get_property("BSSID")
        .await
        .map_err(|_| "cannot validate group owner")?;
    // Device and interface MAC may differ; the peer's Groups property binds this group to the target device.
    let pp = proxy(c, peer.as_str(), "fi.w1.wpa_supplicant1.Peer").await?;
    let peer_groups: Vec<OwnedObjectPath> = pp
        .get_property("Groups")
        .await
        .map_err(|_| "cannot validate peer group membership")?;
    if bssid.len() != 6 || !peer_groups.iter().any(|v| v.as_str() == group.as_str()) {
        return Err("connected group does not belong to target peer");
    }
    let actual_ssid: Vec<u8> = g
        .get_property("SSID")
        .await
        .map_err(|_| "cannot read connected group SSID")?;
    let saved = matching(c, &p, &r.peer).await?;
    if let Some((_, props)) = saved
        .iter()
        .filter(|(_, props)| {
            props.get("ssid").and_then(|s| ssid_bytes(s)).as_deref() == Some(actual_ssid.as_slice())
        })
        .max_by_key(|(path, _)| group_number(path))
    {
        // wpas_p2p_store_persistent_group sets internal export_keys=1 for this group.
        // export_keys is not a writable network configuration property.
        if props.contains_key("psk") {
            save_at(Path::new(STORE), &r.peer, props)?;
        } else if !saved_pairing
            .as_ref()
            .is_some_and(|old| same_ssid(old, props))
        {
            return Err("target pairing has no exportable credentials");
        }
    } else {
        return Err("target persistent credentials were not retained");
    }
    if !verified_ip(&info) {
        return Err("supplicant did not confirm expected local address, mask and GO address");
    }
    configure(&name)?;
    emit(
        socket,
        Response {
            event: "ready",
            interface: Some(name),
            reused: Some(reused),
            ..Default::default()
        },
    )?;
    ready.store(true, Ordering::Release);
    std::future::pending::<Result<()>>().await
}
fn configure(name: &str) -> Result<()> {
    if !name.starts_with("p2p-")
        || name.len() >= libc::IFNAMSIZ
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err("invalid P2P interface name");
    }
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err("cannot open interface configuration socket");
    }
    let socket = unsafe { File::from_raw_fd(fd) };
    for (request, octets) in [
        (libc::SIOCSIFADDR, [192, 168, 49, 10]),
        (libc::SIOCSIFNETMASK, [255, 255, 255, 0]),
    ] {
        let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
        for (dst, src) in ifr.ifr_name.iter_mut().zip(name.bytes()) {
            *dst = src as libc::c_char;
        }
        let mut address: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        address.sin_family = libc::AF_INET as _;
        address.sin_addr.s_addr = u32::from_ne_bytes(octets);
        ifr.ifr_ifru.ifru_addr =
            unsafe { std::mem::transmute::<libc::sockaddr_in, libc::sockaddr>(address) };
        if unsafe { libc::ioctl(socket.as_raw_fd(), request, &ifr) } < 0 {
            return Err("cannot configure P2P address");
        }
    }
    Ok(())
}
fn run(socket: &mut UnixStream) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("helper requires root socket activation");
    }
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|_| "cannot set request timeout")?;
    let mut reader = BufReader::new(socket.try_clone().map_err(|_| "cannot read socket")?);
    let mut line = String::new();
    reader
        .by_ref()
        .take(1025)
        .read_line(&mut line)
        .map_err(|_| "cannot read request")?;
    if line.len() > 1024 || !line.ends_with('\n') {
        return Err("request too large or incomplete");
    }
    let request = parse(&line)?;
    socket
        .set_read_timeout(None)
        .map_err(|_| "cannot reset socket timeout")?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open("/run/remote-screen-p2p/session.lock")
        .map_err(|_| "cannot open helper lock")?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("another P2P helper session is active");
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let signal_cancel = cancel.clone();
    ctrlc::set_handler(move || signal_cancel.store(true, Ordering::Release))
        .map_err(|_| "cannot install cancellation handler")?;
    let flag = cancel.clone();
    std::thread::spawn(move || {
        let mut b = [0u8; 1];
        let _ = reader.read(&mut b);
        flag.store(true, Ordering::Release);
    });
    futures_lite::future::block_on(async {
        let c = zbus::connection::Builder::system()
            .map_err(|_| "cannot open system bus")?
            .method_timeout(Duration::from_secs(5))
            .build()
            .await
            .map_err(|_| "cannot connect to system bus")?;
        let mut cleanup = Cleanup::default();
        let ready = AtomicBool::new(false);
        let work = perform(&c, &request, socket, &mut cleanup, &ready);
        let timeout = async {
            async_io::Timer::after(Duration::from_secs(request.timeout_secs)).await;
            if ready.load(Ordering::Acquire) {
                std::future::pending::<Result<()>>().await
            } else {
                Err("connection deadline exceeded; saved pairing was retained")
            }
        };
        let cancellation = async {
            while !cancel.load(Ordering::Acquire) {
                async_io::Timer::after(Duration::from_millis(100)).await;
            }
            Ok(())
        };
        let result = race(work, race(timeout, cancellation)).await;
        cleanup.run(&c).await;
        result
    })
}
fn main() {
    let mut socket = unsafe { UnixStream::from_raw_fd(0) };
    if let Err(message) = run(&mut socket) {
        let _ = emit(
            &mut socket,
            Response {
                event: "error",
                message: Some(message),
                ..Default::default()
            },
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_paths_and_multicast() {
        for p in [
            "../../root",
            "ff:ff:ff:ff:ff:ff",
            "00:00:00:00:00:00",
            "a0:00:00:00:00:0z",
        ] {
            assert!(mac(p).is_err());
        }
        assert_eq!(mac("AA:BB:CC:DD:EE:00").unwrap(), "aa:bb:cc:dd:ee:00");
    }
    #[test]
    fn request_is_bounded_and_errors_are_secret_free() {
        assert!(
            parse(r#"{"peer":"aa:bb:cc:dd:ee:00","timeout_secs":600,"operation":"connect"}"#)
                .is_ok()
        );
        assert!(
            parse(r#"{"peer":"aa:bb:cc:dd:ee:00","timeout_secs":601,"operation":"connect"}"#)
                .is_err()
        );
        assert_eq!(parse(r#"{"psk":"secret"}"#).err(), Some("invalid request"));
    }
    #[test]
    fn chooses_saved_ssid_then_newest_numeric_group() {
        let props = |ssid: &str| {
            HashMap::from([
                ("bssid".to_owned(), "aa:bb:cc:dd:ee:00".to_owned()),
                ("ssid".to_owned(), ssid.to_owned()),
            ])
        };
        let groups = vec![
            (
                OwnedObjectPath::try_from("/groups/2").unwrap(),
                props("\"old\""),
            ),
            (
                OwnedObjectPath::try_from("/groups/10").unwrap(),
                props("\"new\""),
            ),
        ];
        assert_eq!(choose_group(&groups, None), Some(1));
        assert_eq!(choose_group(&groups, Some(&props("6f6c64"))), Some(0));
        assert_eq!(ssid_bytes("6e6577"), Some(b"new".to_vec()));
        assert_eq!(ssid_bytes("x"), None);
    }
    #[test]
    fn requires_exact_negotiated_address_and_mask() {
        let mut info = Dict::new();
        for (key, bytes) in [
            ("IpAddrGo", vec![192u8, 168, 49, 1]),
            ("IpAddr", vec![192, 168, 49, 10]),
            ("IpAddrMask", vec![255, 255, 255, 0]),
        ] {
            info.insert(key.to_owned(), Value::from(bytes).try_to_owned().unwrap());
        }
        assert!(verified_ip(&info));
        info.remove("IpAddr");
        assert!(!verified_ip(&info));
        info.insert(
            "IpAddr".to_owned(),
            Value::from(vec![192u8, 168, 49, 11])
                .try_to_owned()
                .unwrap(),
        );
        assert!(!verified_ip(&info));
    }
    #[test]
    fn restore_uses_dbus_types_not_config_file_strings() {
        let props = HashMap::from([
            ("ssid".to_owned(), "\"DIRECT-test\"".to_owned()),
            ("psk".to_owned(), "\"test-passphrase\"".to_owned()),
            ("bssid".to_owned(), "aa:bb:cc:dd:ee:00".to_owned()),
            ("mode".to_owned(), "0".to_owned()),
            ("disabled".to_owned(), "2".to_owned()),
            ("key_mgmt".to_owned(), "WPA-PSK".to_owned()),
            ("ieee80211w".to_owned(), "1".to_owned()),
            ("unrelated_default".to_owned(), "0".to_owned()),
        ]);
        let values = restore_properties(&props).unwrap();
        assert_eq!(values["ssid"], Value::from(b"DIRECT-test".to_vec()));
        assert_eq!(values["psk"], Value::from("test-passphrase"));
        assert_eq!(values["mode"], Value::from(0u32));
        assert_eq!(values["disabled"], Value::from(2u32));
        assert_eq!(values["ieee80211w"], Value::from(1u32));
        assert_eq!(values["key_mgmt"], Value::from("WPA-PSK"));
        assert!(!values.contains_key("unrelated_default"));
        let mut defaults = props.clone();
        defaults.insert("ieee80211w".to_owned(), "3".to_owned());
        assert!(
            !restore_properties(&defaults)
                .unwrap()
                .contains_key("ieee80211w")
        );
        let mut raw = props.clone();
        raw.insert("ssid".to_owned(), "4449524543542d74657374".to_owned());
        raw.insert("psk".to_owned(), "ab".repeat(32));
        let restored = restore_properties(&raw).unwrap();
        assert_eq!(restored["ssid"], values["ssid"]);
        assert_eq!(restored["psk"], Value::from(vec![0xabu8; 32]));
        raw.insert("psk".to_owned(), "bad".to_owned());
        assert_eq!(
            restore_properties(&raw).err(),
            Some("saved pairing has invalid raw key")
        );
    }
    #[test]
    fn saves_private_credentials() {
        let dir = std::env::temp_dir().join(format!("p2p-helper-test-{}", std::process::id()));
        let props = HashMap::from([
            ("bssid".to_owned(), "aa:bb:cc:dd:ee:00".to_owned()),
            ("psk".to_owned(), "test-only".to_owned()),
        ]);
        save_at(&dir, "aa:bb:cc:dd:ee:00", &props).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(dir.join("aabbccddee00.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
