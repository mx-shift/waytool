//! ui-tree / ui-find: read another app's widget tree over the dedicated
//! accessibility bus (AT-SPI2). GTK, Qt, Firefox and Chromium expose
//! trees; SDL, emulator and custom-toolkit apps expose nothing.
//! Coordinates: AT-SPI "window" coordinates are relative to the app's
//! accessible toplevel. When the toplevel's screen extents can be
//! trusted (X11 and XWayland apps), they are translated to be relative
//! to the window's frame, so they feed straight into click-in.

use anyhow::{Context, Result, anyhow};

use crate::protocol::{UiNode, UiTree, WindowDict, render_value};
use crate::windows::{Windows, dict_i64, resolve_window_selector};

const ATSPI_ROOT_DEST: &str = "org.a11y.atspi.Registry";
const ATSPI_ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
const ATSPI_IFACE_ACC: &str = "org.a11y.atspi.Accessible";
const ATSPI_IFACE_COMP: &str = "org.a11y.atspi.Component";
/// StateType bit in word 0 of GetState: element is currently rendered.
const ATSPI_STATE_SHOWING: u32 = 1 << 25;
/// GetExtents coordinate types.
const ATSPI_COORD_SCREEN: u32 = 0;
const ATSPI_COORD_WINDOW: u32 = 1;
/// Hard cap on accessibles visited per walk — big apps (browsers)
/// expose tens of thousands of nodes and each costs D-Bus round-trips.
const UI_WALK_BUDGET: usize = 5000;
/// How deep ui-find searches.
const UI_DEFAULT_DEPTH: u32 = 40;

#[derive(Clone, Debug)]
struct AccRef {
    dest: String,
    path: zvariant::OwnedObjectPath,
}

/// Connect to the dedicated accessibility bus (its address comes from
/// org.a11y.Bus on the session bus).
pub(crate) async fn a11y_connection(session: &zbus::Connection) -> Result<zbus::Connection> {
    let reply = session
        .call_method(
            Some("org.a11y.Bus"),
            "/org/a11y/bus",
            Some("org.a11y.Bus"),
            "GetAddress",
            &(),
        )
        .await
        .context("org.a11y.Bus.GetAddress — is the accessibility stack running?")?;
    let addr: String = reply.body().deserialize()?;
    zbus::connection::Builder::address(addr.as_str())?
        .build()
        .await
        .with_context(|| format!("connect to accessibility bus at {addr}"))
}

async fn acc_children(conn: &zbus::Connection, r: &AccRef) -> Result<Vec<AccRef>> {
    let reply = conn
        .call_method(
            Some(r.dest.as_str()),
            &r.path,
            Some(ATSPI_IFACE_ACC),
            "GetChildren",
            &(),
        )
        .await?;
    let kids: Vec<(String, zvariant::OwnedObjectPath)> = reply.body().deserialize()?;
    Ok(kids
        .into_iter()
        .filter(|(_, p)| p.as_str() != "/org/a11y/atspi/null")
        .map(|(dest, path)| AccRef { dest, path })
        .collect())
}

