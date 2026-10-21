//! Calls into the waytool gnome-shell extension's D-Bus interface
//! (com.mxshift.Waytool): window information and focus control (see
//! `windows`), and typing text the keyboard layout lacks. If the
//! extension isn't installed or enabled the calls fail at once, with an
//! error saying so.

use anyhow::{Context, Result, anyhow};

/// The extension exports its object on gnome-shell's own connection, so
/// it's reached through gnome-shell's bus name — which, unlike a name of
/// the extension's own, no other client can claim first and answer for.
const EXT_BUS_NAME: &str = "org.gnome.Shell";
const EXT_OBJECT_PATH: &str = "/com/mxshift/Waytool";
const EXT_INTERFACE: &str = "com.mxshift.Waytool";

const EXT_NOT_FOUND_HINT: &str = "Is the waytool gnome-shell extension installed and enabled? \
     GNOME also turns extensions off while the screen is locked. After installing, log \
     out and back in so gnome-shell picks it up.";

pub(crate) async fn ext_proxy(bus: &zbus::Connection) -> Result<zbus::Proxy<'static>> {
    zbus::Proxy::new(bus, EXT_BUS_NAME, EXT_OBJECT_PATH, EXT_INTERFACE)
        .await
        .context("extension proxy")
}

/// How long an extension call may take.
const EXT_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Call an extension method. Proxy creation is lazy on zbus — the
/// actual "is the extension running?" check happens at the first call
/// — so when the object or method isn't there, the error points at the
/// installer rather than being a bare UnknownObject.
pub(crate) async fn ext_call<B, R>(proxy: &zbus::Proxy<'_>, method: &str, body: &B) -> Result<R>
where
    B: serde::Serialize + zvariant::DynamicType,
    R: for<'d> zvariant::DynamicDeserialize<'d>,
{
    // gnome-shell runs these on its main thread; one that never answers
    // mustn't hang the request (and the input lock it may hold).
    let call = tokio::time::timeout(EXT_CALL_TIMEOUT, proxy.call(method, body))
        .await
        .map_err(|_| {
            anyhow!(
                "{EXT_INTERFACE}.{method} didn't answer within {}s",
                EXT_CALL_TIMEOUT.as_secs()
            )
        })?;
    call.map_err(|e| match e {
        zbus::Error::MethodError(name, _, _)
            if [
                "org.freedesktop.DBus.Error.ServiceUnknown",
                "org.freedesktop.DBus.Error.UnknownObject",
                "org.freedesktop.DBus.Error.UnknownInterface",
                "org.freedesktop.DBus.Error.UnknownMethod",
            ]
            .contains(&name.as_str()) =>
        {
            anyhow!("{EXT_INTERFACE}.{method} isn't available ({name}). {EXT_NOT_FOUND_HINT}")
        }
        zbus::Error::MethodError(_, Some(msg), _) => anyhow!("{method}: {msg}"),
        e => anyhow!("{EXT_INTERFACE}.{method} failed: {e}"),
    })
}

/// Whether the focused window takes text input, so `commit_text` can
/// type into it.
pub(crate) async fn can_commit_text(bus: &zbus::Connection) -> Result<bool> {
    ext_call(&ext_proxy(bus).await?, "CanCommitText", &()).await
}

/// Type `text` into the focused window through gnome-shell's input
/// method, as GNOME's on-screen keyboard does for emoji. Returns false,
/// typing nothing, when the focused window doesn't take text input.
pub(crate) async fn commit_text(bus: &zbus::Connection, text: &str) -> Result<bool> {
    ext_call(&ext_proxy(bus).await?, "CommitText", &(text,)).await
}

/// Press and release a control key (evdev `keycode`, producing
/// `keysym`) in the focused window through gnome-shell's input method,
/// in order with `commit_text`. Returns false when the focused window
/// doesn't take text input.
pub(crate) async fn forward_key(bus: &zbus::Connection, keysym: u32, keycode: u32) -> Result<bool> {
    ext_call(&ext_proxy(bus).await?, "ForwardKey", &(keysym, keycode)).await
}
