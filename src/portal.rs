//! The portal session: establishing it, its streams, and input through
//! libei. The RemoteDesktop portal's own Notify* input calls aren't
//! used: every portal that offers RemoteDesktop also offers
//! ConnectToEIS (GNOME 45, Plasma 6.1, COSMIC 1.7), and GNOME reads
//! Notify* pointer coordinates differently from the portal docs.

use std::os::fd::OwnedFd;

use anyhow::{Context, Result, anyhow};
use ashpd::desktop::{
    PersistMode, Session,
    remote_desktop::{
        ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
    },
    screencast::{
        CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    },
};

use crate::eis::Eis;

/// One portal stream: its PipeWire node, and, when the source is a
/// monitor, the monitor's rect in global logical coordinates (the
/// compositor's layout). A screenshot of it is the rect's size times
/// the monitor's scale.
#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub node_id: u32,
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
    /// Matches the stream to its libei region (ScreenCast v5).
    pub mapping_id: Option<String>,
}

impl StreamInfo {
    /// Does this stream's reported rect cover point (x, y) in global
    /// screen coordinates?  Returns false if the portal didn't supply
    /// position+size for this stream (Window-source streams).
    pub fn contains(&self, x: f64, y: f64) -> bool {
        match (self.position, self.size) {
            (Some((sx, sy)), Some((sw, sh))) => {
                let (sx, sy, sw, sh) = (sx as f64, sy as f64, sw as f64, sh as f64);
                x >= sx && x < sx + sw && y >= sy && y < sy + sh
            }
            _ => false,
        }
    }
}

/// One established portal session: the RemoteDesktop + ScreenCast
/// session, its libei connection, and the streams it started.
pub(crate) struct Portal {
    pub(crate) session: Session<RemoteDesktop>,
    eis: Eis,
    /// For OpenPipeWireRemote, which each capture calls for a PipeWire
    /// connection of its own.
    sc: Screencast,
    /// Each portal stream, in portal order.
    streams: Vec<StreamInfo>,
    /// Whether GNOME's physical layout mode was on (see `display`).
    pub(crate) physical_layout: bool,
}

impl Portal {
    /// Establish a fresh portal session and connect its input. May
    /// surface a consent dialog.
    pub(crate) async fn establish(bus: &zbus::Connection) -> Result<Portal> {
        let (rd, sc, session, streams) = establish_session().await?;
        let physical_layout = crate::display::physical_layout(bus).await.unwrap_or(false);
        let eis = match connect_input(&rd, &session).await {
            Ok(eis) => eis,
            Err(e) => {
                // A started session left open keeps sharing the screen
                // until the daemon exits.
                let _ = session.close().await;
                return Err(e);
            }
        };
        Ok(Portal {
            session,
            eis,
            sc,
            streams,
            physical_layout,
        })
    }

    /// The session's streams.
    pub(crate) fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    /// A new PipeWire connection for one capture.
    pub(crate) async fn pipewire_fd(&self) -> Result<OwnedFd> {
        open_pipewire_remote(&self.sc, &self.session).await
    }

    /// Whether `keysym` is on the keyboard layout, so it can be typed as
    /// a key.
    pub(crate) fn has_keysym(&self, keysym: u32) -> Result<bool> {
        self.eis.has_keysym(keysym)
    }

    /// Press or release the key for `keysym`.
    pub(crate) fn keysym(&self, keysym: u32, press: bool) -> Result<()> {
        self.eis.keysym(keysym, press)
    }

    pub(crate) fn tap_keysym(&self, keysym: u32) -> Result<()> {
        self.keysym(keysym, true)?;
        self.keysym(keysym, false)
    }

    /// Press and release a Linux evdev keycode.
    pub(crate) fn tap_keycode(&self, code: u32) -> Result<()> {
        self.eis.keycode(code, true)?;
        self.eis.keycode(code, false)
    }

    /// Press or release a Linux evdev pointer button.
    pub(crate) fn button(&self, code: u32, press: bool) -> Result<()> {
        self.eis.button(code, press)
    }

    pub(crate) fn click(&self, code: u32) -> Result<()> {
        self.button(code, true)?;
        self.button(code, false)
    }

    /// Relative pointer motion in logical pixels.
    pub(crate) fn move_rel(&self, dx: f64, dy: f64) -> Result<()> {
        self.eis.move_rel(dx, dy)
    }

