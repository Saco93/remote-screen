//! Unprivileged client for the persistent Wi-Fi Direct pairing service.
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const SOCKET: &str = "/run/remote-screen-p2p.sock";
const MAX_LINE: usize = 16 * 1024;
const POLL: Duration = Duration::from_millis(100);

#[derive(Deserialize)]
struct Event {
    event: String,
    message: Option<String>,
    interface: Option<String>,
    reused: Option<bool>,
}

pub struct Connection {
    stream: UnixStream,
    pub interface: String,
    pub reused: bool,
}

impl Connection {
    pub fn connect(
        peer: &str,
        timeout: Duration,
        allow_pairing: bool,
        stop: &AtomicBool,
    ) -> Result<Self> {
        Self::connect_at(
            Path::new(SOCKET),
            peer,
            timeout,
            Duration::from_secs(2),
            allow_pairing,
            stop,
        )
    }

    fn connect_at(
        path: &Path,
        peer: &str,
        timeout: Duration,
        grace: Duration,
        allow_pairing: bool,
        stop: &AtomicBool,
    ) -> Result<Self> {
        ensure!(!stop.load(Ordering::Relaxed), "Mirroring cancelled");
        let deadline = Instant::now()
            .checked_add(timeout)
            .and_then(|d| d.checked_add(grace))
            .context("Pairing timeout is too large")?;
        let stream = UnixStream::connect(path).with_context(|| {
            format!(
                "Cannot connect to pairing service at {}; install it with tools/install-pairing-service",
                path.display()
            )
        })?;
        stream.set_read_timeout(Some(POLL))?;
        stream.set_write_timeout(Some(POLL))?;
        let mut connection = Self {
            stream,
            interface: String::new(),
            reused: false,
        };
        let mut request = serde_json::to_vec(&serde_json::json!({
            "operation": "connect", "peer": peer, "timeout_secs": timeout.as_secs(),
            "allow_pairing": allow_pairing
        }))?;
        request.push(b'\n');
        connection
            .stream
            .write_all(&request)
            .context("Send pairing request")?;
        let mut line = Vec::new();
        loop {
            ensure!(!stop.load(Ordering::Relaxed), "Mirroring cancelled");
            ensure!(
                Instant::now() < deadline,
                "Timed out waiting for pairing service"
            );
            let mut byte = [0];
            match connection.stream.read(&mut byte) {
                Ok(0) => bail!("Pairing service disconnected before ready"),
                Ok(_) if byte[0] == b'\n' => {
                    let event: Event = serde_json::from_slice(&line)
                        .context("Invalid pairing service response")?;
                    line.clear();
                    match event.event.as_str() {
                        "status" => {
                            if let Some(message) = event.message {
                                crate::status(&message);
                            }
                        }
                        "ready" => {
                            let interface =
                                event.interface.context("Pairing ready lacks interface")?;
                            ensure!(
                                interface.starts_with("p2p-")
                                    && interface.len() > 4
                                    && interface.len() < libc::IFNAMSIZ
                                    && interface.bytes().all(|b| b.is_ascii_alphanumeric()
                                        || b == b'-'
                                        || b == b'_'),
                                "Pairing service returned invalid P2P interface"
                            );
                            connection.interface = interface;
                            connection.reused =
                                event.reused.context("Pairing ready lacks reused flag")?;
                            return Ok(connection);
                        }
                        "error" => bail!(
                            "Pairing service: {}",
                            event.message.as_deref().unwrap_or("unspecified error")
                        ),
                        _ => bail!("Unknown pairing service event: {}", event.event),
                    }
                }
                Ok(_) => {
                    ensure!(
                        line.len() < MAX_LINE,
                        "Pairing service response exceeds 16 KiB"
                    );
                    line.push(byte[0]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error).context("Read pairing service response"),
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        let _ = self.stream.write_all(b"{\"operation\":\"disconnect\"}\n");
        let _ = self.stream.shutdown(Shutdown::Write);
        let mut bytes = [0; 1024];
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if remaining.is_zero() {
                break;
            }
            let _ = self.stream.set_read_timeout(Some(POLL.min(remaining)));
            match self.stream.read(&mut bytes) {
                Ok(0) => break,
                Ok(_) => (),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => break,
            }
        }
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        os::unix::net::UnixListener,
        sync::atomic::AtomicU64,
        thread,
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Socket(std::path::PathBuf);
    impl Drop for Socket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn server(
        action: impl FnOnce(UnixStream) + Send + 'static,
    ) -> (Socket, thread::JoinHandle<()>) {
        let path = Socket(std::env::temp_dir().join(format!(
            "rs-pairing-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        let listener = UnixListener::bind(&path.0).unwrap();
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            action(stream);
        });
        (path, worker)
    }
    fn request_policy(stream: &mut UnixStream, allow_pairing: bool) {
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["operation"], "connect");
        assert_eq!(request["peer"], "aa:bb:cc:dd:ee:ff");
        assert_eq!(request["allow_pairing"], allow_pairing);
    }
    fn request(stream: &mut UnixStream) {
        request_policy(stream, true);
    }
    fn connect(path: &Path, timeout: Duration, stop: &AtomicBool) -> Result<Connection> {
        Connection::connect_at(
            path,
            "aa:bb:cc:dd:ee:ff",
            timeout,
            Duration::ZERO,
            true,
            stop,
        )
    }
    #[test]
    fn recovery_forbids_pairing_in_service_request() {
        let (path, worker) = server(|mut stream| {
            request_policy(&mut stream, false);
            stream
                .write_all(b"{\"event\":\"error\",\"message\":\"No persistent group available\"}\n")
                .unwrap();
        });
        let error = Connection::connect_at(
            &path.0,
            "aa:bb:cc:dd:ee:ff",
            Duration::from_secs(2),
            Duration::ZERO,
            false,
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("No persistent group available"));
        worker.join().unwrap();
    }

    #[test]
    fn fragmented_status_ready_and_drop_disconnect() {
        let (path, worker) = server(|mut stream| {
            request(&mut stream);
            for byte in b"{\"event\":\"status\",\"message\":\"Pairing\"}\n{\"event\":\"ready\",\"interface\":\"p2p-wlan0-1\",\"reused\":true}\n" {
                stream.write_all(&[*byte]).unwrap();
            }
            let mut tail = String::new();
            stream.read_to_string(&mut tail).unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(tail.trim()).unwrap()["operation"],
                "disconnect"
            );
        });
        let connection = connect(&path.0, Duration::from_secs(2), &AtomicBool::new(false)).unwrap();
        assert_eq!(connection.interface, "p2p-wlan0-1");
        assert!(connection.reused);
        drop(connection);
        worker.join().unwrap();
    }
    #[test]
    fn eof_error_and_invalid_ready_are_rejected() {
        for (reply, expected) in [
            ("", "disconnected before ready"),
            (
                "{\"event\":\"error\",\"message\":\"Pairing rejected\"}\n",
                "Pairing rejected",
            ),
            (
                "{\"event\":\"ready\",\"interface\":\"wlan0\",\"reused\":false}\n",
                "invalid P2P interface",
            ),
            (
                "{\"event\":\"ready\",\"interface\":\"p2p-1234567890123456\",\"reused\":false}\n",
                "invalid P2P interface",
            ),
        ] {
            let (path, worker) = server(move |mut stream| {
                request(&mut stream);
                stream.write_all(reply.as_bytes()).unwrap();
            });
            let error = connect(&path.0, Duration::from_secs(2), &AtomicBool::new(false))
                .err()
                .unwrap();
            assert!(error.to_string().contains(expected), "{error:#}");
            worker.join().unwrap();
        }
    }
    #[test]
    fn timeout_and_cancellation_disconnect_inflight_request() {
        for cancel in [false, true] {
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let server_stop = stop.clone();
            let (path, worker) = server(move |mut stream| {
                request(&mut stream);
                if cancel {
                    server_stop.store(true, Ordering::Relaxed);
                }
                let mut tail = String::new();
                stream.read_to_string(&mut tail).unwrap();
                assert!(tail.contains("disconnect"));
            });
            let error = connect(&path.0, Duration::from_millis(50), &stop)
                .err()
                .unwrap();
            assert!(
                error
                    .to_string()
                    .contains(if cancel { "cancelled" } else { "Timed out" })
            );
            worker.join().unwrap();
        }
    }
    #[test]
    fn oversized_line_is_rejected_and_connection_closed() {
        let (path, worker) = server(|mut stream| {
            request(&mut stream);
            stream.write_all(&vec![b'x'; MAX_LINE + 1]).unwrap();
            let mut tail = String::new();
            stream.read_to_string(&mut tail).unwrap();
            assert!(tail.contains("disconnect"));
        });
        let error = connect(&path.0, Duration::from_secs(2), &AtomicBool::new(false))
            .err()
            .unwrap();
        assert!(error.to_string().contains("exceeds 16 KiB"));
        worker.join().unwrap();
    }
    #[test]
    fn drop_does_not_wait_forever_for_service_cleanup() {
        let (release, wait) = std::sync::mpsc::channel();
        let (path, worker) = server(move |mut stream| {
            request(&mut stream);
            stream
                .write_all(
                    b"{\"event\":\"ready\",\"interface\":\"p2p-wlan0-1\",\"reused\":false}\n",
                )
                .unwrap();
            let mut tail = String::new();
            stream.read_to_string(&mut tail).unwrap();
            assert!(tail.contains("disconnect"));
            // Hold our write side open until the client's bounded Drop returns.
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let connection = connect(&path.0, Duration::from_secs(2), &AtomicBool::new(false)).unwrap();
        let started = Instant::now();
        drop(connection);
        assert!(started.elapsed() < Duration::from_secs(4));
        release.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn missing_service_explains_installation() {
        let error = connect(
            Path::new("/nonexistent/remote-screen-pairing.sock"),
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("tools/install-pairing-service"));
    }
}
