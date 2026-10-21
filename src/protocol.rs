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
use serde::{Deserialize, Serialize};
use tokio::net::UnixStream;
use zvariant::{DeserializeDict, SerializeDict, Type};

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

/// One portal stream (a monitor). `position`/`size` are its rect in
/// global logical coordinates, when the portal reported one.
#[derive(Debug, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
pub struct StreamDesc {
    /// The PipeWire node, for a portal stream.
    pub node_id: Option<u32>,
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
}

/// One captured frame: `rgba` is a memfd holding `width * height * 4`
/// bytes of tightly packed RGBA8, starting at offset 0.
#[derive(Debug, Serialize, Deserialize, Type)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: zvariant::OwnedFd,
}

/// Elements of a window's accessibility (AT-SPI2) tree.
#[derive(Debug, Serialize, Deserialize, Type)]
pub struct UiTree {
    pub window_id: u64,
    /// Rects are relative to the window's frame, so they feed straight
    /// into ClickIn. When false they are relative to the app's a11y
    /// toplevel, whose screen position couldn't be verified (typical
    /// for Wayland-native apps).
    pub frame_relative: bool,
    /// The walk hit its element budget before visiting everything.
    pub truncated: bool,
    pub nodes: Vec<UiNode>,
}

/// One accessibility element. `rect` is (x, y, w, h); it is absent for
/// elements without extents and for hidden ones, whose reported
/// positions are meaningless.
#[derive(Debug, SerializeDict, DeserializeDict, Type)]
#[zvariant(signature = "dict")]
pub struct UiNode {
    pub depth: u32,
    pub role: String,
    pub name: String,
    pub showing: bool,
    pub rect: Option<(i32, i32, i32, i32)>,
}

/// A window or monitor as the gnome-shell extension describes it,
/// passed through unchanged: `a{sv}` with keys like `id`, `title`,
/// `frame_x`, `scale`.
pub type WindowDict = std::collections::HashMap<String, zvariant::OwnedValue>;

