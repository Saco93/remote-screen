mod audio;
mod hevc_color;
mod latency;
mod measure;
mod media;
mod network;
mod pairing;
mod portal;
mod transport_clock;
mod wfd;

use anyhow::{Context, Result, ensure};
use clap::{Parser, ValueEnum};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    net::TcpListener,
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        net::UnixStream,
    },
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

static LOG: OnceLock<Mutex<File>> = OnceLock::new();
pub fn status(message: &str) {
    println!("{message}");
    if let Some(log) = LOG.get()
        && let Ok(mut file) = log.lock()
    {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = writeln!(file, "[{seconds}] {message}");
        let _ = file.flush();
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Command {
    Mirror,
    Discover,
    Doctor,
    ProbeCodecs,
    VerifyMedia,
    Measure,
}
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Profile {
    #[value(alias = "hevc-experimental")]
    Hevc,
    Baseline,
    Rhp2,
}
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Encoder {
    Auto,
    Vaapi,
    X264,
}

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Native LG C9 mirroring: Rust, HEVC 1080p60 and audio"
)]
struct Args {
    #[arg(value_enum, default_value = "mirror")]
    command: Command,
    #[arg(long, default_value = "OLED65C9")]
    tv: String,
    #[arg(long, default_value = "hevc", value_enum)]
    profile: Profile,
    #[arg(long, default_value = "auto", value_enum)]
    encoder: Encoder,
    #[arg(long, default_value = "1080p60", value_parser = ["1080p60", "1080p30", "720p60"])]
    mode: String,
    #[arg(long)]
    no_audio: bool,
    /// Use the older NetworkManager/WPS transport for troubleshooting.
    #[arg(long)]
    legacy_pairing: bool,
    /// MPEG-TS playback lead in milliseconds (125 preserves the mux default).
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(0..=125))]
    pcr_lead_ms: u32,
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u64).range(1..=60))]
    scan_seconds: u64,
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=600))]
    timeout: u64,
    #[arg(long, default_value = ".state")]
    state_dir: PathBuf,
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(2..=60))]
    test_seconds: u64,
    #[arg(long)]
    interface: Option<String>,
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=60))]
    seconds: u64,
}

impl Args {
    fn settings(&self) -> Result<media::Settings> {
        let (width, height, fps) = match self.mode.as_str() {
            "1080p60" => (1920, 1080, 60),
            "1080p30" => (1920, 1080, 30),
            "720p60" => (1280, 720, 60),
            _ => unreachable!(),
        };
        let profile = match self.profile {
            Profile::Hevc => wfd::Profile::Hevc,
            Profile::Baseline => wfd::Profile::Baseline,
            Profile::Rhp2 => wfd::Profile::Rhp2,
        };
        ensure!(
            matches!(self.profile, Profile::Baseline) || self.mode == "1080p60",
            "HEVC and RHP2 require --mode 1080p60"
        );
        ensure!(
            !matches!(self.profile, Profile::Hevc) || !matches!(self.encoder, Encoder::X264),
            "HEVC requires --encoder vaapi or auto"
        );
        let encoder = match self.encoder {
            Encoder::Auto => media::Encoder::Auto,
            Encoder::Vaapi => media::Encoder::Vaapi,
            Encoder::X264 => media::Encoder::X264,
        };
        Ok(media::Settings {
            profile,
            encoder,
            width,
            height,
            fps,
            idr_request_capability: false,
            pcr_lead_ms: self.pcr_lead_ms,
        })
    }
}

