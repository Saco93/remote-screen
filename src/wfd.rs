//! Native Wi-Fi Display RTSP source control. Media stays outside this module.
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

pub const LOCAL_RTP_PORT: u16 = 50000;
pub const LOCAL_RTCP_PORT: u16 = 50001;
const MAX_MESSAGE: usize = 128 * 1024;
const PUBLIC: &str =
    "org.wfa.wfd1.0, OPTIONS, GET_PARAMETER, SET_PARAMETER, SETUP, PLAY, PAUSE, TEARDOWN";

/// The TV has not opened its RTSP connection yet; a P2P reconnect may recover it.
#[derive(Debug)]
struct InitialConnectionTimeout;

impl std::fmt::Display for InitialConnectionTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Timed out waiting for TV RTSP connection")
    }
}

impl std::error::Error for InitialConnectionTimeout {}

/// Only retry the initial accept timeout, never cancellation or an RTSP failure.
pub fn is_initial_connection_timeout(error: &anyhow::Error) -> bool {
    error.is::<InitialConnectionTimeout>()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Profile {
    #[default]
    Hevc,
    Baseline,
    Rhp2,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub profile: Profile,
    pub audio: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub timeout: Duration,
    pub probe_codecs: bool,
    pub expected_peer: Option<IpAddr>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            profile: Profile::Hevc,
            audio: true,
            width: 1920,
            height: 1080,
            fps: 60,
            timeout: Duration::from_secs(120),
            probe_codecs: false,
            expected_peer: Some(IpAddr::V4(Ipv4Addr::new(192, 168, 49, 1))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Negotiated {
    pub target: SocketAddr,
    pub rtcp_target: Option<SocketAddr>,
    pub idr_request_capability: bool,
    pub raw_capabilities: String,
    pub probe_only: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Play,
    Pause,
    Idr,
    Teardown,
    Tick,
}

#[derive(Debug, Clone)]
struct Message {
    start: String,
    headers: BTreeMap<String, String>,
    body: String,
}
impl Message {
    fn header(&self, key: &str) -> Option<&str> {
        self.headers.get(key).map(String::as_str)
    }
    fn cseq(&self) -> Result<u32> {
        self.header("cseq")
            .context("RTSP message lacks CSeq")?
            .parse()
            .context("Invalid RTSP CSeq")
    }
    fn status(&self) -> Option<u16> {
        self.start
            .strip_prefix("RTSP/1.0 ")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }
    fn method(&self) -> &str {
        self.start.split_whitespace().next().unwrap_or("")
    }
}

#[derive(Default)]
struct Decoder {
    bytes: Vec<u8>,
}
impl Decoder {
    fn next(&mut self) -> Result<Option<Message>> {
        let Some(end) = self.bytes.windows(4).position(|v| v == b"\r\n\r\n") else {
            ensure!(
                self.bytes.len() <= MAX_MESSAGE,
                "RTSP headers exceed size limit"
            );
            return Ok(None);
        };
        let header =
            std::str::from_utf8(&self.bytes[..end]).context("Invalid RTSP header encoding")?;
        let mut lines = header.split("\r\n");
        let start = lines.next().context("Missing RTSP start line")?.to_owned();
        ensure!(
            start.starts_with("RTSP/1.0 ") || start.ends_with(" RTSP/1.0"),
            "Invalid RTSP start line: {start}"
        );
        let mut headers = BTreeMap::new();
        for line in lines {
            let (key, value) = line.split_once(':').context("Malformed RTSP header")?;
            let key = key.trim().to_ascii_lowercase();
            ensure!(!headers.contains_key(&key), "Duplicate RTSP header: {key}");
            headers.insert(key, value.trim().to_owned());
        }
        let length: usize = headers
            .get("content-length")
            .map(|v| v.parse())
            .transpose()
            .context("Invalid RTSP Content-Length")?
            .unwrap_or(0);
        let total = (end + 4)
            .checked_add(length)
            .context("RTSP length overflow")?;
        ensure!(total <= MAX_MESSAGE, "RTSP message exceeds size limit");
        if self.bytes.len() < total {
            return Ok(None);
        }
        let body = std::str::from_utf8(&self.bytes[end + 4..total])
            .context("Invalid RTSP parameter encoding")?
            .to_owned();
        self.bytes.drain(..total);
        Ok(Some(Message {
            start,
            headers,
            body,
        }))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    Options,
    Capabilities,
    Parameters,
    Trigger,
    Keepalive,
    Teardown,
}

pub struct Session {
    stream: TcpStream,
    decoder: Decoder,
    pending: BTreeMap<u32, Pending>,
    next_cseq: u32,
    presentation: String,
    session_id: String,
    negotiated: Negotiated,
    config: Config,
    options_reply: bool,
    sink_options: bool,
    capabilities_sent: bool,
    parameters_accepted: bool,
    setup_done: bool,
    last_received: Instant,
    last_keepalive: Instant,
}

impl Session {
    /// Accepts the receiver connection, negotiates codecs and responds to SETUP.
    /// The caller must start media from the Play callback passed to `run`.
    pub fn negotiate(listener: &TcpListener, config: &Config, stop: &AtomicBool) -> Result<Self> {
        video_descriptor(config)?;
        listener.set_nonblocking(true)?;
        let deadline = Instant::now() + config.timeout;
        let (stream, peer) = loop {
            ensure!(!stop.load(Ordering::Relaxed), "Mirroring cancelled");
            if Instant::now() >= deadline {
                return Err(InitialConnectionTimeout.into());
            }
            match listener.accept() {
                Ok((_stream, peer)) if config.expected_peer.is_some_and(|ip| ip != peer.ip()) => {
                    crate::status(&format!(
                        "Ignoring RTSP connection from unexpected peer {peer}"
                    ));
                }
                Ok(pair) => break pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100))
                }
                Err(e) => return Err(e.into()),
            }
        };
        stream.set_read_timeout(Some(Duration::from_millis(250)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream.set_nodelay(true)?;
        let presentation = format!("rtsp://{}/wfd1.0/streamid=0", stream.local_addr()?);
        let now = Instant::now();
        let mut session = Self {
            stream,
            decoder: Decoder::default(),
            pending: BTreeMap::new(),
            next_cseq: 1,
            presentation,
            session_id: format!("{:08x}", std::process::id()),
            negotiated: Negotiated {
                target: SocketAddr::new(peer.ip(), 0),
                rtcp_target: None,
                idr_request_capability: false,
                raw_capabilities: String::new(),
                probe_only: config.probe_codecs,
            },
            config: config.clone(),
            options_reply: false,
            sink_options: false,
            capabilities_sent: false,
            parameters_accepted: false,
            setup_done: false,
            last_received: now,
            last_keepalive: now,
        };
        session.request(
            "OPTIONS",
            "*",
            "",
            &[("Require", "org.wfa.wfd1.0")],
            Pending::Options,
        )?;
        while !session.setup_done {
            ensure!(!stop.load(Ordering::Relaxed), "Mirroring cancelled");
            ensure!(
                Instant::now() < deadline,
                "Timed out negotiating WFD with TV"
            );
            if let Some(message) = session.receive()? {
                session.handle(message, &mut |_| Ok(()))?;
                if config.probe_codecs && !session.negotiated.raw_capabilities.is_empty() {
                    break;
                }
            }
        }
        Ok(session)
    }

    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }

    pub fn run(
        &mut self,
        stop: &AtomicBool,
        mut on_event: impl FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        if self.negotiated.probe_only {
            return Ok(());
        }
        loop {
            if stop.load(Ordering::Relaxed) {
                let _ = self.request(
                    "SET_PARAMETER",
                    "rtsp://localhost/wfd1.0",
                    "wfd_trigger_method: TEARDOWN\r\n",
                    &[],
                    Pending::Teardown,
                );
                on_event(Event::Teardown)?;
                return Ok(());
            }
            if self.last_keepalive.elapsed() >= Duration::from_secs(15) {
                let session_id = self.session_id.clone();
                self.request(
                    "GET_PARAMETER",
                    "rtsp://localhost/wfd1.0",
                    "",
                    &[("Session", &session_id)],
                    Pending::Keepalive,
                )?;
                self.last_keepalive = Instant::now();
            }
            ensure!(
                self.last_received.elapsed() < Duration::from_secs(45),
                "TV RTSP keepalive timed out"
            );
            if let Some(message) = self.receive()?
                && self.handle(message, &mut on_event)?
            {
                return Ok(());
            }
            on_event(Event::Tick)?;
        }
    }

    fn receive(&mut self) -> Result<Option<Message>> {
        if let Some(message) = self.decoder.next()? {
            self.last_received = Instant::now();
            return Ok(Some(message));
        }
        let mut bytes = [0u8; 8192];
        match self.stream.read(&mut bytes) {
            Ok(0) => bail!("TV closed RTSP connection"),
            Ok(count) => self.decoder.bytes.extend_from_slice(&bytes[..count]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
        let message = self.decoder.next()?;
        if message.is_some() {
            self.last_received = Instant::now();
        }
        Ok(message)
    }

    fn request(
        &mut self,
        method: &str,
        uri: &str,
        body: &str,
        headers: &[(&str, &str)],
        pending: Pending,
    ) -> Result<()> {
        let sequence = self.next_cseq;
        self.next_cseq += 1;
        let mut text = format!("{method} {uri} RTSP/1.0\r\nCSeq: {sequence}\r\n");
        for (name, value) in headers {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        if !body.is_empty() {
            text.push_str("Content-Type: text/parameters\r\n");
        }
        text.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        crate::status(&format!("RTSP -> {method} CSeq={sequence} ({pending:?})"));
        self.stream.write_all(text.as_bytes())?;
        self.pending.insert(sequence, pending);
        Ok(())
    }

    fn reply(
        &mut self,
        request: &Message,
        status: u16,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<()> {
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            454 => "Session Not Found",
            455 => "Method Not Valid in This State",
            461 => "Unsupported Transport",
            _ => "Not Implemented",
        };
        let mut text = format!(
            "RTSP/1.0 {status} {reason}\r\nCSeq: {}\r\n",
            request.cseq()?
        );
        for (key, value) in headers {
            text.push_str(&format!("{key}: {value}\r\n"));
        }
        if !body.is_empty() {
            text.push_str("Content-Type: text/parameters\r\n");
        }
        text.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        self.stream.write_all(text.as_bytes())?;
        Ok(())
    }

    fn query_capabilities(&mut self) -> Result<()> {
        if self.options_reply && self.sink_options && !self.capabilities_sent {
            self.capabilities_sent = true;
            let body = if self.config.probe_codecs {
                "wfd_client_rtp_ports\r\nwfd_audio_codecs\r\nwfd2_audio_codecs\r\nwfd_video_formats\r\nwfd2_video_formats\r\nwfdx_video_formats\r\n"
            } else if self.config.profile == Profile::Baseline {
                "wfd_client_rtp_ports\r\nwfd_audio_codecs\r\nwfd_video_formats\r\nwfd_idr_request_capability\r\n"
            } else {
                "wfd_client_rtp_ports\r\nwfd_audio_codecs\r\nwfd2_audio_codecs\r\nwfd2_video_formats\r\nwfd_idr_request_capability\r\n"
            };
            self.request(
                "GET_PARAMETER",
                "rtsp://localhost/wfd1.0",
                body,
                &[],
                Pending::Capabilities,
            )?;
        }
        Ok(())
    }

    fn handle(
        &mut self,
        message: Message,
        callback: &mut impl FnMut(Event) -> Result<()>,
    ) -> Result<bool> {
        crate::status(&format!(
            "RTSP <- {} CSeq={}",
            message.start,
            message.cseq()?
        ));
        if let Some(status) = message.status() {
            let kind = self
                .pending
                .remove(&message.cseq()?)
                .context("Unmatched RTSP response CSeq")?;
            check_response(kind, status)?;
            match kind {
                Pending::Options => {
                    self.options_reply = true;
                    self.query_capabilities()?;
                }
                Pending::Capabilities => {
                    crate::status(&format!("TV capabilities:\n{}", message.body));
                    self.negotiated.raw_capabilities = message.body;
                    let params = parameters(&self.negotiated.raw_capabilities);
                    self.negotiated.idr_request_capability = params
                        .get("wfd_idr_request_capability")
                        .is_some_and(|value| *value == "1");
                    if self.config.probe_codecs {
                        return Ok(false);
                    }
                    let (rtp, rtcp) = parse_ports(
                        params
                            .get("wfd_client_rtp_ports")
                            .context("TV did not provide RTP ports")?,
                    )?;
                    self.negotiated.target.set_port(rtp);
                    self.negotiated.rtcp_target =
                        rtcp.map(|port| SocketAddr::new(self.negotiated.target.ip(), port));
                    if self.config.audio {
                        let audio = params
                            .get("wfd2_audio_codecs")
                            .or_else(|| params.get("wfd_audio_codecs"))
                            .context("TV did not advertise audio")?;
                        ensure!(
                            supports_aac(audio),
                            "TV did not advertise AAC stereo 48 kHz"
                        );
                    }
                    if self.config.profile == Profile::Rhp2 {
                        ensure!(
                            supports_rhp2(&self.negotiated.raw_capabilities),
                            "TV did not advertise RHP2 1080p60"
                        );
                    }
                    let (key, descriptor) = video_descriptor(&self.config)?;
                    let audio_key = if self.config.profile == Profile::Baseline {
                        "wfd_audio_codecs"
                    } else {
                        "wfd2_audio_codecs"
                    };
                    let audio = if self.config.audio {
                        "AAC 00000001 00"
                    } else {
                        "none"
                    };
                    let body = format!(
                        "{key}: {descriptor}\r\n{audio_key}: {audio}\r\nwfd_presentation_URL: {} none\r\nwfd_client_rtp_ports: RTP/AVP/UDP;unicast {rtp} {} mode=play\r\n",
                        self.presentation,
                        rtcp.unwrap_or(0)
                    );
                    crate::status(&format!("Selected WFD parameters:\n{body}"));
                    self.request(
                        "SET_PARAMETER",
                        "rtsp://localhost/wfd1.0",
                        &body,
                        &[],
                        Pending::Parameters,
                    )?;
                }
                Pending::Parameters => {
                    self.parameters_accepted = true;
                    self.request(
                        "SET_PARAMETER",
                        "rtsp://localhost/wfd1.0",
                        "wfd_trigger_method: SETUP\r\n",
                        &[],
                        Pending::Trigger,
                    )?;
                }
                Pending::Trigger | Pending::Keepalive | Pending::Teardown => (),
            }
            return Ok(false);
        }
        match message.method() {
            "OPTIONS" => {
                self.reply(&message, 200, &[("Public", PUBLIC)], "")?;
                self.sink_options = true;
                self.query_capabilities()?;
            }
            "SETUP" => {
                if !self.parameters_accepted {
                    self.reply(&message, 455, &[], "")?;
                    return Ok(false);
                }
                let transport = message.header("transport").unwrap_or("");
                let ports = parse_transport(transport);
                let Ok((rtp, rtcp)) = ports else {
                    self.reply(&message, 461, &[], "")?;
                    return Ok(false);
                };
                self.negotiated.target.set_port(rtp);
                self.negotiated.rtcp_target =
                    rtcp.map(|p| SocketAddr::new(self.negotiated.target.ip(), p));
                let client_ports = rtcp
                    .map(|p| format!("{rtp}-{p}"))
                    .unwrap_or_else(|| rtp.to_string());
                let transport = format!(
                    "RTP/AVP/UDP;unicast;client_port={client_ports};server_port={LOCAL_RTP_PORT}-{LOCAL_RTCP_PORT}"
                );
                let session_id = format!("{};timeout=60", self.session_id);
                self.reply(
                    &message,
                    200,
                    &[("Transport", &transport), ("Session", &session_id)],
                    "",
                )?;
                self.setup_done = true;
            }
            "PLAY" | "PAUSE" | "TEARDOWN" => {
                if !self.setup_done {
                    self.reply(&message, 455, &[], "")?;
                    return Ok(false);
                }
                if let Some(id) = message.header("session")
                    && id.split(';').next() != Some(self.session_id.as_str())
                {
                    self.reply(&message, 454, &[], "")?;
                    return Ok(false);
                }
                let event = match message.method() {
                    "PLAY" => Event::Play,
                    "PAUSE" => Event::Pause,
                    _ => Event::Teardown,
                };
                callback(event)?;
                let id = self.session_id.clone();
                self.reply(&message, 200, &[("Session", &id)], "")?;
                return Ok(event == Event::Teardown);
            }
            "GET_PARAMETER" => {
                let id = self.session_id.clone();
                self.reply(&message, 200, &[("Session", &id)], "")?;
            }
            "SET_PARAMETER" => {
                let params = parameters(&message.body);
                if params.contains_key("wfd_idr_request")
                    || message.body.trim() == "wfd_idr_request"
                {
                    callback(Event::Idr)?;
                }
                self.reply(&message, 200, &[], "")?;
                if params
                    .get("wfd_trigger_method")
                    .is_some_and(|s| *s == "TEARDOWN")
                {
                    callback(Event::Teardown)?;
                    return Ok(true);
                }
            }
            _ => self.reply(&message, 501, &[], "")?,
        }
        Ok(false)
    }
}

fn check_response(kind: Pending, status: u16) -> Result<()> {
    ensure!(status == 200, "TV rejected {kind:?}: RTSP {status}");
    Ok(())
}

fn parameters(body: &str) -> BTreeMap<&str, &str> {
    body.lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect()
}

fn parse_ports(value: &str) -> Result<(u16, Option<u16>)> {
    let fields: Vec<_> = value.split_whitespace().collect();
    ensure!(
        fields.len() >= 3 && fields[0] == "RTP/AVP/UDP;unicast",
        "Unsupported WFD RTP transport: {value}"
    );
    let rtp: u16 = fields[1].parse()?;
    let rtcp: u16 = fields[2].parse()?;
    ensure!(rtp != 0, "TV returned zero RTP port");
    Ok((rtp, Some(normalize_rtcp(rtp, Some(rtcp))?)))
}

fn normalize_rtcp(rtp: u16, rtcp: Option<u16>) -> Result<u16> {
    match rtcp {
        Some(port) if port != 0 && port != rtp => Ok(port),
        _ => {
            let port = rtp
                .checked_add(1)
                .context("Cannot derive RTCP port from RTP port 65535")?;
            crate::status(&format!(
                "TV omitted a distinct RTCP port; using {port} as in the verified LG C9 session"
            ));
            Ok(port)
        }
    }
}

fn parse_transport(value: &str) -> Result<(u16, Option<u16>)> {
    let fields: Vec<_> = value.split(';').map(str::trim).collect();
    ensure!(
        matches!(fields.first(), Some(&"RTP/AVP") | Some(&"RTP/AVP/UDP"))
            && fields.contains(&"unicast"),
        "Only UDP unicast is supported"
    );
    let ports = fields
        .iter()
        .find_map(|f| f.strip_prefix("client_port="))
        .context("SETUP lacks client_port")?;
    let (rtp, rtcp) = ports
        .split_once('-')
        .map(|(a, b)| (a, Some(b)))
        .unwrap_or((ports, None));
    let rtp: u16 = rtp.parse()?;
    let rtcp: Option<u16> = rtcp.map(str::parse).transpose()?;
    ensure!(rtp > 0, "Invalid RTP port");
    Ok((rtp, Some(normalize_rtcp(rtp, rtcp)?)))
}

fn supports_aac(value: &str) -> bool {
    value.split(',').any(|entry| {
        let fields: Vec<_> = entry.split_whitespace().collect();
        fields.first() == Some(&"AAC")
            && fields
                .get(1)
                .and_then(|s| u32::from_str_radix(s, 16).ok())
                .is_some_and(|m| m & 1 != 0)
    })
}

fn supports_rhp2(body: &str) -> bool {
    let params = parameters(body);
    let Some(value) = params.get("wfd2_video_formats") else {
        return false;
    };
    value.split(',').enumerate().any(|(index, entry)| {
        let fields: Vec<_> = entry.split_whitespace().collect();
        let offset = usize::from(index == 0);
        if fields.len() < offset + 10 || fields[offset] != "01" || fields[offset + 1] != "04" {
            return false;
        }
        let level = u16::from_str_radix(fields[offset + 2], 16).unwrap_or(0);
        let cea = u64::from_str_radix(fields[offset + 3], 16).unwrap_or(0);
        fields[offset + 2].len() == 4
            && fields[offset + 3].len() == 12
            && (0x10..=0x80).contains(&level)
            && level.is_power_of_two()
            && cea & (1 << 8) != 0
    })
}

fn video_descriptor(config: &Config) -> Result<(&'static str, String)> {
    let bit = match (config.width, config.height, config.fps) {
        (1920, 1080, 60) => 8,
        (1920, 1080, 30) => 7,
        (1280, 720, 60) => 6,
        _ => bail!(
            "Unsupported WFD mode: {}x{}@{}",
            config.width,
            config.height,
            config.fps
        ),
    };
    if config.profile == Profile::Baseline {
        return Ok((
            "wfd_video_formats",
            format!(
                "00 00 01 10 {:08X} 00000000 00000000 00 0000 0000 00 none none",
                1u32 << bit
            ),
        ));
    }
    ensure!(bit == 8, "HEVC and RHP2 require 1080p60");
    let codec = if config.profile == Profile::Hevc {
        "02 01 0004"
    } else {
        "01 04 0010"
    };
    Ok((
        "wfd2_video_formats",
        format!("00 {codec} 000000000100 000000000000 000000000000 00 0000 0000 00 00"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decoder_handles_every_fragment_boundary_and_coalesced_messages() {
        let wire=b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nContent-Length: 5\r\n\r\nhelloOPTIONS * RTSP/1.0\r\nCSeq: 9\r\n\r\n";
        for boundary in 0..wire.len() {
            let mut decoder = Decoder::default();
            decoder.bytes.extend_from_slice(&wire[..boundary]);
            let first = decoder.next().unwrap();
            decoder.bytes.extend_from_slice(&wire[boundary..]);
            let first = first.or_else(|| decoder.next().unwrap()).unwrap();
            assert_eq!(first.body, "hello");
            assert_eq!(first.cseq().unwrap(), 3);
            let second = decoder.next().unwrap().unwrap();
            assert_eq!(second.method(), "OPTIONS");
            assert!(decoder.next().unwrap().is_none());
        }
    }
    #[test]
    fn decoder_rejects_ambiguous_or_unbounded_lengths() {
        for headers in [
            "Content-Length: 1\r\nContent-Length: 2",
            "Content-Length: 999999999",
            "Content-Length: -1",
        ] {
            let mut d = Decoder {
                bytes: format!("RTSP/1.0 200 OK\r\nCSeq: 1\r\n{headers}\r\n\r\n").into_bytes(),
            };
            assert!(d.next().is_err());
        }
    }
    #[test]
    fn codec_rejection_does_not_advance_to_setup() {
        assert!(check_response(Pending::Parameters, 400).is_err());
        assert!(check_response(Pending::Parameters, 406).is_err());
        assert!(check_response(Pending::Parameters, 200).is_ok());
    }
    #[test]
    fn descriptors_match_the_verified_tv_negotiation() {
        let mut c = Config::default();
        assert_eq!(
            video_descriptor(&c).unwrap(),
            (
                "wfd2_video_formats",
                "00 02 01 0004 000000000100 000000000000 000000000000 00 0000 0000 00 00".into()
            )
        );
        c.profile = Profile::Rhp2;
        assert!(video_descriptor(&c).unwrap().1.starts_with("00 01 04 0010"));
        c.profile = Profile::Baseline;
        assert!(video_descriptor(&c).unwrap().1.contains("00000100"));
        c.fps = 30;
        assert!(video_descriptor(&c).unwrap().1.contains("00000080"));
        c.width = 1280;
        c.height = 720;
        c.fps = 60;
        assert!(video_descriptor(&c).unwrap().1.contains("00000040"));
    }
    #[test]
    fn rhp2_checks_codec_profile_level_and_mode() {
        let body = "wfd2_video_formats: 20 01 04 0040 0000000F97FF 0001555575DF 000000000555 00 0000 0000 1F 00\r\n";
        assert!(supports_rhp2(body));
        for invalid in [
            body.replace("01 04", "02 04"),
            body.replace("0040", "0008"),
            body.replace("0F97FF", "0F96FF"),
            body.replace("0040", "0030"),
        ] {
            assert!(!supports_rhp2(&invalid));
        }
    }
    #[test]
    fn parses_tv_transport_and_aac() {
        assert_eq!(
            parse_ports("RTP/AVP/UDP;unicast 53000 0 mode=play").unwrap(),
            (53000, Some(53001))
        );
        assert_eq!(
            parse_transport("RTP/AVP/UDP;unicast;client_port=53000-53001").unwrap(),
            (53000, Some(53001))
        );
        assert!(parse_transport("RTP/AVP/TCP;unicast;interleaved=0-1").is_err());
        assert_eq!(
            parse_transport("RTP/AVP/UDP;unicast;client_port=53000-0").unwrap(),
            (53000, Some(53001))
        );
        assert_eq!(
            parse_ports("RTP/AVP/UDP;unicast 53000 53000 mode=play").unwrap(),
            (53000, Some(53001))
        );
        assert!(parse_ports("RTP/AVP/UDP;unicast 65535 0 mode=play").is_err());
        assert!(supports_aac("LPCM 00000003 00, AAC 00000001 00"));
        assert!(!supports_aac("AAC 00000002 00"));
    }

    fn mock_tv(
        reject_configuration: bool,
        idr_capability: Option<bool>,
    ) -> (Result<()>, Vec<Event>) {
        use std::sync::{Arc, Mutex};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let source_events = events.clone();
        let source = thread::spawn(move || {
            let stop = AtomicBool::new(false);
            let config = Config {
                timeout: Duration::from_secs(5),
                expected_peer: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                ..Config::default()
            };
            let mut session = Session::negotiate(&listener, &config, &stop)?;
            assert_eq!(session.negotiated().target.port(), 53000);
            assert_eq!(
                session.negotiated().idr_request_capability,
                idr_capability.unwrap_or(false)
            );
            session.run(&stop, |event| {
                source_events.lock().unwrap().push(event);
                Ok(())
            })
        });
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut decoder = Decoder::default();
        fn read(stream: &mut TcpStream, decoder: &mut Decoder) -> Message {
            loop {
                if let Some(message) = decoder.next().unwrap() {
                    return message;
                }
                let mut buf = [0u8; 4096];
                let length = stream.read(&mut buf).unwrap();
                assert!(length > 0);
                decoder.bytes.extend_from_slice(&buf[..length]);
            }
        }
        fn response(stream: &mut TcpStream, sequence: u32, status: u16, body: &str) {
            let bytes = format!(
                "RTSP/1.0 {status} Test\r\nCSeq: {sequence}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            // Exercise stream fragmentation on the actual TCP receive path.
            for chunk in bytes.as_bytes().chunks(7) {
                stream.write_all(chunk).unwrap();
            }
        }
        let options = read(&mut stream, &mut decoder);
        assert_eq!(options.method(), "OPTIONS");
        // A sink OPTIONS request can arrive before its response to source OPTIONS.
        stream
            .write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 101\r\n\r\n")
            .unwrap();
        assert_eq!(read(&mut stream, &mut decoder).status(), Some(200));
        response(&mut stream, options.cseq().unwrap(), 200, "");
        let query = read(&mut stream, &mut decoder);
        assert_eq!(query.method(), "GET_PARAMETER");
        assert!(query.body.contains("wfd_idr_request_capability\r\n"));
        let idr_parameter = idr_capability
            .map(|capable| format!("wfd_idr_request_capability: {}\r\n", u8::from(capable)))
            .unwrap_or_default();
        response(
            &mut stream,
            query.cseq().unwrap(),
            200,
            &format!(
                "wfd_client_rtp_ports: RTP/AVP/UDP;unicast 53000 0 mode=play\r\nwfd_audio_codecs: LPCM 00000003 00, AAC 00000001 00\r\nwfd2_video_formats: none\r\n{idr_parameter}"
            ),
        );
        let configuration = read(&mut stream, &mut decoder);
        assert!(
            configuration
                .body
                .contains("wfd2_video_formats: 00 02 01 0004")
        );
        assert!(
            configuration
                .body
                .contains("wfd2_audio_codecs: AAC 00000001 00")
        );
        assert!(
            configuration
                .body
                .contains("RTP/AVP/UDP;unicast 53000 53001 mode=play")
        );
        response(
            &mut stream,
            configuration.cseq().unwrap(),
            if reject_configuration { 406 } else { 200 },
            "",
        );
        if !reject_configuration {
            let trigger = read(&mut stream, &mut decoder);
            assert!(trigger.body.contains("wfd_trigger_method: SETUP"));
            // SETUP and the M5 response may be read together in either stage.
            response(&mut stream, trigger.cseq().unwrap(), 200, "");
            stream.write_all(b"SETUP rtsp://localhost/wfd1.0/streamid=0 RTSP/1.0\r\nCSeq: 102\r\nTransport: RTP/AVP/UDP;unicast;client_port=53000-53001\r\n\r\n").unwrap();
            let setup = read(&mut stream, &mut decoder);
            assert_eq!(setup.status(), Some(200));
            assert!(
                setup
                    .header("transport")
                    .unwrap()
                    .contains("server_port=50000-50001")
            );
            let id = setup.header("session").unwrap().split(';').next().unwrap();
            for (index, method) in ["PLAY", "PAUSE", "PLAY", "TEARDOWN"].iter().enumerate() {
                stream.write_all(format!("{method} rtsp://localhost/wfd1.0/streamid=0 RTSP/1.0\r\nCSeq: {}\r\nSession: {id}\r\n\r\n",103+index).as_bytes()).unwrap();
                assert_eq!(read(&mut stream, &mut decoder).status(), Some(200));
                if index == 0 {
                    let body = "wfd_idr_request\r\n";
                    stream.write_all(format!("SET_PARAMETER rtsp://localhost/wfd1.0/streamid=0 RTSP/1.0\r\nCSeq: 200\r\nSession: {id}\r\nContent-Length: {}\r\n\r\n{body}",body.len()).as_bytes()).unwrap();
                    assert_eq!(read(&mut stream, &mut decoder).status(), Some(200));
                }
            }
        }
        let result = source.join().unwrap();
        let events = events
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|event| *event != Event::Tick)
            .collect();
        (result, events)
    }

    #[test]
    fn initial_accept_timeout_remains_retryable_through_error_context() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = Config {
            timeout: Duration::from_millis(10),
            ..Config::default()
        };
        let error = Session::negotiate(&listener, &config, &AtomicBool::new(false))
            .err()
            .expect("No TV is connecting")
            .context("Starting mirror");
        assert!(is_initial_connection_timeout(&error));
    }

    #[test]
    fn cancellation_takes_precedence_over_accept_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = Config {
            timeout: Duration::ZERO,
            ..Config::default()
        };
        let error = Session::negotiate(&listener, &config, &AtomicBool::new(true))
            .err()
            .expect("Cancelled before a TV connects");
        assert!(error.to_string().contains("cancelled"));
        assert!(!is_initial_connection_timeout(&error));
    }

    #[test]
    fn connected_tv_negotiation_timeout_is_not_an_accept_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        // Queue an actual connection, but withhold the TV's RTSP response.
        let _tv = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let config = Config {
            timeout: Duration::from_millis(20),
            expected_peer: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            ..Config::default()
        };
        let error = Session::negotiate(&listener, &config, &AtomicBool::new(false))
            .err()
            .expect("Connected TV never answers OPTIONS");
        assert!(error.to_string().contains("Timed out negotiating"));
        assert!(!is_initial_connection_timeout(&error));
    }

    #[test]
    fn full_rtsp_handshake_and_media_lifecycle() {
        for idr_capability in [Some(true), Some(false), None] {
            let (result, events) = mock_tv(false, idr_capability);
            result.unwrap();
            assert_eq!(
                events,
                vec![
                    Event::Play,
                    Event::Idr,
                    Event::Pause,
                    Event::Play,
                    Event::Teardown
                ]
            );
        }
    }

    #[test]
    fn rejected_configuration_ends_before_any_media_start() {
        let (result, events) = mock_tv(true, Some(true));
        let error = result.unwrap_err();
        assert!(error.to_string().contains("RTSP 406"));
        assert!(!is_initial_connection_timeout(&error));
        assert!(events.is_empty());
    }
}
