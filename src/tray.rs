//! `waytool tray`: a system-tray icon that switches the daemon's systemd
//! user unit on and off, for desktops without the GNOME extension's
//! Quick Settings toggle. It's a StatusNotifierItem (the freedesktop
//! tray protocol KDE's system tray is built on, which COSMIC's panel and
//! bars like waybar also host): the icon's tooltip shows the unit's
//! state, and activating it (a click) starts or stops the unit — so on
//! desktops with a consent dialog, the dialog comes up on start as it
//! does from the GNOME toggle.
//!
//! The packages start it at login with an XDG autostart entry.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use zbus::OwnedObjectPath;
use zbus::object_server::SignalEmitter;

const UNIT: &str = "waytool.service";
const ITEM_PATH: &str = "/StatusNotifierItem";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const SYSTEMD: &str = "org.freedesktop.systemd1";
const ICON: &str = "preferences-desktop-remote-desktop";

/// The unit's state, as the tray shows it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Unit {
    Missing,
    Off,
    /// Starting: through the portals, waiting for screen-sharing consent.
    Starting,
    On,
    Stopping,
    Failed,
}

impl Unit {
    fn from_states(load: &str, active: &str) -> Unit {
        match (load, active) {
            ("not-found", _) => Unit::Missing,
            (_, "active" | "reloading") => Unit::On,
            (_, "activating") => Unit::Starting,
            (_, "deactivating") => Unit::Stopping,
            (_, "failed") => Unit::Failed,
            _ => Unit::Off,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Unit::Missing => "not installed",
            Unit::Off => "off — click to start",
            Unit::Starting => "waiting for screen-sharing consent",
            Unit::On => "on — click to stop",
            Unit::Stopping => "stopping",
            Unit::Failed => "failed — click to start again",
        }
    }
}

/// A StatusNotifierItem tooltip: icon name, icon pixmaps (width,
/// height, ARGB32 data), title, description.
type ToolTip = (String, Vec<(i32, i32, Vec<u8>)>, String, String);

