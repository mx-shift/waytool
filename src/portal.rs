//! The portal session: establishing it, and input through libei. The
//! RemoteDesktop portal's own Notify* input calls aren't used: every
//! portal that offers RemoteDesktop also offers ConnectToEIS (GNOME 45,
//! Plasma 6.1, COSMIC 1.7), and GNOME reads Notify* pointer coordinates
//! differently from the portal docs.

use anyhow::{Context, Result};
use ashpd::desktop::{
    PersistMode, Session,
    remote_desktop::{
        ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
    },
};

use crate::eis::Eis;

/// One established portal session: the RemoteDesktop session and its
/// libei connection.
pub(crate) struct Portal {
    /// Held for as long as the session should last.
    _session: Session<RemoteDesktop>,
    eis: Eis,
}

impl Portal {
    /// Establish a fresh portal session and connect its input. May
    /// surface a consent dialog.
    pub(crate) async fn establish() -> Result<Portal> {
        let (rd, session) = establish_session().await?;
        let eis = match connect_input(&rd, &session).await {
            Ok(eis) => eis,
            Err(e) => {
                // A started session left open keeps its grant until the
                // daemon exits.
                let _ = session.close().await;
                return Err(e);
            }
        };
        Ok(Portal {
            _session: session,
            eis,
        })
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
async fn establish_session() -> Result<(RemoteDesktop, Session<RemoteDesktop>)> {
    let rd = RemoteDesktop::new().await.context("RemoteDesktop proxy")?;

    let session = rd
        .create_session(Default::default())
        .await
        .context("portal CreateSession")?;
    match start_session(&rd, &session).await {
        Ok(()) => Ok((rd, session)),
        Err(e) => {
            // Don't leave a half-set-up session (or, past Start, a
            // started one) behind.
            let _ = session.close().await;
            Err(e)
        }
    }
}

/// Select devices and start the session. Surfaces the consent dialog.
async fn start_session(rd: &RemoteDesktop, session: &Session<RemoteDesktop>) -> Result<()> {
    let rd_opts = SelectDevicesOptions::default()
        .set_devices(enumflags2::BitFlags::from(DeviceType::Keyboard))
        // The restore token would be thrown away anyway (see the daemon
        // docs), so don't ask the portal to keep a grant around for it.
        .set_persist_mode(PersistMode::DoNot);
    rd.select_devices(session, rd_opts)
        .await
        .context("portal SelectDevices")?
        .response()
        .context("SelectDevices response")?;

    rd.start(session, None, StartOptions::default())
        .await
        .context("portal Start")?
        .response()
        .context("Start response")?;
    Ok(())
}