    /// Move the pointer to screenshot pixel (x, y) of stream index
    /// `stream`.
    pub(crate) fn move_to_pixel(&self, stream: u32, x: f64, y: f64) -> Result<()> {
        let s = self
            .streams
            .get(stream as usize)
            .ok_or_else(|| anyhow!("no stream {stream}; `waytool streams` lists them"))?;
        let rect = s.position.zip(s.size);
        let mut region = self
            .eis
            .region_for(s.mapping_id.as_deref(), rect)
            .ok_or_else(|| anyhow!("the compositor gave no libei region for stream {stream}"))?;
        // In physical layout mode a monitor's logical size is its mode, so
        // a screenshot pixel is one logical pixel whatever the region's
        // scale says.
        if self.physical_layout {
            region.scale = 1.0;
        }
        let (w, h) = region.physical_size();
        if !(0.0..w).contains(&x) || !(0.0..h).contains(&y) {
            return Err(anyhow!(
                "({x}, {y}) is outside stream {stream}'s {w:.0}x{h:.0} screenshot"
            ));
        }
        let (gx, gy) = region.pixel_to_global(x, y);
        self.eis.move_abs(gx, gy)
    }

    /// Move the pointer to a global logical coordinate — the space
    /// window and monitor rects are reported in.
    pub(crate) fn move_to_global(&self, x: f64, y: f64) -> Result<()> {
        if !self.eis.in_any_region(x, y) {
            return Err(anyhow!("no monitor covers global point ({x}, {y})"));
        }
        self.eis.move_abs(x, y)
    }
}

/// Connect the session's input through libei.
async fn connect_input(rd: &RemoteDesktop, session: &Session<RemoteDesktop>) -> Result<Eis> {
    let fd = rd
        .connect_to_eis(session, ConnectToEISOptions::default())
        .await
        .context(
            "portal ConnectToEIS (this desktop's RemoteDesktop portal doesn't offer libei input)",
        )?;
    Eis::connect(fd).await
}

/// Establish the portal session. Surfaces the consent dialog.
async fn establish_session() -> Result<(
    RemoteDesktop,
    Screencast,
    Session<RemoteDesktop>,
    Vec<StreamInfo>,
)> {
    let rd = RemoteDesktop::new().await.context("RemoteDesktop proxy")?;
    let sc = Screencast::new().await.context("Screencast proxy")?;

    let session = rd
        .create_session(Default::default())
        .await
        .context("portal CreateSession")?;
    match start_session(&rd, &sc, &session).await {
        Ok(streams) => Ok((rd, sc, session, streams)),
        Err(e) => {
            // Don't leave a half-set-up session (or, past Start, a
            // started one) behind.
            let _ = session.close().await;
            Err(e)
        }
    }
}

/// Select devices and sources, start the session, and return its
/// streams. Surfaces the consent dialog.
async fn start_session(
    rd: &RemoteDesktop,
    sc: &Screencast,
    session: &Session<RemoteDesktop>,
) -> Result<Vec<StreamInfo>> {
    let rd_opts = SelectDevicesOptions::default()
        .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
        // The restore token would be thrown away anyway (see the daemon
        // docs), so don't ask the portal to keep a grant around for it.
        .set_persist_mode(PersistMode::DoNot);
    rd.select_devices(session, rd_opts)
        .await
        .context("portal SelectDevices")?
        .response()
        .context("SelectDevices response")?;

    let sc_opts = SelectSourcesOptions::default()
        .set_sources(enumflags2::BitFlags::<SourceType>::from(
            SourceType::Monitor,
        ))
        .set_cursor_mode(CursorMode::Embedded)
        .set_multiple(true);
    sc.select_sources(session, sc_opts)
        .await
        .context("portal Screencast SelectSources")?
        .response()
        .context("Screencast SelectSources response")?;

    let started = rd
        .start(session, None, StartOptions::default())
        .await
        .context("portal Start")?
        .response()
        .context("Start response")?;

    let streams: Vec<StreamInfo> = started
        .streams()
        .iter()
        .map(|s| StreamInfo {
            node_id: s.pipe_wire_node_id(),
            position: s.position(),
            size: s.size(),
            mapping_id: s.mapping_id().map(str::to_owned),
        })
        .collect();
    if streams.is_empty() {
        return Err(anyhow!(
            "portal returned no streams — screencast consent denied or no monitor picked"
        ));
    }
    for (i, st) in streams.iter().enumerate() {
        eprintln!(
            "waytool: stream[{}] node={} pos={:?} size={:?}",
            i, st.node_id, st.position, st.size
        );
    }

    Ok(streams)
}

/// Open a PipeWire connection that sees the session's streams. The
/// portal makes a new connection on every call; closing the fd ends it,
/// and the server then destroys everything created over it.
async fn open_pipewire_remote(
    sc: &Screencast,
    session: &Session<RemoteDesktop>,
) -> Result<OwnedFd> {
    sc.open_pipe_wire_remote(session, OpenPipeWireRemoteOptions::default())
        .await
        .context("portal OpenPipeWireRemote")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_does_not_round_toward_zero() {
        let s = StreamInfo {
            node_id: 1,
            position: Some((0, 0)),
            size: Some((100, 50)),
            mapping_id: None,
        };
        assert!(s.contains(0.0, 0.0));
        assert!(s.contains(99.9, 49.9));
        assert!(!s.contains(-0.5, 10.0));
        assert!(!s.contains(100.0, 10.0));
        assert!(!s.contains(f64::NAN, 10.0));
    }
}
