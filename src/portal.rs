//! ScreenCast portal ownership and request/response handling.
//!
//! https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html
use std::{
    collections::HashMap,
    future::Future,
    os::fd::{AsRawFd, OwnedFd, RawFd},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use futures_lite::{StreamExt, future};
use serde::Serialize;
use zbus::{
    MatchRule, MessageStream,
    blocking::Connection,
    zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Value},
};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const DESKTOP: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";
static TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(1);
type Results = HashMap<String, OwnedValue>;

/// The portal session must outlive every consumer of `fd()` and `node_id()`.
pub struct PortalSession {
    session: SessionGuard,
    remote: OwnedFd,
    node: u32,
}

struct SessionGuard {
    connection: Connection,
    path: OwnedObjectPath,
}

impl PortalSession {
    /// Ask the desktop to select one monitor, with an embedded cursor.
    /// Select the laptop's built-in display in the desktop's permission dialog.
    /// `timeout` is a total deadline for the entire authorization sequence.
    /// A true cancellation flag interrupts waiting within 25ms. Request and
    /// session cleanup each wait at most another 75ms for the portal.
    pub fn open_with_cancel(timeout: Duration, cancelled: &AtomicBool) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        let connection = Connection::from(timed_cancellable(
            deadline,
            "connecting to the session bus",
            cancelled,
            async { Ok(zbus::Connection::session().await?) },
        )?);
        let sender = connection
            .unique_name()
            .context("session bus has no unique name")?;
        let sender_component = sender.as_str().trim_start_matches(':').replace('.', "_");
        let session_token = token();
        let session_path = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/session/{sender_component}/{session_token}"
        ))?;
        // Construct the guard before CreateSession so even a late or failed reply
        // releases the session on our private bus connection.
        let mut session = SessionGuard {
            connection,
            path: session_path,
        };
        let request_token = token();
        let options = HashMap::from([
            ("handle_token", Value::from(request_token.as_str())),
            ("session_handle_token", Value::from(session_token.as_str())),
        ]);
        let mut created = request(
            &session.connection,
            deadline,
            cancelled,
            "CreateSession",
            &request_token,
            &(options,),
        )?;
        let handle = created
            .remove("session_handle")
            .context("portal omitted session_handle")?;
        // The portal specification intentionally declares session_handle as a string.
        session.path = OwnedObjectPath::try_from(String::try_from(handle)?)?;

        let request_token = token();
        let options = HashMap::from([
            ("handle_token", Value::from(request_token.as_str())),
            ("types", Value::from(1u32)),
            ("multiple", Value::from(false)),
            ("cursor_mode", Value::from(2u32)),
        ]);
        request(
            &session.connection,
            deadline,
            cancelled,
            "SelectSources",
            &request_token,
            &(&session.path, options),
        )?;

        let request_token = token();
        let options = HashMap::from([("handle_token", Value::from(request_token.as_str()))]);
        let mut started = request(
            &session.connection,
            deadline,
            cancelled,
            "Start",
            &request_token,
            &(&session.path, "", options),
        )?;
        let node = stream_node(
            started
                .remove("streams")
                .context("portal omitted streams")?,
        )?;
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        let remote: zbus::zvariant::OwnedFd =
            timed_cancellable(deadline, "opening the PipeWire remote", cancelled, async {
                let reply = session
                    .connection
                    .inner()
                    .call_method(
                        Some(DESTINATION),
                        DESKTOP,
                        Some(SCREENCAST),
                        "OpenPipeWireRemote",
                        &(&session.path, options),
                    )
                    .await?;
                Ok(reply.body().deserialize()?)
            })?;
        Ok(Self {
            session,
            remote: remote.into(),
            node,
        })
    }

    pub fn node_id(&self) -> u32 {
        self.node
    }

    pub fn fd(&self) -> RawFd {
        self.remote.as_raw_fd()
    }

    pub fn session_path(&self) -> &str {
        self.session.path.as_str()
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        close(&self.connection, self.path.as_str(), SESSION);
    }
}