/// Render a dict value as text. zvariant doesn't implement Display on
/// OwnedValue/Value so this dispatches on the common scalar variants
/// explicitly and falls back to Debug for the rest (arrays, dicts,
/// structs).
pub fn render_value(v: &zvariant::OwnedValue) -> String {
    use zvariant::Value;
    let inner: &Value = v;
    match inner {
        Value::Str(s) => s.to_string(),
        Value::U64(n) => n.to_string(),
        Value::U32(n) => n.to_string(),
        Value::U16(n) => n.to_string(),
        Value::U8(n) => n.to_string(),
        Value::I64(n) => n.to_string(),
        Value::I32(n) => n.to_string(),
        Value::I16(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::F64(f) => format!("{f}"),
        Value::Signature(s) => s.to_string(),
        Value::ObjectPath(p) => p.to_string(),
        _ => format!("{inner:?}"),
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
    /// Press a chord of keysyms separated by '+', left to right, then
    /// release them in reverse.
    fn key_chord(&self, keys: &str) -> Result<(), Error>;
    /// Press+release a Linux evdev keycode.
    fn key_code(&self, code: i32) -> Result<(), Error>;
    /// Press and hold a keysym until a matching KeyUp.
    fn key_down(&self, name: &str) -> Result<(), Error>;
    /// Release a keysym held with KeyDown.
    fn key_up(&self, name: &str) -> Result<(), Error>;
    /// Type a UTF-8 string, one press+release per character.
    /// `delay_ms` waits after each character and `hold_ms` keeps each
    /// key down; 0 means no wait.
    fn type_text(&self, text: &str, delay_ms: u32, hold_ms: u32) -> Result<(), Error>;

    /// Move the pointer to screenshot pixels (x, y) of stream index
    /// `stream`.
    #[zbus(name = "Move")]
    fn move_to(&self, x: f64, y: f64, stream: u32) -> Result<(), Error>;
    /// Move the pointer to a global logical coordinate.
    fn move_global(&self, x: f64, y: f64) -> Result<(), Error>;
    /// Move the pointer by a relative delta in logical pixels.
    fn move_rel(&self, dx: f64, dy: f64) -> Result<(), Error>;
    /// Press+release a pointer button (left, right, middle, back,
    /// forward) at the current position.
    fn click(&self, button: &str) -> Result<(), Error>;
    /// Press and hold a pointer button until a matching ButtonUp.
    fn button_down(&self, button: &str) -> Result<(), Error>;
    /// Release a pointer button held with ButtonDown.
    fn button_up(&self, button: &str) -> Result<(), Error>;
    /// Move to screenshot pixels (x, y) of stream index `stream`, then
    /// click.
    fn click_at(&self, x: f64, y: f64, button: &str, stream: u32) -> Result<(), Error>;
    /// Move to a global logical coordinate, then click.
    fn click_global(&self, x: f64, y: f64, button: &str) -> Result<(), Error>;

    /// The portal streams, in index order.
    fn streams(&self) -> Result<Vec<StreamDesc>, Error>;
    /// Index of the stream whose rect covers global point (x, y).
    fn stream_at(&self, x: f64, y: f64) -> Result<u32, Error>;
    /// Capture one frame per stream, in stream index order.
    fn screenshot(&self) -> Result<Vec<Frame>, Error>;

    /* ----- these need the gnome-shell extension ----- */

    /// Visible top-level windows.
    fn windows(&self) -> Result<Vec<WindowDict>, Error>;
    /// One window by id.
    fn window(&self, id: u64) -> Result<WindowDict, Error>;
    /// Activate (focus + raise) a window by id.
    fn focus(&self, id: u64) -> Result<(), Error>;
    /// Monitors with geometry and scale.
    fn monitors(&self) -> Result<Vec<WindowDict>, Error>;
    /// Windows whose wm_class/app_id/title contains `pattern`. Errors
    /// unless exactly one matches, or `all` is set.
    fn find_window(&self, pattern: &str, all: bool) -> Result<Vec<WindowDict>, Error>;
    /// Focus a window (an id or a pattern matching one window), then
    /// click at (x, y) relative to its frame, or its client area when
    /// `client` is set. Returns the window id and the global point.
    fn click_in(
        &self,
        window: &str,
        x: f64,
        y: f64,
        button: &str,
        client: bool,
    ) -> Result<(u64, f64, f64), Error>;
    /// Focus a window, then type into it like TypeText. Returns the
    /// window id.
    fn type_in(&self, window: &str, text: &str, delay_ms: u32, hold_ms: u32) -> Result<u64, Error>;
    /// A window's accessibility tree down to `depth` levels; hidden
    /// elements only when `all` is set.
    fn ui_tree(&self, window: &str, depth: u32, all: bool) -> Result<UiTree, Error>;
    /// Accessibility elements whose name contains `pattern`, optionally
    /// restricted to `role` (empty for any).
    fn ui_find(&self, window: &str, pattern: &str, role: &str) -> Result<UiTree, Error>;
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
        .context("the daemon isn't answering — is it still waiting for screen-sharing consent?")?
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

        async fn streams(&self) -> Vec<StreamDesc> {
            vec![StreamDesc {
                node_id: Some(42),
                position: Some((-1920, 0)),
                size: None,
            }]
        }

        async fn screenshot(&self) -> Result<Vec<Frame>, Error> {
            let rgba = [1, 2, 3, 255, 4, 5, 6, 255];
            Ok(vec![Frame {
                width: 2,
                height: 1,
                rgba: crate::daemon::memfd_with(&rgba)?.into(),
            }])
        }
    }

    #[tokio::test]
    async fn round_trip_over_socket_pair() {
        let (server, client) = UnixStream::pair().unwrap();
        let served = tokio::spawn(crate::daemon::serve_client(server, |_| Stub));
        let d = connect_stream(client).await.unwrap();

        d.ping().await.unwrap();

        let err = describe(d.key("nope").await.unwrap_err()).to_string();
        assert_eq!(err, "unknown keysym \"nope\"");

        let err = describe(d.key_up("a").await.unwrap_err()).to_string();
        assert!(err.contains("restart it"), "{err}");

        let streams = d.streams().await.unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].node_id, Some(42));
        assert_eq!(streams[0].position, Some((-1920, 0)));
        assert_eq!(streams[0].size, None);

        let mut frames = d.screenshot().await.unwrap();
        assert_eq!(frames.len(), 1);
        let img = crate::frame_to_image(frames.remove(0)).unwrap();
        assert_eq!(img.dimensions(), (2, 1));
        assert_eq!(img.get_pixel(1, 0).0, [4, 5, 6, 255]);

        // The server side finishes once the client hangs up.
        drop(d);
        tokio::time::timeout(std::time::Duration::from_secs(5), served)
            .await
            .expect("server didn't notice the hang-up")
            .unwrap()
            .unwrap();
    }
}