/// The StatusNotifierItem.
struct Item {
    state: Arc<Mutex<Unit>>,
    systemd: zbus::Connection,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "waytool"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "waytool"
    }

    /// Always in view (hosts tuck passive items away, and a toggle you
    /// can't see is no use), drawing attention when the unit failed.
    #[zbus(property)]
    fn status(&self) -> &str {
        match *self.state.lock().unwrap() {
            Unit::Failed => "NeedsAttention",
            _ => "Active",
        }
    }

    #[zbus(property)]
    fn icon_name(&self) -> &str {
        ICON
    }

    /// Hosts that can draw an overlay mark the stopped states.
    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        match *self.state.lock().unwrap() {
            Unit::On => "",
            Unit::Starting | Unit::Stopping => "chronometer",
            Unit::Off | Unit::Missing => "process-stop",
            Unit::Failed => "dialog-error",
        }
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        "dialog-error"
    }

    #[zbus(property)]
    fn tool_tip(&self) -> ToolTip {
        let state = *self.state.lock().unwrap();
        (
            ICON.into(),
            Vec::new(),
            "waytool".into(),
            format!("The waytool daemon is {}.", state.describe()),
        )
    }

    /// No menu of its own: activation toggles.
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }

    /// Start the unit, or stop it if it's running or starting.
    async fn activate(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> {
        let state = *self.state.lock().unwrap();
        let method = match state {
            Unit::On | Unit::Starting => "StopUnit",
            Unit::Off | Unit::Failed => "StartUnit",
            Unit::Missing | Unit::Stopping => return Ok(()),
        };
        self.systemd
            .call_method(
                Some(SYSTEMD),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.systemd1.Manager"),
                method,
                &(UNIT, "replace"),
            )
            .await?;
        Ok(())
    }

    async fn secondary_activate(&self, x: i32, y: i32) -> zbus::fdo::Result<()> {
        self.activate(x, y).await
    }

    async fn context_menu(&self, _x: i32, _y: i32) {}

    async fn scroll(&self, _delta: i32, _orientation: String) {}

    #[zbus(signal)]
    async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_overlay_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_status(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;
}

/// Run the tray icon until the session ends.
pub(crate) async fn run_tray() -> Result<()> {
    let bus = zbus::Connection::session()
        .await
        .context("connect to the session bus")?;
    // The user's systemd instance is on the session bus too.
    let systemd = bus.clone();
    let unit = unit_path(&systemd).await?;
    let state = Arc::new(Mutex::new(read_state(&systemd, &unit).await));

    let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    bus.object_server()
        .at(
            ITEM_PATH,
            Item {
                state: state.clone(),
                systemd: systemd.clone(),
            },
        )
        .await?;
    bus.request_name(name.as_str()).await?;
    let iface = bus.object_server().interface::<_, Item>(ITEM_PATH).await?;

    // Register with the tray host's watcher, now and whenever a new one
    // appears (the panel restarting, say).
    let dbus = zbus::fdo::DBusProxy::new(&bus).await?;
    let mut owners = dbus.receive_name_owner_changed().await?;
    register(&bus, &name).await;

    // Follow the unit.
    let props = zbus::fdo::PropertiesProxy::builder(&systemd)
        .destination(SYSTEMD)
        .path(unit.clone())
        .build()
        .await?;
    let mut changes = props.receive_properties_changed().await?;

    loop {
        tokio::select! {
            Some(change) = owners.next() => {
                if let Ok(args) = change.args()
                    && args.name().as_str() == WATCHER
                    && args.new_owner().is_some()
                {
                    register(&bus, &name).await;
                }
            }
            Some(_) = changes.next() => {
                let now = read_state(&systemd, &unit).await;
                if std::mem::replace(&mut *state.lock().unwrap(), now) != now {
                    let emitter = iface.signal_emitter();
                    let item = iface.get().await;
                    let _ = Item::new_status(emitter, item.status()).await;
                    let _ = Item::new_overlay_icon(emitter).await;
                    let _ = Item::new_tool_tip(emitter).await;
                    let _ = Item::new_icon(emitter).await;
                }
            }
            else => return Ok(()),
        }
    }
}

/// Tell the tray host about the item. Without a host (GNOME without an
/// AppIndicator extension) there's nothing to show it in; it stays ready
/// for one to appear.
async fn register(bus: &zbus::Connection, name: &str) {
    let result = bus
        .call_method(
            Some(WATCHER),
            "/StatusNotifierWatcher",
            Some(WATCHER),
            "RegisterStatusNotifierItem",
            &(name,),
        )
        .await;
    if let Err(e) = result {
        eprintln!("waytool tray: no system tray to show in yet ({e})");
    }
}

/// The unit's object path, loading it if systemd hasn't.
async fn unit_path(systemd: &zbus::Connection) -> Result<OwnedObjectPath> {
    Ok(systemd
        .call_method(
            Some(SYSTEMD),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "LoadUnit",
            &(UNIT,),
        )
        .await
        .context("ask the systemd user instance for waytool.service")?
        .body()
        .deserialize()?)
}

async fn read_state(systemd: &zbus::Connection, unit: &OwnedObjectPath) -> Unit {
    let get = |prop: &'static str| async move {
        let reply = systemd
            .call_method(
                Some(SYSTEMD),
                unit.as_str(),
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.freedesktop.systemd1.Unit", prop),
            )
            .await
            .ok()?;
        let value: zbus::OwnedVariant = reply.body().deserialize().ok()?;
        String::try_from(value).ok()
    };
    let load = get("LoadState").await.unwrap_or_default();
    let active = get("ActiveState").await.unwrap_or_default();
    Unit::from_states(&load, &active)
}

#[cfg(test)]
mod tests {
    use super::Unit;

    #[test]
    fn unit_states_map_to_tray_states() {
        assert_eq!(Unit::from_states("not-found", "inactive"), Unit::Missing);
        assert_eq!(Unit::from_states("loaded", "inactive"), Unit::Off);
        assert_eq!(Unit::from_states("loaded", "activating"), Unit::Starting);
        assert_eq!(Unit::from_states("loaded", "active"), Unit::On);
        assert_eq!(Unit::from_states("loaded", "failed"), Unit::Failed);
    }
}
