//! Where input and screenshots go: the RemoteDesktop and ScreenCast
//! portals with libei (`portal`), where the desktop offers them (GNOME,
//! KDE Plasma, COSMIC), or Wayland protocols (`wlr_input`; input only,
//! so far) where it doesn't (sway and other wlroots compositors,
//! Hyprland, niri). The daemon picks one at start.
//!
//! The portal asks the user for consent and limits capture to the
//! monitors they pick. The Wayland protocols ask nothing: the compositor
//! offers them to every unsandboxed client, so any program running as
//! the user can already do what the daemon does with them.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;

use crate::capture::CapturedFrame;
use crate::portal::Portal;
use crate::wayland::Wayland;
use crate::wlr_input::WlrInput;

/// The Wayland-protocol backend.
pub(crate) struct Native {
    wayland: Arc<Wayland>,
    input: WlrInput,
}

pub(crate) enum Backend {
    Portal(Portal),
    Native(Box<Native>),
}

/// One capture stream, as `streams` reports it: a portal stream, or a
/// monitor under the Wayland backend.
pub(crate) struct Stream {
    /// The PipeWire node, for a portal stream.
    pub node_id: Option<u32>,
    /// The monitor's rect in global logical coordinates.
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
}

impl Stream {
    pub(crate) fn contains(&self, x: f64, y: f64) -> bool {
        match (self.position, self.size) {
            (Some((sx, sy)), Some((sw, sh))) => {
                let (sx, sy, sw, sh) = (sx as f64, sy as f64, sw as f64, sh as f64);
                x >= sx && x < sx + sw && y >= sy && y < sy + sh
            }
            _ => false,
        }
    }
}

/// Which backend to use: `WAYTOOL_BACKEND` (`portal` or
/// `wayland`); else the Wayland protocols when the compositor offers all
/// the backend needs; else the portal if the desktop's RemoteDesktop
/// portal offers libei.
///
/// The compositor is asked first because it's the one thing that can't
/// be wrong about itself: the portal service picks its backends from
/// configuration, and with several desktops installed it can offer
/// RemoteDesktop through one (KDE's, say) that can't work under the
/// running compositor, leaving the session waiting for a dialog that
/// never shows. The compositors that offer the full set (sway and other
/// wlroots compositors, Hyprland, niri) have no working RemoteDesktop
/// portal of their own anyway; GNOME and KDE don't offer it, and COSMIC
/// lacks a virtual pointer.
pub(crate) async fn choose(bus: &zbus::Connection, wayland: &Wayland) -> Result<Choice> {
    match std::env::var("WAYTOOL_BACKEND").as_deref() {
        Ok("portal") => return Ok(Choice::Portal),
        Ok("wayland") => return Ok(Choice::Native),
        Ok(other) => {
            return Err(anyhow!(
                "WAYTOOL_BACKEND is {other:?}; it can be \"portal\" or \"wayland\""
            ));
        }
        Err(_) => {}
    }
    if native_complete(wayland) {
        return Ok(Choice::Native);
    }
    match remote_desktop_version(bus).await {
        // ConnectToEIS arrived in version 2.
        Ok(v) if v >= 2 => Ok(Choice::Portal),
        Ok(v) => {
            eprintln!(
                "waytool: the RemoteDesktop portal is version {v}, without libei; \
                 using Wayland protocols"
            );
            Ok(Choice::Native)
        }
        Err(e) => {
            eprintln!("waytool: no RemoteDesktop portal ({e:#}); using Wayland protocols");
            Ok(Choice::Native)
        }
    }
}

/// Whether the compositor offers everything the Wayland backend needs.
fn native_complete(wayland: &Wayland) -> bool {
    let g = wayland.globals();
    g.seat.is_some() && g.virtual_keyboard.is_some() && g.virtual_pointer.is_some()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Choice {
    Portal,
    Native,
}

/// The RemoteDesktop portal's interface version, which reading needs no
/// session or consent.
async fn remote_desktop_version(bus: &zbus::Connection) -> Result<u32> {
    let reply = bus
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.freedesktop.portal.RemoteDesktop", "version"),
        )
        .await
        .context("read the RemoteDesktop portal's version")?;
    let value: zvariant::OwnedValue = reply.body().deserialize()?;
    Ok(u32::try_from(value)?)
}

impl Native {
    pub(crate) fn start(wayland: Arc<Wayland>) -> Result<Native> {
        let input = WlrInput::new(wayland.clone())?;
        Ok(Native { wayland, input })
    }
}

impl Backend {
    pub(crate) fn has_keysym(&self, keysym: u32) -> Result<bool> {
        match self {
            Backend::Portal(p) => p.has_keysym(keysym),
            Backend::Native(n) => n.input.has_keysym(keysym),
        }
    }

    pub(crate) fn keysym(&self, keysym: u32, press: bool) -> Result<()> {
        match self {
            Backend::Portal(p) => p.keysym(keysym, press),
            Backend::Native(n) => n.input.keysym(keysym, press),
        }
    }