async fn acc_name(conn: &zbus::Connection, r: &AccRef) -> String {
    let reply = conn
        .call_method(
            Some(r.dest.as_str()),
            &r.path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(ATSPI_IFACE_ACC, "Name"),
        )
        .await;
    match reply {
        Ok(m) => m
            .body()
            .deserialize::<zvariant::OwnedValue>()
            .ok()
            .and_then(|v| String::try_from(v).ok())
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

async fn acc_role(conn: &zbus::Connection, r: &AccRef) -> String {
    match conn
        .call_method(
            Some(r.dest.as_str()),
            &r.path,
            Some(ATSPI_IFACE_ACC),
            "GetRoleName",
            &(),
        )
        .await
    {
        Ok(m) => m.body().deserialize().unwrap_or_else(|_| "?".into()),
        Err(_) => "?".into(),
    }
}

/// Is the element currently rendered?  Errors default to true so a
/// flaky app hides nothing.
async fn acc_showing(conn: &zbus::Connection, r: &AccRef) -> bool {
    match conn
        .call_method(
            Some(r.dest.as_str()),
            &r.path,
            Some(ATSPI_IFACE_ACC),
            "GetState",
            &(),
        )
        .await
    {
        Ok(m) => m
            .body()
            .deserialize::<Vec<u32>>()
            .ok()
            .and_then(|words| words.first().copied())
            .map(|w| w & ATSPI_STATE_SHOWING != 0)
            .unwrap_or(true),
        Err(_) => true,
    }
}

/// (x, y, w, h) in the requested AT-SPI coordinate space; None when the
/// element has no Component interface (structural nodes).
async fn acc_extents(
    conn: &zbus::Connection,
    r: &AccRef,
    coord: u32,
) -> Option<(i32, i32, i32, i32)> {
    let m = conn
        .call_method(
            Some(r.dest.as_str()),
            &r.path,
            Some(ATSPI_IFACE_COMP),
            "GetExtents",
            &(coord,),
        )
        .await
        .ok()?;
    m.body()
        .deserialize::<((i32, i32, i32, i32),)>()
        .ok()
        .map(|(t,)| t)
}

async fn a11y_app_pid(conn: &zbus::Connection, dest: &str) -> Option<u32> {
    let m = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetConnectionUnixProcessID",
            &(dest,),
        )
        .await
        .ok()?;
    m.body().deserialize().ok()
}

/// Find the AT-SPI applications that could belong to an extension
/// window dict, best match first.  Primary key is the pid; fallback is
/// a name match against wm_class/app_id (flatpak apps sit behind
/// xdg-dbus-proxy, so their bus pid is the proxy's, not the app's).
/// Returns a list because one app can register several bus connections
/// (86Box under flatpak registers two, only one of which exposes
/// windows) — the caller keeps the first candidate with toplevels — and
/// whether they matched by pid. Names are a weaker match, used only when
/// no app has the window's pid.
async fn a11y_find_apps(conn: &zbus::Connection, win: &WindowDict) -> Result<(Vec<AccRef>, bool)> {
    let root = AccRef {
        dest: ATSPI_ROOT_DEST.into(),
        path: zvariant::OwnedObjectPath::try_from(ATSPI_ROOT_PATH)?,
    };
    let apps = acc_children(conn, &root)
        .await
        .context("list AT-SPI applications")?;
    let win_pid = dict_i64(win, "pid").ok();
    let targets: Vec<String> = ["wm_class", "app_id"]
        .iter()
        .filter_map(|k| win.get(*k).map(|v| render_value(v).to_lowercase()))
        .filter(|s| !s.is_empty())
        .collect();

    let mut by_pid = Vec::new();
    let mut by_name = Vec::new();
    let mut names = Vec::new();
    for a in &apps {
        if win_pid.is_some() && a11y_app_pid(conn, &a.dest).await == win_pid.map(|p| p as u32) {
            by_pid.push(a.clone());
            continue;
        }
        let name = acc_name(conn, a).await;
        if name.is_empty() {
            continue;
        }
        if targets.iter().any(|t| app_name_matches(&name, t)) {
            by_name.push(a.clone());
        } else {
            names.push(name);
        }
    }
    let matched_pid = !by_pid.is_empty();
    if !matched_pid {
        by_pid = by_name;
    }
    if by_pid.is_empty() {
        return Err(anyhow!(
            "no AT-SPI application matches this window (pid {win_pid:?}) — the app \
             probably doesn't expose accessibility info (SDL/emulator/custom-toolkit \
             apps don't). Apps currently on the a11y bus: {}",
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            }
        ));
    }
    Ok((by_pid, matched_pid))
}

/// Whether AT-SPI application name `name` is the app a window's
/// wm_class or app_id `target` (lowercased) names: equal to it, or to
/// its last dotted component (org.gnome.TextEditor → texteditor),
/// ignoring dashes and underscores. Substrings don't count — a short
/// name would match unrelated windows.
fn app_name_matches(name: &str, target: &str) -> bool {
    let norm = |s: &str| s.to_lowercase().replace(['-', '_', ' '], "");
    let name = norm(name);
    let last = target.rsplit('.').next().unwrap_or(target);
    !name.is_empty() && (name == norm(target) || name == norm(last))
}

