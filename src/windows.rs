//! Windows: listing them, focusing one, and finding the one a selector
//! names. Every desktop has its own way to do this, and no Wayland
//! protocol gives a window's geometry, so there's a provider per desktop:
//! the gnome-shell extension on GNOME, a KWin script on KDE Plasma, the
//! compositor's IPC socket on sway, niri and Hyprland, and COSMIC's
//! toplevel-info and toplevel-management on COSMIC. Elsewhere,
//! wlr-foreign-toplevel-management or ext-foreign-toplevel-list lists
//! windows, without geometry. Each reports a window as a dict with the
//! same fields (see `windows` in the CLI help), leaving out what its
//! desktop doesn't know.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use zvariant::{OwnedValue, Value};

use crate::extension::{ext_call, ext_proxy};
use crate::protocol::{WindowDict, render_value};
use crate::wayland::Wayland;

/// Where window information comes from.
pub(crate) enum Windows {
    /// The gnome-shell extension, on the session bus.
    Gnome(zbus::Connection),
    /// sway's IPC socket.
    Sway(PathBuf),
    /// niri's IPC socket.
    Niri(PathBuf),
    /// Hyprland's IPC socket.
    Hyprland(PathBuf),
    /// KWin, through a script (see `kwin`).
    Kwin(crate::kwin::Kwin),
    /// wlr-foreign-toplevel-management (titles, app ids, states, focus) or
    /// ext-foreign-toplevel-list (titles and app ids only): no geometry,
    /// unless COSMIC's toplevel-info and toplevel-management add it
    /// (with state and focus) to the ext handles.
    Toplevels(Arc<Wayland>),
    /// No provider: why.
    Unavailable(String),
}

impl Windows {
    /// Find the provider for the compositor the daemon's Wayland
    /// connection reaches.
    pub(crate) async fn detect(bus: &zbus::Connection, wayland: &Arc<Wayland>) -> Windows {
        let compositor = wayland.compositor_pid();
        // The compositor sets these for the programs it starts.
        if let Some(sock) = env_socket("SWAYSOCK") {
            return Windows::Sway(sock);
        }
        if let Some(sock) = env_socket("NIRI_SOCKET") {
            return Windows::Niri(sock);
        }
        if let Some(sock) = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")
            .and_then(|sig| hyprland_socket(Path::new(&sig)))
        {
            return Windows::Hyprland(sock);
        }
        // Otherwise (a systemd unit the variables didn't reach), the IPC
        // socket our compositor made: both name theirs with their pid.
        if let Some(pid) = compositor {
            if let Some(sock) = runtime_socket(|n| {
                n.starts_with("sway-ipc.") && n.ends_with(&format!(".{pid}.sock"))
            }) {
                return Windows::Sway(sock);
            }
            if let Some(sock) =
                runtime_socket(|n| n.starts_with("niri.") && n.ends_with(&format!(".{pid}.sock")))
            {
                return Windows::Niri(sock);
            }
            if let Some(sock) = hyprland_socket_for(pid) {
                return Windows::Hyprland(sock);
            }
        }
        // gnome-shell, if it's the compositor (not just on the bus, as it
        // is for a compositor nested inside GNOME).
        if let Some(shell) = bus_name_pid(bus, "org.gnome.Shell").await
            && compositor.is_none_or(|pid| same_compositor(pid, shell))
        {
            return Windows::Gnome(bus.clone());
        }
        if let Some(kwin) = bus_name_pid(bus, "org.kde.KWin").await
            && compositor.is_none_or(|pid| same_compositor(pid, kwin))
        {
            return match crate::kwin::Kwin::new(bus).await {
                Ok(kwin) => Windows::Kwin(kwin),
                Err(e) => Windows::Unavailable(format!("{e:#}")),
            };
        }
        let g = wayland.globals();
        if g.foreign_toplevel.is_some() || g.toplevel_list.is_some() {
            return Windows::Toplevels(wayland.clone());
        }
        Windows::Unavailable(
            "this desktop has no window provider (GNOME with the waytool extension, \
             KDE Plasma, sway, niri, Hyprland, or wlr-foreign-toplevel-management)"
                .into(),
        )
    }

