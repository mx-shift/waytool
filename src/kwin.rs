//! Windows on KDE Plasma, through KWin's scripting interface: KWin is
//! the only thing that knows them, and it shares them only with scripts
//! it runs. The script is kwin/waytool.js, installed with the
//! daemon (it's never written at run time). For each request the daemon
//! loads it (org.kde.kwin.Scripting.loadScript), the script asks the
//! daemon what to do and replies over D-Bus to a bus name the daemon
//! owns, and the daemon unloads it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use zbus::message::Header;

const NAME: &str = "com.mxshift.Waytool.KWin";
const PATH: &str = "/com/mxshift/Waytool/KWin";
const KWIN: &str = "org.kde.KWin";

/// How long a request may take, script loading included.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The request in flight, and where its reply goes.
#[derive(Default)]
struct Exchange {
    request: String,
    reply: Option<tokio::sync::oneshot::Sender<String>>,
}

/// The object the script talks to.
struct Channel {
    exchange: Arc<Mutex<Exchange>>,
}

/// Fail unless the caller is KWin: anyone could call our name, and a
/// reply decides what we report and focus.
async fn check_kwin(conn: &zbus::Connection, header: &Header<'_>) -> zbus::fdo::Result<()> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    let kwin = dbus
        .get_name_owner(zbus::names::BusName::try_from(KWIN).unwrap())
        .await?;
    match header.sender() {
        Some(sender) if sender.as_str() == kwin.as_str() => Ok(()),
        _ => Err(zbus::fdo::Error::AccessDenied(
            "only KWin may call this".into(),
        )),
    }
}

#[zbus::interface(name = "com.mxshift.Waytool.KWin")]
impl Channel {
    /// The request for the script to carry out, as JSON.
    async fn request(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<String> {
        check_kwin(conn, &header).await?;
        Ok(self.exchange.lock().unwrap().request.clone())
    }

    /// The script's result, as JSON.
    async fn reply(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        reply: String,
    ) -> zbus::fdo::Result<()> {
        check_kwin(conn, &header).await?;
        if let Some(tx) = self.exchange.lock().unwrap().reply.take() {
            let _ = tx.send(reply);
        }
        Ok(())
    }
}

/// KWin, reached through the script.
pub(crate) struct Kwin {
    bus: zbus::Connection,
    script: PathBuf,
    exchange: Arc<Mutex<Exchange>>,
    /// One request at a time: the script fetches "the" request.
    serial: tokio::sync::Mutex<u64>,
}

impl Kwin {
    /// Set up: find the script, own our bus name, and serve the object
    /// the script calls.
    pub(crate) async fn new(bus: &zbus::Connection) -> Result<Kwin> {
        let script = find_script()?;
        let exchange = Arc::new(Mutex::new(Exchange::default()));
        bus.object_server()
            .at(
                PATH,
                Channel {
                    exchange: exchange.clone(),
                },
            )
            .await
            .context("serve the KWin script's object")?;
        bus.request_name_with_flags(NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into())
            .await
            .with_context(|| format!("own {NAME} (is another waytool daemon running?)"))?;
        Ok(Kwin {
            bus: bus.clone(),
            script,
            exchange,
            serial: tokio::sync::Mutex::new(0),
        })
    }

    /// Run `request` through the script and return its reply.
    pub(crate) async fn call(&self, request: serde_json::Value) -> Result<serde_json::Value> {
        let mut serial = self.serial.lock().await;
        *serial += 1;
        let plugin = format!("waytool-{}-{}", std::process::id(), *serial);
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut ex = self.exchange.lock().unwrap();
            ex.request = request.to_string();
            ex.reply = Some(tx);
        }
        let result = tokio::time::timeout(TIMEOUT, async {
            let script = self.script.to_string_lossy();
            let id: i32 = self
                .bus
                .call_method(
                    Some(KWIN),
                    "/Scripting",
                    Some("org.kde.kwin.Scripting"),
                    "loadScript",
                    &(script.as_ref(), plugin.as_str()),
                )
                .await
                .context("KWin loadScript")?
                .body()
                .deserialize()?;
            if id < 0 {
                return Err(anyhow!("KWin couldn't load {}", self.script.display()));
            }
            self.bus
                .call_method(
                    Some(KWIN),
                    format!("/Scripting/Script{id}").as_str(),
                    Some("org.kde.kwin.Script"),
                    "run",
                    &(),
                )
                .await
                .context("KWin run script")?;
            rx.await
                .map_err(|_| anyhow!("the KWin script gave no reply"))
        })
        .await;
        let _ = self
            .bus
            .call_method(
                Some(KWIN),
                "/Scripting",
                Some("org.kde.kwin.Scripting"),
                "unloadScript",
                &(plugin.as_str(),),
            )
            .await;
        let reply = result.map_err(|_| anyhow!("KWin didn't answer within {TIMEOUT:?}"))??;
        let reply: serde_json::Value = serde_json::from_str(&reply).context("KWin script reply")?;
        if let Some(e) = reply["error"].as_str() {
            return Err(anyhow!("KWin: {e}"));
        }
        Ok(reply)
    }
}

/// The installed script: WAYTOOL_KWIN_SCRIPT, or
/// waytool/kwin/waytool.js under the XDG data directories,
/// or kwin/ in the source checkout a development build runs from.
fn find_script() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("WAYTOOL_KWIN_SCRIPT") {
        return Ok(PathBuf::from(path));
    }
    let rel = "waytool/kwin/waytool.js";
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    let checkout = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.parent()?.join("kwin/waytool.js")));
    home.map(|h| h.join(rel))
        .into_iter()
        .chain(dirs.split(':').map(|d| PathBuf::from(d).join(rel)))
        .chain(checkout)
        .find(|p| p.is_file())
        .ok_or_else(|| {
            anyhow!(
                "KWin window support needs its script, waytool/kwin/waytool.js, \
                 in a data directory (the packages install it to /usr/share)"
            )
        })
}