/// Pick the a11y toplevel matching the extension window's title,
/// trying each candidate app connection until one exposes windows. A
/// lone toplevel stands for the window without a title check only when
/// the app was found by pid.
async fn a11y_find_toplevel(
    conn: &zbus::Connection,
    apps: &[AccRef],
    title: &str,
    by_pid: bool,
) -> Result<AccRef> {
    let mut tops = Vec::new();
    for app in apps {
        tops = acc_children(conn, app)
            .await
            .context("list application toplevels")?;
        if !tops.is_empty() {
            break;
        }
    }
    match tops.len() {
        0 => return Err(anyhow!("application exposes no windows on the a11y bus")),
        1 if by_pid => return Ok(tops[0].clone()),
        _ => {}
    }
    let mut names = Vec::new();
    for t in &tops {
        let name = acc_name(conn, t).await;
        if name == title {
            return Ok(t.clone());
        }
        names.push(name);
    }
    // No exact title match: a single SHOWING toplevel is unambiguous,
    // for an app found by pid.
    let mut showing = Vec::new();
    for t in &tops {
        if acc_showing(conn, t).await {
            showing.push(t.clone());
        }
    }
    if by_pid && showing.len() == 1 {
        return Ok(showing.remove(0));
    }
    Err(anyhow!(
        "none of the app's {} a11y toplevels matches title {title:?}: {names:?}",
        tops.len()
    ))
}

/// Offset of the a11y toplevel's origin within OUR frame rect, when the
/// app's SCREEN coordinates are trustworthy (X11/XWayland apps report
/// honest global coords; Wayland CSD apps report surface-relative junk,
/// which lands outside the frame rect and yields None).
async fn a11y_frame_offset(
    conn: &zbus::Connection,
    top: &AccRef,
    win: &WindowDict,
) -> Option<(i32, i32)> {
    let (sx, sy, _, _) = acc_extents(conn, top, ATSPI_COORD_SCREEN).await?;
    let fx = dict_i64(win, "frame_x").ok()? as i32;
    let fy = dict_i64(win, "frame_y").ok()? as i32;
    let fw = dict_i64(win, "frame_w").ok()? as i32;
    let fh = dict_i64(win, "frame_h").ok()? as i32;
    let (dx, dy) = (sx - fx, sy - fy);
    if dx >= 0 && dy >= 0 && dx < fw && dy < fh {
        Some((dx, dy))
    } else {
        None
    }
}

/// Depth-first walk of an a11y subtree in child order.  Returns the
/// visited nodes plus whether the walk hit the node budget.  Skips (and
/// doesn't descend into) non-SHOWING elements unless `include_hidden`.
async fn a11y_walk(
    conn: &zbus::Connection,
    top: &AccRef,
    max_depth: u32,
    include_hidden: bool,
) -> Result<(Vec<UiNode>, bool)> {
    let mut out = Vec::new();
    let mut stack = vec![(top.clone(), 0u32)];
    let mut visited = 0usize;
    let mut truncated = false;
    while let Some((r, depth)) = stack.pop() {
        visited += 1;
        if visited > UI_WALK_BUDGET {
            truncated = true;
            break;
        }
        // Hidden elements are skipped without asking anything else of
        // them; otherwise the per-element calls go out together, which
        // matters when every one is a D-Bus round trip to the app.
        if !include_hidden && !acc_showing(conn, &r).await {
            continue;
        }
        let showing = async {
            if include_hidden {
                acc_showing(conn, &r).await
            } else {
                true
            }
        };
        let children = async {
            if depth < max_depth {
                acc_children(conn, &r).await.unwrap_or_default()
            } else {
                Vec::new()
            }
        };
        let (showing, role, name, rect, children) = futures_util::join!(
            showing,
            acc_role(conn, &r),
            acc_name(conn, &r),
            acc_extents(conn, &r, ATSPI_COORD_WINDOW),
            children,
        );
        out.push(UiNode {
            depth,
            role,
            name,
            rect,
            showing,
        });
        for k in children.into_iter().rev() {
            stack.push((k, depth + 1));
        }
    }
    Ok((out, truncated))
}

/// Shared front half of ui-tree/ui-find: resolve the window through the
/// extension, find its a11y app + toplevel, and work out the coordinate
/// translation.  Returns (toplevel, window id, frame offset).
async fn ui_query_common(
    windows: &Windows,
    conn: &zbus::Connection,
    sel: &str,
) -> Result<(AccRef, i64, Option<(i32, i32)>)> {
    let win = resolve_window_selector(windows.list().await?, sel)?;
    let id = dict_i64(&win, "id")?;
    let title = win.get("title").map(render_value).unwrap_or_default();

    let (apps, by_pid) = a11y_find_apps(conn, &win).await?;
    let top = a11y_find_toplevel(conn, &apps, &title, by_pid).await?;
    let off = a11y_frame_offset(conn, &top, &win).await;
    Ok((top, id, off))
}