    /// Where windows come from, for the log.
    pub(crate) fn describe(&self) -> String {
        match self {
            Windows::Gnome(_) => "the gnome-shell extension".into(),
            Windows::Sway(sock) => format!("sway ({})", sock.display()),
            Windows::Niri(sock) => format!("niri ({})", sock.display()),
            Windows::Hyprland(sock) => format!("Hyprland ({})", sock.display()),
            Windows::Toplevels(w) if w.globals().foreign_toplevel.is_some() => {
                "wlr-foreign-toplevel-management".into()
            }
            Windows::Toplevels(w) if w.globals().cosmic_info.is_some() => {
                "ext-foreign-toplevel-list with COSMIC's toplevel info".into()
            }
            Windows::Toplevels(_) => "ext-foreign-toplevel-list".into(),
            Windows::Kwin(_) => "KWin".into(),
            Windows::Unavailable(why) => format!("nowhere: {why}"),
        }
    }

    /// Every top-level window.
    pub(crate) async fn list(&self) -> Result<Vec<WindowDict>> {
        match self {
            Windows::Gnome(bus) => ext_call(&ext_proxy(bus).await?, "GetWindows", &()).await,
            Windows::Sway(sock) => sway_windows(sock).await,
            Windows::Niri(sock) => niri_windows(sock).await,
            Windows::Hyprland(sock) => hyprland_windows(sock).await,
            Windows::Toplevels(wayland) => {
                wayland.toplevels().iter().map(toplevel_window).collect()
            }
            Windows::Kwin(kwin) => {
                let reply = kwin.call(serde_json::json!({"op": "list"})).await?;
                reply["windows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(kwin_window)
                    .collect()
            }
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
            Windows::Sway(sock) => sway_command(sock, &format!("[con_id={id}] focus")).await,
            Windows::Niri(sock) => {
                let request = serde_json::json!({"Action": {"FocusWindow": {"id": id}}});
                niri_request(sock, &request).await.map(|_| ())
            }
            Windows::Kwin(kwin) => kwin
                .call(serde_json::json!({"op": "focus", "id": format!("{id:016x}")}))
                .await
                .map(|_| ()),
            Windows::Toplevels(wayland) => {
                let toplevel = wayland
                    .toplevels()
                    .into_iter()
                    .find(|t| t.id == id)
                    .ok_or_else(|| anyhow!("no window with id {id}"))?;
                let seat = wayland
                    .globals()
                    .seat
                    .as_ref()
                    .ok_or_else(|| anyhow!("the compositor has no seat"))?;
                match (
                    &toplevel.handle,
                    &toplevel.cosmic,
                    &wayland.globals().cosmic_manager,
                ) {
                    (Some(handle), _, _) => handle.activate(seat),
                    (None, Some(cosmic), Some(manager)) => manager.activate(cosmic, seat),
                    _ => {
                        return Err(anyhow!(
                            "this compositor offers no way to focus another window \
                             (ext-foreign-toplevel-list only lists them)"
                        ));
                    }
                }
                wayland.flush()
            }
            Windows::Hyprland(sock) => {
                let reply =
                    hyprland_request(sock, &format!("dispatch focuswindow address:0x{id:x}"))
                        .await?;
                if reply.trim() != "ok" {
                    return Err(anyhow!("Hyprland: {}", reply.trim()));
                }
                Ok(())
            }
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

/// A window dict from its fields.
fn dict(entries: Vec<(&str, Value)>) -> Result<WindowDict> {
    entries
        .into_iter()
        .map(|(k, v)| Ok((k.to_owned(), OwnedValue::try_from(v)?)))
        .collect()
}

/// The socket an environment variable names, if it exists.
fn env_socket(var: &str) -> Option<PathBuf> {
    let sock = PathBuf::from(std::env::var_os(var)?);
    sock.exists().then_some(sock)
}

/// The socket in XDG_RUNTIME_DIR whose name `matches`.
fn runtime_socket(matches: impl Fn(&str) -> bool) -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(&matches))
}

/// Whether `owner`, a bus name's process, is the compositor whose
/// Wayland socket `socket` (the pid holding it) belongs to: the same
/// process, or one it started — Plasma's kwin_wayland_wrapper makes the
/// socket and runs kwin_wayland, which owns org.kde.KWin.
fn same_compositor(socket: u32, owner: u32) -> bool {
    let mut pid = owner;
    for _ in 0..4 {
        if pid == socket {
            return true;
        }
        match parent_pid(pid) {
            Some(parent) if parent > 1 => pid = parent,
            _ => return false,
        }
    }
    false
}

/// A process's parent, from /proc/<pid>/stat.
fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may hold spaces; the state
    // and the parent pid follow its closing parenthesis.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The pid of the process that owns `name` on the bus, if any.
async fn bus_name_pid(bus: &zbus::Connection, name: &str) -> Option<u32> {
    let dbus = zbus::fdo::DBusProxy::new(bus).await.ok()?;
    let name = zbus::names::BusName::try_from(name).ok()?;
    dbus.get_connection_unix_process_id(name).await.ok()
}

/* ----- KWin ----- */

/// A window as the KWin script describes it.
fn kwin_window(w: &serde_json::Value) -> Result<WindowDict> {
    let id = w["id"]
        .as_str()
        .and_then(|h| u64::from_str_radix(h, 16).ok())
        .ok_or_else(|| anyhow!("KWin window without an id"))?;
    let int = |v: &serde_json::Value| v.as_i64().unwrap_or(0) as i32;
    let (f, c) = (&w["frame"], &w["client"]);
    let text = |k: &str| w[k].as_str().unwrap_or("").to_owned();
    let flag = |k: &str| w[k].as_bool().unwrap_or(false);
    dict(vec![
        ("id", Value::U64(id)),
        ("title", text("title").into()),
        ("app_id", text("app_id").into()),
        ("wm_class", text("wm_class").into()),
        ("pid", Value::I32(int(&w["pid"]))),
        ("monitor", text("monitor").into()),
        ("frame_x", Value::I32(int(&f[0]))),
        ("frame_y", Value::I32(int(&f[1]))),
        ("frame_w", Value::I32(int(&f[2]))),
        ("frame_h", Value::I32(int(&f[3]))),
        ("client_x", Value::I32(int(&c[0]))),
        ("client_y", Value::I32(int(&c[1]))),
        ("client_w", Value::I32(int(&c[2]))),
        ("client_h", Value::I32(int(&c[3]))),
        ("focused", flag("focused").into()),
        ("minimized", flag("minimized").into()),
        ("maximized", flag("maximized").into()),
        ("fullscreen", flag("fullscreen").into()),
        ("on_all_workspaces", flag("on_all_workspaces").into()),
        ("workspace", text("workspace").into()),
        ("on_active_workspace", flag("on_active_workspace").into()),
    ])
}

/* ----- wlr-foreign-toplevel-management ----- */

fn toplevel_window(t: &crate::wayland::Toplevel) -> Result<WindowDict> {
    let mut fields = vec![
        ("id", Value::U64(t.id)),
        ("title", t.title.as_str().into()),
        ("app_id", t.app_id.as_str().into()),
        ("wm_class", t.app_id.as_str().into()),
        ("monitor", t.outputs.join("+").into()),
        ("focused", t.activated.into()),
        ("minimized", t.minimized.into()),
        ("maximized", t.maximized.into()),
        ("fullscreen", t.fullscreen.into()),
    ];
    // Where the compositor says (COSMIC); its geometry is the window's,
    // with no frame of its own.
    if let Some((x, y, w, h)) = t.rect {
        fields.extend([
            ("frame_x", Value::I32(x)),
            ("frame_y", Value::I32(y)),
            ("frame_w", Value::I32(w)),
            ("frame_h", Value::I32(h)),
            ("client_x", Value::I32(x)),
            ("client_y", Value::I32(y)),
            ("client_w", Value::I32(w)),
            ("client_h", Value::I32(h)),
        ]);
    }
    dict(fields)
}

/* ----- sway ----- */

/// One sway IPC exchange: a message of `kind` with `payload`, and the
/// reply's JSON (sway-ipc(7)).
async fn sway_ipc(sock: &Path, kind: u32, payload: &str) -> Result<serde_json::Value> {
    let mut stream = UnixStream::connect(sock)
        .await
        .with_context(|| format!("connect to sway at {}", sock.display()))?;
    let mut msg = b"i3-ipc".to_vec();
    msg.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    msg.extend_from_slice(&kind.to_le_bytes());
    msg.extend_from_slice(payload.as_bytes());
    stream.write_all(&msg).await.context("sway IPC write")?;
    let mut header = [0u8; 14];
    stream
        .read_exact(&mut header)
        .await
        .context("sway IPC read")?;
    if &header[..6] != b"i3-ipc" {
        return Err(anyhow!("sway IPC: bad reply header"));
    }
    let len = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
    let mut body = vec![0; len];
    stream
        .read_exact(&mut body)
        .await
        .context("sway IPC read")?;
    serde_json::from_slice(&body).context("sway IPC reply")
}

const SWAY_RUN_COMMAND: u32 = 0;
const SWAY_GET_TREE: u32 = 4;

async fn sway_command(sock: &Path, command: &str) -> Result<()> {
    let reply = sway_ipc(sock, SWAY_RUN_COMMAND, command).await?;
    let first = &reply[0];
    if first["success"].as_bool() != Some(true) {
        return Err(anyhow!(
            "sway: {}",
            first["error"].as_str().unwrap_or("the command failed")
        ));
    }
    Ok(())
}

async fn sway_windows(sock: &Path) -> Result<Vec<WindowDict>> {
    let tree = sway_ipc(sock, SWAY_GET_TREE, "").await?;
    let mut out = Vec::new();
    sway_collect(&tree, "", None, &mut out)?;
    Ok(out)
}

/// Walk sway's tree, collecting views (nodes with a pid) with the output
/// and workspace they're on.
fn sway_collect(
    node: &serde_json::Value,
    output: &str,
    workspace: Option<&serde_json::Value>,
    out: &mut Vec<WindowDict>,
) -> Result<()> {
    let (output, workspace) = match node["type"].as_str() {
        Some("output") => (node["name"].as_str().unwrap_or(output), workspace),
        Some("workspace") => (output, Some(node)),
        _ => (output, workspace),
    };
    if node["pid"].is_number() {
        out.push(sway_window(node, output, workspace)?);
    }
    for key in ["nodes", "floating_nodes"] {
        for child in node[key].as_array().into_iter().flatten() {
            sway_collect(child, output, workspace, out)?;
        }
    }
    Ok(())
}

fn sway_window(
    n: &serde_json::Value,
    output: &str,
    workspace: Option<&serde_json::Value>,
) -> Result<WindowDict> {
    let num = |v: &serde_json::Value| v.as_i64().unwrap_or(0) as i32;
    let rect = &n["rect"];
    let inner = &n["window_rect"];
    let is_x11 = n["shell"].as_str() == Some("xwayland");
    let app_id = n["app_id"].as_str().unwrap_or("").to_owned();
    let class = n["window_properties"]["class"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let ws_name = workspace.and_then(|w| w["name"].as_str()).unwrap_or("");
    dict(vec![
        ("id", Value::U64(n["id"].as_u64().unwrap_or(0))),
        ("title", n["name"].as_str().unwrap_or("").into()),
        ("app_id", app_id.clone().into()),
        ("wm_class", if is_x11 { class } else { app_id }.into()),
        ("pid", Value::I32(num(&n["pid"]))),
        ("monitor", output.into()),
        ("is_x11", is_x11.into()),
        ("frame_x", Value::I32(num(&rect["x"]))),
        ("frame_y", Value::I32(num(&rect["y"]))),
        ("frame_w", Value::I32(num(&rect["width"]))),
        ("frame_h", Value::I32(num(&rect["height"]))),
        ("client_x", Value::I32(num(&rect["x"]) + num(&inner["x"]))),
        ("client_y", Value::I32(num(&rect["y"]) + num(&inner["y"]))),
        ("client_w", Value::I32(num(&inner["width"]))),
        ("client_h", Value::I32(num(&inner["height"]))),
        ("focused", n["focused"].as_bool().unwrap_or(false).into()),
        // The scratchpad is sway's nearest thing to minimizing.
        ("minimized", (ws_name == "__i3_scratch").into()),
        (
            "fullscreen",
            (n["fullscreen_mode"].as_i64().unwrap_or(0) != 0).into(),
        ),
        (
            "on_all_workspaces",
            n["sticky"].as_bool().unwrap_or(false).into(),
        ),
        ("workspace", ws_name.into()),
        (
            "on_active_workspace",
            n["visible"].as_bool().unwrap_or(false).into(),
        ),
    ])
}

/* ----- niri ----- */

/// One niri IPC request (niri-ipc: a JSON line each way), returning the
/// reply's `Ok` value.
async fn niri_request(sock: &Path, request: &serde_json::Value) -> Result<serde_json::Value> {
    let stream = UnixStream::connect(sock)
        .await
        .with_context(|| format!("connect to niri at {}", sock.display()))?;
    let (read, mut write) = stream.into_split();
    let mut line = request.to_string();
    line.push('\n');
    write
        .write_all(line.as_bytes())
        .await
        .context("niri IPC write")?;
    let mut reply = String::new();
    tokio::io::AsyncBufReadExt::read_line(&mut tokio::io::BufReader::new(read), &mut reply)
        .await
        .context("niri IPC read")?;
    let mut reply: serde_json::Value = serde_json::from_str(&reply).context("niri IPC reply")?;
    match reply.get_mut("Ok") {
        Some(ok) => Ok(ok.take()),
        None => Err(anyhow!(
            "niri: {}",
            reply["Err"].as_str().unwrap_or("request failed")
        )),
    }
}

async fn niri_windows(sock: &Path) -> Result<Vec<WindowDict>> {
    let request = |name: &str| serde_json::Value::String(name.into());
    let windows = niri_request(sock, &request("Windows")).await?;
    let workspaces = niri_request(sock, &request("Workspaces")).await?;
    let outputs = niri_request(sock, &request("Outputs")).await?;
    let workspace = |id: &serde_json::Value| {
        workspaces["Workspaces"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|w| &w["id"] == id)
    };
    windows["Windows"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|w| {
            let ws = workspace(&w["workspace_id"]);
            let output = ws.and_then(|ws| ws["output"].as_str()).unwrap_or("");
            let mut fields = vec![
                ("id", Value::U64(w["id"].as_u64().unwrap_or(0))),
                ("title", w["title"].as_str().unwrap_or("").into()),
                ("app_id", w["app_id"].as_str().unwrap_or("").into()),
                ("wm_class", w["app_id"].as_str().unwrap_or("").into()),
                ("pid", Value::I32(w["pid"].as_i64().unwrap_or(0) as i32)),
                ("monitor", output.into()),
                ("focused", w["is_focused"].as_bool().unwrap_or(false).into()),
                (
                    "workspace",
                    Value::I32(ws.and_then(|ws| ws["idx"].as_i64()).unwrap_or(0) as i32),
                ),
                (
                    "on_active_workspace",
                    ws.and_then(|ws| ws["is_active"].as_bool())
                        .unwrap_or(false)
                        .into(),
                ),
            ];
            // niri gives a position only for floating windows: the
            // tile's place in the workspace view, which sits at its
            // output's logical origin.
            let layout = &w["layout"];
            let origin = &outputs["Outputs"][output]["logical"];
            if let (Some(tile), Some(offset), Some(size)) = (
                layout["tile_pos_in_workspace_view"].as_array(),
                layout["window_offset_in_tile"].as_array(),
                layout["window_size"].as_array(),
            ) {
                let f = |v: &serde_json::Value| v.as_f64().unwrap_or(0.0);
                let x = (f(&origin["x"]) + f(&tile[0]) + f(&offset[0])) as i32;
                let y = (f(&origin["y"]) + f(&tile[1]) + f(&offset[1])) as i32;
                let (w, h) = (f(&size[0]) as i32, f(&size[1]) as i32);
                fields.extend([
                    ("frame_x", Value::I32(x)),
                    ("frame_y", Value::I32(y)),
                    ("frame_w", Value::I32(w)),
                    ("frame_h", Value::I32(h)),
                    ("client_x", Value::I32(x)),
                    ("client_y", Value::I32(y)),
                    ("client_w", Value::I32(w)),
                    ("client_h", Value::I32(h)),
                ]);
            }
            dict(fields)
        })
        .collect()
}

/* ----- Hyprland ----- */

/// The IPC socket of the Hyprland instance `signature`
/// ($XDG_RUNTIME_DIR/hypr/<signature>/.socket.sock).
fn hyprland_socket(signature: &Path) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("hypr");
    let sock = dir.join(signature).join(".socket.sock");
    sock.exists().then_some(sock)
}

/// The IPC socket of the Hyprland instance with process id `pid`: each
/// instance's directory has a hyprland.lock whose first line is its pid.
fn hyprland_socket_for(pid: u32) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("hypr");
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|instance| {
            std::fs::read_to_string(instance.join("hyprland.lock"))
                .is_ok_and(|lock| lock.lines().next() == Some(&pid.to_string()))
        })
        .map(|instance| instance.join(".socket.sock"))
        .filter(|sock| sock.exists())
}

