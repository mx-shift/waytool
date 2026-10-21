//! Windows: listing them, focusing one, and finding the one a selector
//! names. Every desktop has its own way to do this, and no Wayland
//! protocol gives a window's geometry, so there's a provider per desktop:
//! the gnome-shell extension on GNOME. It reports a window as a dict
//! (see `windows` in the CLI help).

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};

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

    /// Resolve a window selector, focus the window if it isn't already,
    /// and wait for focus to land (re-reading geometry, which also picks
    /// up a post-raise/unminimize move). An error if it doesn't — a modal
    /// dialog took it, say — naming the window that has focus instead,
    /// since a click or keystroke sent anyway would go somewhere else.
    /// Shared by `click-in` and `type-in`.
    pub(crate) async fn resolve_and_focus(&self, sel: &str) -> Result<WindowDict> {
        let mut win = resolve_window_selector(self.list().await?, sel)?;
        let id = dict_i64(&win, "id")? as u64;
        if dict_bool(&win, "focused") {
            return Ok(win);
        }
        self.focus(id).await?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            win = self
                .get(id)
                .await
                .context("re-reading the window after focusing it")?;
            if dict_bool(&win, "focused") {
                return Ok(win);
            }
        }
        let holder = self
            .list()
            .await?
            .iter()
            .find(|w| dict_bool(w, "focused"))
            .map(|w| {
                format!(
                    "window {} ({:?}) has it",
                    dict_i64(w, "id").unwrap_or(-1),
                    w.get("title").map(render_value).unwrap_or_default()
                )
            })
            .unwrap_or_else(|| "no window has it".into());
        Err(anyhow!(
            "window {id} didn't take keyboard focus; {holder}. Nothing was sent."
        ))
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

pub(crate) fn dict_bool(w: &WindowDict, key: &str) -> bool {
    matches!(w.get(key).map(|v| &**v), Some(zvariant::Value::Bool(true)))
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

/// Resolve a `click-in` window selector against the current window
/// list: a string that parses as a u64 AND names an existing window id
/// wins; otherwise it's a find-window pattern that must match exactly
/// one window — several matches is the same loud error find-window
/// gives, so callers disambiguate instead of clicking into the wrong
/// window.
pub(crate) fn resolve_window_selector(windows: Vec<WindowDict>, sel: &str) -> Result<WindowDict> {
    if let Ok(id) = sel.parse::<i64>()
        && let Some(w) = windows
            .iter()
            .find(|w| dict_i64(w, "id").map(|i| i == id).unwrap_or(false))
    {
        return Ok(w.clone());
    }
    let mut matches: Vec<WindowDict> = windows
        .into_iter()
        .filter(|w| window_matches(w, sel))
        .collect();
    match matches.len() {
        0 => Err(anyhow!("no window matched {sel:?}")),
        1 => Ok(matches.remove(0)),
        n => Err(anyhow!(
            "{n} windows match {sel:?} — refine the pattern or use a window id:\n{}",
            summarize_windows(&matches)
        )),
    }
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
    fn selector_prefers_exact_id() {
        // "2" is both a valid id and a substring of the other title.
        let ws = vec![
            win(1, "term", "window 2", false),
            win(2, "editor", "e", false),
        ];
        let hit = resolve_window_selector(ws, "2").unwrap();
        assert_eq!(dict_i64(&hit, "id").unwrap(), 2);
    }

    #[test]
    fn selector_falls_back_to_pattern_when_id_unknown() {
        let ws = vec![win(1, "term-99", "t", false)];
        let hit = resolve_window_selector(ws, "99").unwrap();
        assert_eq!(dict_i64(&hit, "id").unwrap(), 1);
    }

    #[test]
    fn ambiguous_selector_errors_with_candidates() {
        let ws = vec![win(1, "term", "a", true), win(2, "term", "b", false)];
        let err = resolve_window_selector(ws, "term").unwrap_err().to_string();
        assert!(err.contains("2 windows match"), "{err}");
        assert!(err.contains("id=1"), "{err}");
        assert!(err.contains("id=2"), "{err}");
    }

    #[test]
    fn unmatched_selector_errors() {
        let ws = vec![win(1, "term", "a", false)];
        assert!(resolve_window_selector(ws, "nope").is_err());
    }

    #[test]
    fn dict_accessors() {
        let w = win(7, "x", "y", true);
        assert_eq!(dict_i64(&w, "id").unwrap(), 7);
        assert!(dict_bool(&w, "focused"));
        assert!(!dict_bool(&w, "missing"));
        assert!(dict_i64(&w, "missing").is_err());
    }
}
