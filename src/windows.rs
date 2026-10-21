//! Windows: listing them, focusing one, and finding the one a selector
//! names. Every desktop has its own way to do this, and no Wayland
//! protocol gives a window's geometry, so there's a provider per desktop:
//! the gnome-shell extension on GNOME. It reports a window as a dict
//! (see `windows` in the CLI help).

use std::sync::Arc;

use anyhow::{Result, anyhow};

use crate::extension::{ext_call, ext_proxy};
use crate::protocol::{WindowDict, render_value};
use crate::wayland::Wayland;

/// Where window information comes from.
pub(crate) enum Windows {
    /// The gnome-shell extension, on the session bus.
    Gnome(zbus::Connection),
    /// No provider: why.
    Unavailable(String),
}

impl Windows {
    /// Find the provider for the compositor the daemon's Wayland
    /// connection reaches.
    pub(crate) async fn detect(bus: &zbus::Connection, wayland: &Arc<Wayland>) -> Windows {
        let compositor = wayland.compositor_pid();
        // gnome-shell, if it's the compositor (not just on the bus, as it
        // is for a compositor nested inside GNOME).
        if let Some(shell) = bus_name_pid(bus, "org.gnome.Shell").await
            && compositor.is_none_or(|pid| pid == shell)
        {
            return Windows::Gnome(bus.clone());
        }
        Windows::Unavailable(
            "this desktop has no window provider (GNOME with the waytool extension)".into(),
        )
    }

    /// Where windows come from, for the log.
    pub(crate) fn describe(&self) -> String {
        match self {
            Windows::Gnome(_) => "the gnome-shell extension".into(),
            Windows::Unavailable(why) => format!("nowhere: {why}"),
        }
    }

    /// Every top-level window.
    pub(crate) async fn list(&self) -> Result<Vec<WindowDict>> {
        match self {
            Windows::Gnome(bus) => ext_call(&ext_proxy(bus).await?, "GetWindows", &()).await,
            Windows::Unavailable(why) => Err(anyhow!("{why}")),
        }
    }

    /// One window, by id.
    pub(crate) async fn get(&self, id: u64) -> Result<WindowDict> {
        match self {
            Windows::Gnome(bus) => ext_call(&ext_proxy(bus).await?, "GetWindow", &(id,)).await,
            _ => self
                .list()
                .await?
                .into_iter()
                .find(|w| dict_i64(w, "id").is_ok_and(|i| i as u64 == id))
                .ok_or_else(|| anyhow!("no window with id {id}")),
        }
    }

    /// Ask for a window to be focused (raised, its workspace shown).
    /// Activation is asynchronous; read the window back to see whether
    /// focus landed.
    pub(crate) async fn focus(&self, id: u64) -> Result<()> {
        match self {
            Windows::Gnome(bus) => ext_call(&ext_proxy(bus).await?, "FocusWindow", &(id,)).await,
            Windows::Unavailable(why) => Err(anyhow!("{why}")),
        }
    }
}

/// The pid of the process that owns `name` on the bus, if any.
async fn bus_name_pid(bus: &zbus::Connection, name: &str) -> Option<u32> {
    let dbus = zbus::fdo::DBusProxy::new(bus).await.ok()?;
    let name = zbus::names::BusName::try_from(name).ok()?;
    dbus.get_connection_unix_process_id(name).await.ok()
}

/* ----- selectors ----- */

/// Case-insensitive substring match against a window's wm_class,
/// app_id, and title — the find-window matching rule.
pub(crate) fn window_matches(w: &WindowDict, pattern: &str) -> bool {
    let needle = pattern.to_lowercase();
    ["wm_class", "app_id", "title"].iter().any(|k| {
        w.get(*k)
            .map(|v| render_value(v).to_lowercase().contains(&needle))
            .unwrap_or(false)
    })
}

pub(crate) fn dict_i64(w: &WindowDict, key: &str) -> Result<i64> {
    use zvariant::Value;
    let v = w
        .get(key)
        .ok_or_else(|| anyhow!("window dict missing {key:?}"))?;
    Ok(match &**v {
        Value::U64(n) => *n as i64,
        Value::U32(n) => *n as i64,
        Value::I64(n) => *n,
        Value::I32(n) => *n as i64,
        other => return Err(anyhow!("window dict {key:?} is not an integer: {other:?}")),
    })
}

/// One line per window — enough for a caller to pick an id.
pub(crate) fn summarize_windows(windows: &[WindowDict]) -> String {
    windows
        .iter()
        .map(|w| {
            let s = |k: &str| w.get(k).map(render_value).unwrap_or_default();
            format!(
                "  id={}\twm_class={:?}\tapp_id={:?}\tfocused={}\ttitle={:?}",
                s("id"),
                s("wm_class"),
                s("app_id"),
                s("focused"),
                s("title"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use zvariant::{OwnedValue, Value};

    fn win(id: u64, wm_class: &str, title: &str, focused: bool) -> WindowDict {
        let mut w = WindowDict::new();
        w.insert("id".into(), OwnedValue::try_from(Value::U64(id)).unwrap());
        w.insert(
            "wm_class".into(),
            OwnedValue::try_from(Value::new(wm_class)).unwrap(),
        );
        w.insert(
            "app_id".into(),
            OwnedValue::try_from(Value::new("")).unwrap(),
        );
        w.insert(
            "title".into(),
            OwnedValue::try_from(Value::new(title)).unwrap(),
        );
        w.insert(
            "focused".into(),
            OwnedValue::try_from(Value::Bool(focused)).unwrap(),
        );
        w
    }

    /// Live: which provider this session gets.
    /// `cargo test -- --ignored detect_live --nocapture`
    #[tokio::test]
    #[ignore]
    async fn detect_live() {
        let bus = zbus::Connection::session().await.unwrap();
        let wayland = Wayland::connect().unwrap();
        println!("{}", Windows::detect(&bus, &wayland).await.describe());
    }

    #[test]
    fn match_is_case_insensitive_substring() {
        let w = win(1, "dosbox-x", "DOSBox-X 2024", false);
        assert!(window_matches(&w, "DOSBox"));
        assert!(window_matches(&w, "2024"));
        assert!(!window_matches(&w, "firefox"));
    }

    #[test]
    fn dict_accessors() {
        let w = win(7, "x", "y", true);
        assert_eq!(dict_i64(&w, "id").unwrap(), 7);
        assert!(dict_i64(&w, "missing").is_err());
    }
}