/// One Hyprland IPC request (hyprctl's protocol: the request, then the
/// reply until the socket closes).
async fn hyprland_request(sock: &Path, request: &str) -> Result<String> {
    let mut stream = UnixStream::connect(sock)
        .await
        .with_context(|| format!("connect to Hyprland at {}", sock.display()))?;
    stream
        .write_all(request.as_bytes())
        .await
        .context("Hyprland IPC write")?;
    let mut reply = String::new();
    stream
        .read_to_string(&mut reply)
        .await
        .context("Hyprland IPC read")?;
    Ok(reply)
}

async fn hyprland_windows(sock: &Path) -> Result<Vec<WindowDict>> {
    let clients = hyprland_request(sock, "j/clients").await?;
    let monitors = hyprland_request(sock, "j/monitors").await?;
    let active = hyprland_request(sock, "j/activewindow").await?;
    hyprland_parse(&clients, &monitors, &active)
}

/// `active` is the `activewindow` reply: the window with keyboard
/// focus, or `{}` with none. (A client's `focusHistoryID` stays 0 when
/// focus moves to an empty workspace, so it can't say.)
fn hyprland_parse(clients: &str, monitors: &str, active: &str) -> Result<Vec<WindowDict>> {
    let clients: serde_json::Value =
        serde_json::from_str(clients).context("Hyprland clients reply")?;
    let monitors: serde_json::Value =
        serde_json::from_str(monitors).context("Hyprland monitors reply")?;
    let monitors = monitors.as_array().cloned().unwrap_or_default();
    let active: serde_json::Value =
        serde_json::from_str(active).context("Hyprland activewindow reply")?;
    let focused = active["address"].as_str();
    let int = |v: &serde_json::Value| v.as_i64().unwrap_or(0) as i32;
    clients
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["mapped"].as_bool() != Some(false))
        .map(|c| {
            let id = c["address"]
                .as_str()
                .and_then(|a| u64::from_str_radix(a.trim_start_matches("0x"), 16).ok())
                .unwrap_or(0);
            let monitor = monitors.iter().find(|m| m["id"] == c["monitor"]);
            // A window's workspace is showing if it's some monitor's
            // active one.
            let ws = &c["workspace"];
            let active = monitors.iter().any(|m| {
                let a = &m["activeWorkspace"];
                if ws["id"].is_number() {
                    a["id"] == ws["id"]
                } else {
                    a["name"] == ws["name"]
                }
            });
            // fullscreen is a mode number in newer releases, a bool in
            // older ones.
            let fullscreen = c["fullscreen"]
                .as_bool()
                .unwrap_or(c["fullscreen"].as_i64().unwrap_or(0) != 0);
            let class = c["class"].as_str().unwrap_or("");
            let (x, y) = (int(&c["at"][0]), int(&c["at"][1]));
            let (w, h) = (int(&c["size"][0]), int(&c["size"][1]));
            dict(vec![
                ("id", Value::U64(id)),
                ("title", c["title"].as_str().unwrap_or("").into()),
                ("app_id", class.into()),
                ("wm_class", class.into()),
                ("pid", Value::I32(int(&c["pid"]))),
                (
                    "monitor",
                    monitor
                        .and_then(|m| m["name"].as_str())
                        .unwrap_or("")
                        .into(),
                ),
                ("is_x11", c["xwayland"].as_bool().unwrap_or(false).into()),
                ("frame_x", Value::I32(x)),
                ("frame_y", Value::I32(y)),
                ("frame_w", Value::I32(w)),
                ("frame_h", Value::I32(h)),
                ("client_x", Value::I32(x)),
                ("client_y", Value::I32(y)),
                ("client_w", Value::I32(w)),
                ("client_h", Value::I32(h)),
                (
                    "focused",
                    (focused.is_some() && c["address"].as_str() == focused).into(),
                ),
                ("minimized", c["hidden"].as_bool().unwrap_or(false).into()),
                ("fullscreen", fullscreen.into()),
                (
                    "on_all_workspaces",
                    c["pinned"].as_bool().unwrap_or(false).into(),
                ),
                ("workspace", ws["name"].as_str().unwrap_or("").into()),
                ("on_active_workspace", active.into()),
            ])
        })
        .collect()
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

    /// Live: list windows through this session's provider, and focus the
    /// first that isn't focused.
    /// `cargo test -- --ignored windows_live --nocapture`
    #[tokio::test]
    #[ignore]
    async fn windows_live() {
        let bus = zbus::Connection::session().await.unwrap();
        let wayland = Wayland::connect().unwrap();
        let windows = Windows::detect(&bus, &wayland).await;
        println!("provider: {}", windows.describe());
        let list = windows.list().await.unwrap();
        println!("{}", summarize_windows(&list));
        if let Some(w) = list.iter().find(|w| !dict_bool(w, "focused")) {
            let id = dict_i64(w, "id").unwrap() as u64;
            windows.focus(id).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            assert!(dict_bool(&windows.get(id).await.unwrap(), "focused"));
            println!("focused {id}");
        }
    }

    #[test]
    fn a_compositor_is_its_own_or_its_parents() {
        let me = std::process::id();
        let parent = parent_pid(me).unwrap();
        assert!(same_compositor(me, me));
        assert!(same_compositor(parent, me));
        assert!(!same_compositor(me, parent));
    }

    #[test]
    fn hyprland_clients_become_window_dicts() {
        let clients = r#"[{
            "address": "0x5581f2c0a8a0", "mapped": true, "hidden": false,
            "at": [10, 40], "size": [800, 600],
            "workspace": {"id": 2, "name": "2"}, "floating": false, "monitor": 1,
            "class": "foot", "title": "~", "pid": 4242, "xwayland": false,
            "pinned": false, "fullscreen": 0, "focusHistoryID": 0
        }, {
            "address": "0x10", "mapped": false, "at": [0, 0], "size": [1, 1],
            "workspace": {"id": 3, "name": "3"}, "monitor": 0, "class": "x",
            "title": "", "pid": 1, "focusHistoryID": 4
        }]"#;
        let monitors = r#"[{"id": 0, "name": "DP-1", "activeWorkspace": {"id": 1, "name": "1"}},
                           {"id": 1, "name": "DP-2", "activeWorkspace": {"id": 2, "name": "2"}}]"#;
        let windows =
            hyprland_parse(clients, monitors, r#"{"address": "0x5581f2c0a8a0"}"#).unwrap();
        assert_eq!(windows.len(), 1, "unmapped windows are left out");
        let w = &windows[0];
        assert_eq!(dict_i64(w, "id").unwrap(), 0x5581f2c0a8a0);
        assert_eq!(dict_i64(w, "frame_x").unwrap(), 10);
        assert!(dict_bool(w, "focused"));
        assert!(dict_bool(w, "on_active_workspace"));
        assert_eq!(render_value(&w["monitor"]), "DP-2");
        assert!(window_matches(w, "foot"));

        // Focus on an empty workspace: no window has it, though foot's
        // focusHistoryID is still 0.
        let windows = hyprland_parse(clients, monitors, "{}").unwrap();
        assert!(!dict_bool(&windows[0], "focused"));
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