    pub(crate) fn tap_keysym(&self, keysym: u32) -> Result<()> {
        self.keysym(keysym, true)?;
        self.keysym(keysym, false)
    }

    pub(crate) fn tap_keycode(&self, code: u32) -> Result<()> {
        match self {
            Backend::Portal(p) => p.tap_keycode(code),
            Backend::Native(n) => {
                n.input.keycode(code, true)?;
                n.input.keycode(code, false)
            }
        }
    }

    pub(crate) fn button(&self, code: u32, press: bool) -> Result<()> {
        match self {
            Backend::Portal(p) => p.button(code, press),
            Backend::Native(n) => n.input.button(code, press),
        }
    }

    pub(crate) fn click(&self, code: u32) -> Result<()> {
        self.button(code, true)?;
        self.button(code, false)
    }

    pub(crate) fn move_rel(&self, dx: f64, dy: f64) -> Result<()> {
        match self {
            Backend::Portal(p) => p.move_rel(dx, dy),
            Backend::Native(n) => n.input.move_rel(dx, dy),
        }
    }

    /// Move to pixel (x, y) of stream `stream`'s screenshot.
    pub(crate) fn move_to_pixel(&self, stream: u32, x: f64, y: f64) -> Result<()> {
        match self {
            Backend::Portal(p) => p.move_to_pixel(stream, x, y),
            Backend::Native(n) => n.input.move_to_pixel(stream, x, y),
        }
    }

    pub(crate) fn move_to_global(&self, x: f64, y: f64) -> Result<()> {
        match self {
            Backend::Portal(p) => p.move_to_global(x, y),
            Backend::Native(n) => n.input.move_to_global(x, y),
        }
    }

    /// The capture streams: the portal session's, or every monitor.
    pub(crate) fn streams(&self) -> Vec<Stream> {
        match self {
            Backend::Portal(p) => p
                .streams()
                .iter()
                .map(|s| Stream {
                    node_id: Some(s.node_id),
                    position: s.position,
                    size: s.size,
                })
                .collect(),
            Backend::Native(n) => n
                .wayland
                .monitors()
                .into_iter()
                .map(|m| Stream {
                    node_id: None,
                    position: Some((m.x, m.y)),
                    size: Some((m.width, m.height)),
                })
                .collect(),
        }
    }

    /// Capture one frame per stream.
    pub(crate) async fn capture(&self) -> Result<Vec<CapturedFrame>> {
        match self {
            Backend::Portal(p) => {
                let node_ids: Vec<u32> = p.streams().iter().map(|s| s.node_id).collect();
                let fd = p.pipewire_fd().await?;
                tokio::task::spawn_blocking(move || {
                    crate::capture::pipewire_capture_frames(fd, &node_ids)
                })
                .await
                .context("screenshot blocking task")?
            }
            Backend::Native(_) => Err(anyhow!(
                "the Wayland backend can't take screenshots yet; only the portals can"
            )),
        }
    }

    /// Wait until the session ends for good, and say why: the portal
    /// closed it or its libei connection was lost, or the Wayland
    /// connection was lost.
    pub(crate) async fn ended(&self) -> &'static str {
        match self {
            Backend::Portal(p) => {
                let mut closed = match p.session.receive_closed().await {
                    Ok(closed) => closed,
                    Err(e) => {
                        eprintln!(
                            "waytool: could not watch session Closed signal ({e:#}); \
                             daemon will not auto-exit when capture stops"
                        );
                        p.input_lost().await;
                        return "lost the libei input connection";
                    }
                };
                tokio::select! {
                    _ = closed.next() => {
                        "portal session closed (screen sharing stopped or consent revoked)"
                    }
                    _ = p.input_lost() => "lost the libei input connection",
                }
            }
            Backend::Native(n) => {
                let mut changes = n.wayland.changes();
                while !n.wayland.is_lost() {
                    if changes.changed().await.is_err() {
                        break;
                    }
                }
                "lost the Wayland connection"
            }
        }
    }

    /// The portal session, which a display change may replace.
    pub(crate) fn portal(&self) -> Option<&Portal> {
        match self {
            Backend::Portal(p) => Some(p),
            Backend::Native(_) => None,
        }
    }

    /// End the session: close the portal's, so screen sharing stops now.
    pub(crate) async fn close(&self) {
        if let Backend::Portal(p) = self
            && let Err(e) = p.session.close().await
        {
            eprintln!("waytool: closing portal session failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_does_not_round_toward_zero() {
        let s = Stream {
            node_id: Some(1),
            position: Some((0, 0)),
            size: Some((100, 50)),
        };
        assert!(s.contains(0.0, 0.0));
        assert!(s.contains(99.9, 49.9));
        assert!(!s.contains(-0.5, 10.0));
        assert!(!s.contains(100.0, 10.0));
        assert!(!s.contains(f64::NAN, 10.0));
    }
}