fn internal_output() -> Result<String> {
    let runtime = std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is missing")?;
    let signature =
        std::env::var("HYPRLAND_INSTANCE_SIGNATURE").context("Hyprland is not running")?;
    let mut stream = UnixStream::connect(format!("{runtime}/hypr/{signature}/.socket.sock"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(b"j/monitors")?;
    let mut response = String::new();
    stream.take(1024 * 1024).read_to_string(&mut response)?;
    let monitors: Vec<serde_json::Value> = serde_json::from_str(&response)?;
    let names: Vec<_> = monitors
        .iter()
        .filter(|m| m["disabled"] != true)
        .filter_map(|m| m["name"].as_str())
        .filter(|name| {
            ["eDP-", "LVDS-", "DSI-"]
                .iter()
                .any(|prefix| name.starts_with(prefix))
        })
        .collect();
    ensure!(
        names.len() == 1,
        "Cannot identify exactly one built-in monitor"
    );
    Ok(names[0].to_owned())
}

fn setup_log(args: &Args) -> Result<File> {
    std::fs::create_dir_all(&args.state_dir)?;
    std::fs::set_permissions(&args.state_dir, std::fs::Permissions::from_mode(0o700))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(args.state_dir.join("session.lock"))?;
    lock.try_lock()
        .context("Another remote-screen session is already running")?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(args.state_dir.join("miracast-rust.log"))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let _ = LOG.set(Mutex::new(file));
    Ok(lock)
}

enum Transport {
    Persistent(pairing::Connection),
    Legacy(network::ActiveConnection),
}
impl Transport {
    fn interface(&self) -> &str {
        match self {
            Self::Persistent(connection) => &connection.interface,
            Self::Legacy(connection) => &connection.interface,
        }
    }
}

fn run(args: Args, stop: &AtomicBool) -> Result<()> {
    if matches!(args.command, Command::Measure) {
        return measure::run(
            args.interface
                .as_deref()
                .context("measure requires --interface p2p-wlo1-N")?,
            args.seconds,
            stop,
        );
    }
    let mut settings = args.settings()?;
    gstreamer::init()?;
    match args.command {
        Command::Doctor => {
            println!(
                "Rust {} / GStreamer {}",
                env!("CARGO_PKG_VERSION"),
                gstreamer::version_string()
            );
            println!("Built-in monitor: {}", internal_output()?);
            for name in [
                "pipewiresrc",
                "pulsesrc",
                "vah265enc",
                "vah264enc",
                "x264enc",
                "h265parse",
                "h264parse",
                "fdkaacenc",
                "avenc_aac",
                "mpegtsmux",
                "rtpmp2tpay",
                "rtpbin",
            ] {
                println!(
                    "{name}: {}",
                    if gstreamer::ElementFactory::find(name).is_some() {
                        "available"
                    } else {
                        "missing"
                    }
                );
            }
            network::Network::new()?;
            return Ok(());
        }
        Command::VerifyMedia => {
            return media::Media::smoke(&settings, args.test_seconds, !args.no_audio);
        }
        _ => (),
    }
    let _lock = if matches!(args.command, Command::Discover) {
        None
    } else {
        Some(setup_log(&args)?)
    };
    let network = network::Network::new()?;
    let peers = network.discover_with_cancel(Duration::from_secs(args.scan_seconds), stop)?;
    if matches!(args.command, Command::Discover) {
        println!("{}", serde_json::to_string_pretty(&peers)?);
        return Ok(());
    }
    let peer = network::select_tv(&peers, &args.tv)?;
    let listener = TcpListener::bind("0.0.0.0:7236")
        .context("RTSP port 7236 is busy; stop the previous cast first")?;
    let timeout = Duration::from_secs(args.timeout);
    let probe = matches!(args.command, Command::ProbeCodecs);
    let portal = if probe {
        None
    } else {
        status(&format!(
            "Select built-in monitor {} in the sharing dialog.",
            internal_output()?
        ));
        Some(portal::PortalSession::open_with_cancel(timeout, stop)?)
    };
    status(&format!(
        "Connecting to {} ({}) with {:?} {}",
        peer.name, peer.mac, settings.profile, args.mode
    ));
    network.ensure_p2p_available(peer)?;
    let mut connection = if args.legacy_pairing {
        status("Legacy pairing selected: this can require repeated TV WPS confirmation.");
        Transport::Legacy(network.connect_with_cancel(peer, timeout, stop)?)
    } else {
        Transport::Persistent(pairing::Connection::connect(
            &peer.mac, timeout, true, stop,
        )?)
    };
    status(&format!("Wi-Fi Direct active: {}", connection.interface()));
    let config = wfd::Config {
        profile: settings.profile,
        audio: !args.no_audio,
        width: settings.width,
        height: settings.height,
        fps: settings.fps,
        timeout,
        probe_codecs: probe,
        expected_peer: Some(network::TV_ADDRESS.parse()?),
    };
    let mut session = match wfd::Session::negotiate(&listener, &config, stop) {
        Ok(session) => session,
        Err(error) if !args.legacy_pairing && wfd::is_initial_connection_timeout(&error) => {
            status(
                "TV has not started RTSP; reconnecting the saved pairing once without new WPS consent.",
            );
            drop(connection);
            network.ensure_p2p_available(peer)?;
            connection = Transport::Persistent(pairing::Connection::connect(
                &peer.mac, timeout, false, stop,
            )?);
            status(&format!(
                "Saved Wi-Fi Direct connection restored: {}",
                connection.interface()
            ));
            wfd::Session::negotiate(&listener, &config, stop)
                .context("TV did not start mirroring after saved-pairing recovery")?
        }
        Err(error) => return Err(error),
    };
    if probe {
        status(&session.negotiated().raw_capabilities);
        return Ok(());
    }
    let portal = portal.context("Screen sharing session missing")?;
    status(&format!("Portal session: {}", portal.session_path()));
    let mut audio = if args.no_audio {
        None
    } else {
        Some(audio::Audio::open()?)
    };
    settings.idr_request_capability = session.negotiated().idr_request_capability;
    let mut media = media::Media::new(
        &settings,
        media::Source::Desktop {
            fd: portal.fd(),
            node: portal.node_id(),
            audio_sink: audio.as_ref().map(|a| a.sink.as_str()),
        },
        session.negotiated().target,
        session.negotiated().rtcp_target,
    )?;
    let mut routed = false;
    status("Waiting for television PLAY; Ctrl+C stops mirroring.");
    session.run(stop, |event| {
        match event {
            wfd::Event::Play => {
                media.play()?;
                if !routed {
                    if let Some(audio) = audio.as_mut() {
                        audio.route()?;
                    }
                    routed = true;
                }
                status("Streaming: television requested PLAY.");
            }
            wfd::Event::Pause => media.pause()?,
            wfd::Event::Idr => media.keyframe(),
            wfd::Event::Tick => media.poll()?,
            wfd::Event::Teardown => status("Ending the mirroring session."),
        }
        Ok(())
    })?;
    status(&format!(
        "Session ended; {} encoded frames.",
        media.frame_count()
    ));
    Ok(())
}

fn main() {
    let args = Args::parse();
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    if let Err(error) = ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed)) {
        eprintln!("Cannot install signal handler: {error}");
        std::process::exit(1);
    }
    if let Err(error) = run(args, &stop) {
        if stop.load(Ordering::Relaxed) {
            status("Stopped; session resources released.");
        } else {
            status(&format!("Error: {error:#}"));
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_verified_hevc_1080p60() {
        let args = Args::try_parse_from(["remote-screen"]).unwrap();
        let settings = args.settings().unwrap();
        assert_eq!(settings.profile, wfd::Profile::Hevc);
        assert_eq!(
            (settings.width, settings.height, settings.fps),
            (1920, 1080, 60)
        );
    }
    #[test]
    fn reject_incompatible_hevc_encoder() {
        let args = Args::try_parse_from(["remote-screen", "mirror", "--encoder", "x264"]).unwrap();
        assert!(args.settings().is_err());
    }
}