/// Translate a walk into what `UiTree`/`UiFind` return: rects moved
/// into frame-relative space when the toplevel's offset is known, and
/// dropped for hidden elements — their coordinates are meaningless (Qt
/// reports popped-down menus at garbage positions), so don't hand out
/// numbers an agent might click.
fn ui_reply(
    id: i64,
    off: Option<(i32, i32)>,
    truncated: bool,
    nodes: impl IntoIterator<Item = UiNode>,
) -> UiTree {
    let (dx, dy) = off.unwrap_or((0, 0));
    UiTree {
        window_id: id as u64,
        frame_relative: off.is_some(),
        truncated,
        nodes: nodes
            .into_iter()
            .map(|n| UiNode {
                rect: n
                    .rect
                    .filter(|_| n.showing)
                    .map(|(x, y, w, h)| (x + dx, y + dy, w, h)),
                ..n
            })
            .collect(),
    }
}

/// `windows` resolves the window, `a11y` is the accessibility bus.
pub(crate) async fn ui_tree(
    windows: &Windows,
    a11y: &zbus::Connection,
    sel: &str,
    depth: u32,
    all: bool,
) -> Result<UiTree> {
    let (top, id, off) = ui_query_common(windows, a11y, sel).await?;
    let (nodes, truncated) = a11y_walk(a11y, &top, depth, all).await?;
    Ok(ui_reply(id, off, truncated, nodes))
}

pub(crate) async fn ui_find(
    windows: &Windows,
    a11y: &zbus::Connection,
    sel: &str,
    pattern: &str,
    role: &str,
) -> Result<UiTree> {
    let (top, id, off) = ui_query_common(windows, a11y, sel).await?;
    // Search everything, including popped-down menus — knowing a hidden
    // element exists is the point of searching.
    let (nodes, truncated) = a11y_walk(a11y, &top, UI_DEFAULT_DEPTH, true).await?;

    let needle = pattern.to_lowercase();
    let role_needle = role.to_lowercase();
    let hits: Vec<UiNode> = nodes
        .into_iter()
        .filter(|n| n.name.to_lowercase().contains(&needle))
        .filter(|n| role_needle.is_empty() || n.role.to_lowercase() == role_needle)
        .collect();

    if hits.is_empty() {
        return Err(anyhow!(
            "no UI element matched name {pattern:?}{} in window {id}{} — run \
             `ui-tree {id}` to see what the app exposes",
            if role.is_empty() {
                String::new()
            } else {
                format!(" with role {role:?}")
            },
            if truncated {
                " (walk truncated — the tree is very large)"
            } else {
                ""
            },
        ));
    }
    Ok(ui_reply(id, off, truncated, hits))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_names_match_whole_names_only() {
        assert!(app_name_matches("86Box", "86box"));
        assert!(app_name_matches("Firefox", "firefox"));
        assert!(app_name_matches("text-editor", "org.gnome.texteditor"));
        assert!(!app_name_matches("Box", "86box"));
        assert!(!app_name_matches("gnome", "org.gnome.texteditor"));
        assert!(!app_name_matches("", "firefox"));
    }

    async fn buses() -> (Windows, zbus::Connection) {
        let bus = zbus::Connection::session().await.unwrap();
        let a11y = a11y_connection(&bus).await.unwrap();
        let wayland = crate::wayland::Wayland::connect().unwrap();
        (Windows::detect(&bus, &wayland).await, a11y)
    }

    /// Live end-to-end test against the desktop's window provider and
    /// the accessibility bus — run explicitly with
    /// `cargo test ui_tree_live -- --ignored --nocapture`.
    /// Needs at least one AT-SPI-exposing app with an open window.
    #[tokio::test]
    #[ignore]
    async fn ui_tree_live() {
        let (windows, a11y) = buses().await;
        let tree = ui_tree(&windows, &a11y, "86box", 6, false).await.unwrap();
        println!("{tree:#?}");
        assert!(!tree.nodes.is_empty(), "expected at least one tree node");
    }

    #[tokio::test]
    #[ignore]
    async fn ui_find_live() {
        let (windows, a11y) = buses().await;
        let hits = ui_find(&windows, &a11y, "86box", "", "").await.unwrap();
        println!("{hits:#?}");
    }
}
