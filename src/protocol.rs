//! The daemon's wire protocol: D-Bus, spoken peer-to-peer over the
//! daemon's private unix socket rather than through a message bus.
//!
//! Each CLI invocation connects to the socket, runs the D-Bus
//! authentication handshake directly with the daemon, and makes one
//! method call on [`OBJECT_PATH`]. There is no bus in the middle, so
//! nothing beyond the socket's filesystem permissions can reach the
//! daemon. Tools that speak D-Bus peer-to-peer work against it
//! (busctl and gdbus expect a bus and don't):
//!
//! ```text
//! dbus-send --peer=unix:path=$XDG_RUNTIME_DIR/waytool.sock --print-reply \
//!     /com/mxshift/Waytool/Daemon org.freedesktop.DBus.Introspectable.Introspect
//! ```
//!
//! The interface name carries a version. An incompatible change gets a
//! new name (`Daemon2`), so a client talking to a daemon from another
//! release gets "unknown interface/method" instead of misread replies.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use tokio::net::UnixStream;

/// The one object the daemon serves. Its interface is
/// `com.mxshift.Waytool.Daemon1`, spelled out in the proxy below
/// and in the daemon's `#[interface]` because the macros need literals.
pub const OBJECT_PATH: &str = "/com/mxshift/Waytool/Daemon";

/// Where the daemon listens: XDG_RUNTIME_DIR, which is private to the
/// user (0700) and cleaned on logout. Every desktop session sets it, and
/// the daemon can't run outside one anyway, so there is no fallback.
pub fn socket_path() -> Result<PathBuf> {
    let rt = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
        anyhow!(
            "XDG_RUNTIME_DIR is not set — run this from the desktop session, or set \
             it to /run/user/$(id -u)"
        )
    })?;
    Ok(PathBuf::from(rt).join("waytool.sock"))
}

/// Errors the daemon returns. `Failed` carries a human-readable
/// message meant to be shown to the user as is.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "com.mxshift.Waytool.Error")]
pub enum Error {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Failed(format!("{e:#}"))
    }
}

/// Client side of the daemon interface. The `default_service` is
/// required by the proxy machinery but means nothing on a
/// peer-to-peer connection; the daemon ignores the destination.
#[zbus::proxy(
    interface = "com.mxshift.Waytool.Daemon1",
    default_service = "com.mxshift.Waytool.Daemon",
    default_path = "/com/mxshift/Waytool/Daemon",
    gen_blocking = false
)]
pub trait Daemon {
    /// Cheap liveness check.
    fn ping(&self) -> Result<(), Error>;

    /// Press+release a single keysym by name (space, Return, a, ...).
    fn key(&self, name: &str) -> Result<(), Error>;
}

/// Connect to the running daemon.
pub async fn connect() -> Result<DaemonProxy<'static>> {
    let sock = socket_path()?;
    let stream = UnixStream::connect(&sock).await.with_context(|| {
        format!(
            "connecting to {} — is `waytool daemon` running?",
            sock.display()
        )
    })?;
    connect_stream(stream).await
}

async fn connect_stream(stream: UnixStream) -> Result<DaemonProxy<'static>> {
    let handshake = zbus::connection::Builder::unix_stream(stream).p2p().build();
    let conn = tokio::time::timeout(std::time::Duration::from_secs(10), handshake)
        .await
        .context("the daemon isn't answering — is it still waiting for consent?")?
        .context("D-Bus handshake with the daemon")?;
    DaemonProxy::new(&conn).await.context("daemon proxy")
}

/// Turn a failed method call into an error worth showing: the daemon's
/// own message for `Failed`, and a hint for the mismatch you get when
/// the running daemon is from a different release than the CLI.
pub fn describe(e: Error) -> anyhow::Error {
    match e {
        Error::Failed(msg) => anyhow!(msg),
        Error::ZBus(zbus::Error::MethodError(name, _, _))
            if [
                "org.freedesktop.DBus.Error.UnknownMethod",
                "org.freedesktop.DBus.Error.UnknownInterface",
                "org.freedesktop.DBus.Error.UnknownObject",
            ]
            .contains(&name.as_str()) =>
        {
            anyhow!(
                "the running daemon doesn't support this command ({name}); it's from \
                 a different release than this CLI — restart it"
            )
        }
        Error::ZBus(e) => anyhow!(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for the daemon's interface, which needs a live portal
    /// session.
    struct Stub;

    #[zbus::interface(name = "com.mxshift.Waytool.Daemon1")]
    impl Stub {
        async fn ping(&self) {}

        async fn key(&self, name: String) -> Result<(), Error> {
            Err(anyhow!("unknown keysym {name:?}").into())
        }
    }

    #[tokio::test]
    async fn round_trip_over_socket_pair() {
        let (server, client) = UnixStream::pair().unwrap();
        let served = tokio::spawn(crate::daemon::serve_client(server, Stub));
        let d = connect_stream(client).await.unwrap();

        d.ping().await.unwrap();

        let err = describe(d.key("nope").await.unwrap_err()).to_string();
        assert_eq!(err, "unknown keysym \"nope\"");

        // The server side finishes once the client hangs up.
        drop(d);
        tokio::time::timeout(std::time::Duration::from_secs(5), served)
            .await
            .expect("server didn't notice the hang-up")
            .unwrap()
            .unwrap();
    }
}