fn token() -> String {
    format!(
        "remote_screen_{}_{}",
        std::process::id(),
        TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn timed<T>(
    deadline: Instant,
    operation: &str,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    if Instant::now() >= deadline {
        bail!("timed out while {operation}");
    }
    future::block_on(future::race(work, async {
        async_io::Timer::at(deadline).await;
        bail!("timed out while {operation}")
    }))
}

fn timed_cancellable<T>(
    deadline: Instant,
    operation: &str,
    cancelled: &AtomicBool,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    if cancelled.load(Ordering::Relaxed) {
        bail!("screen sharing cancelled while {operation}");
    }
    timed(
        deadline,
        operation,
        future::race(work, async {
            loop {
                async_io::Timer::after(Duration::from_millis(25)).await;
                if cancelled.load(Ordering::Relaxed) {
                    bail!("screen sharing cancelled while {operation}");
                }
            }
        }),
    )
}

fn close(connection: &Connection, path: &str, interface: &str) {
    let _ = timed(
        Instant::now() + Duration::from_millis(75),
        "closing the portal object",
        async {
            connection
                .inner()
                .call_method(Some(DESTINATION), path, Some(interface), "Close", &())
                .await?;
            Ok(())
        },
    );
}

fn request<B>(
    connection: &Connection,
    deadline: Instant,
    cancelled: &AtomicBool,
    method: &str,
    request_token: &str,
    body: &B,
) -> Result<Results>
where
    B: Serialize + DynamicType,
{
    let sender = connection
        .unique_name()
        .context("session bus has no unique name")?;
    let sender_component = sender.as_str().trim_start_matches(':').replace('.', "_");
    let mut request_path =
        format!("/org/freedesktop/portal/desktop/request/{sender_component}/{request_token}");
    let result = timed_cancellable(deadline, method, cancelled, async {
        // Subscribe before the method call. Matching all Response paths also handles
        // portals returning a different request path without a subscription race.
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(DESTINATION)?
            .interface(REQUEST)?
            .member("Response")?
            .build();
        let mut responses =
            MessageStream::for_match_rule(rule, connection.inner(), Some(16)).await?;
        let reply = connection
            .inner()
            .call_method(Some(DESTINATION), DESKTOP, Some(SCREENCAST), method, body)
            .await?;
        let returned: OwnedObjectPath = reply.body().deserialize()?;
        request_path = returned.to_string();
        while let Some(message) = responses.next().await {
            let message = message?;
            if message.header().path().map(|p| p.as_str()) != Some(request_path.as_str()) {
                continue;
            }
            let (code, results): (u32, Results) = message.body().deserialize()?;
            return response_result(method, code, results);
        }
        bail!("portal disconnected while waiting for {method}")
    });
    if result.is_err() {
        close(connection, &request_path, REQUEST);
    }
    result
}

fn response_result(method: &str, code: u32, results: Results) -> Result<Results> {
    match code {
        0 => Ok(results),
        1 => bail!("screen sharing was cancelled during {method}"),
        code => bail!("screen sharing portal rejected {method} (response {code})"),
    }
}

fn stream_node(value: OwnedValue) -> Result<u32> {
    let streams: Vec<(u32, Results)> = value
        .try_into()
        .context("invalid portal streams response")?;
    if streams.len() != 1 {
        bail!("expected one monitor stream, received {}", streams.len());
    }
    Ok(streams[0].0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_and_failure_are_not_success() {
        assert!(response_result("Start", 0, Results::new()).is_ok());
        assert!(
            response_result("Start", 1, Results::new())
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        assert!(
            response_result("Start", 2, Results::new())
                .unwrap_err()
                .to_string()
                .contains("rejected")
        );
    }

    #[test]
    fn deadline_interrupts_a_missing_response() {
        let began = Instant::now();
        let result: Result<()> = timed(
            began + Duration::from_millis(10),
            "test response",
            future::pending(),
        );
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(began.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancellation_interrupts_a_missing_response() {
        let cancelled = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(10));
                cancelled.store(true, Ordering::Relaxed);
            });
            let began = Instant::now();
            let result: Result<()> = timed_cancellable(
                began + Duration::from_secs(30),
                "test response",
                &cancelled,
                future::pending(),
            );
            assert!(result.unwrap_err().to_string().contains("cancelled"));
            assert!(began.elapsed() < Duration::from_millis(250));
        });
    }

    #[test]
    fn already_cancelled_does_not_start_the_operation() {
        let cancelled = AtomicBool::new(true);
        let result: Result<()> = timed_cancellable(
            Instant::now() + Duration::from_secs(30),
            "test response",
            &cancelled,
            async { panic!("cancelled operation must not run") },
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
    }

    #[test]
    fn tokens_are_unique_object_path_components() {
        let first = token();
        assert_ne!(first, token());
        assert!(
            first
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        );
    }

    #[test]
    fn stream_reply_preserves_node_and_requires_one_monitor() {
        let stream = Value::from(vec![(73u32, Results::new())]);
        assert_eq!(stream_node(stream.try_into().unwrap()).unwrap(), 73);
        let empty: Vec<(u32, Results)> = Vec::new();
        assert!(stream_node(Value::from(empty).try_into().unwrap()).is_err());
        let multiple = Value::from(vec![(73u32, Results::new()), (74u32, Results::new())]);
        assert!(stream_node(multiple.try_into().unwrap()).is_err());
    }
}
