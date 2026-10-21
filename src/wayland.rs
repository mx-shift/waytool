//! The daemon's own Wayland connection: the globals it binds, the
//! monitor layout, and the seat's keyboard layout.
//!
//! The layout comes from wl_output (each output's name and mode) and
//! xdg-output (its rect in the compositor's logical layout), which
//! every desktop compositor implements, so `monitors` and display-change
//! tracking work the same everywhere. The keyboard layout is the keymap
//! the compositor sends a client's wl_keyboard. The input and capture
//! managers are what the Wayland backend (`wlr_input`, `wlr_capture`)
//! uses where there's no RemoteDesktop portal.
//!
//! Events are read on a thread of its own; requests can be made from any
//! thread through the proxies here (wayland-client is thread-safe), and
//! are sent with `flush`.

use std::os::fd::AsFd;

use cosmic_protocols::toplevel_info::v1::client::{
    zcosmic_toplevel_handle_v1::{self, ZcosmicToplevelHandleV1},
    zcosmic_toplevel_info_v1::{self, ZcosmicToplevelInfoV1},
};
use cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1::{
    self, ZcosmicToplevelManagerV1,
};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_registry, wl_seat, wl_shm};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;

/// One output (monitor), as the compositor lays it out.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Monitor {
    /// The connector name, such as DP-1 (wl_output v4, xdg-output v2).
    pub name: String,
    pub description: String,
    /// Rect in global logical coordinates, the space window rects are
    /// reported in.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Physical pixels per logical pixel: the current mode's size (as
    /// displayed, after rotation) over the logical size. A screenshot of
    /// the monitor is its logical size times this.
    pub scale: f64,
    /// How the output turns its framebuffer for display.
    pub transform: wl_output::Transform,
}

/// The globals the Wayland backend can use, where the compositor offers
/// them.
#[derive(Clone, Default)]
pub(crate) struct Globals {
    pub seat: Option<wl_seat::WlSeat>,
    pub shm: Option<wl_shm::WlShm>,
    pub virtual_keyboard: Option<ZwpVirtualKeyboardManagerV1>,
    pub virtual_pointer: Option<ZwlrVirtualPointerManagerV1>,
    pub image_copy: Option<ExtImageCopyCaptureManagerV1>,
    pub output_source: Option<ExtOutputImageCaptureSourceManagerV1>,
    pub screencopy: Option<ZwlrScreencopyManagerV1>,
    pub foreign_toplevel: Option<ZwlrForeignToplevelManagerV1>,
    pub toplevel_list: Option<ExtForeignToplevelListV1>,
    /// COSMIC's extensions to the ext list: state and geometry (version
    /// 2 and up), and activation.
    pub cosmic_info: Option<ZcosmicToplevelInfoV1>,
    pub cosmic_manager: Option<ZcosmicToplevelManagerV1>,
}

/// A window as wlr-foreign-toplevel-management or ext-foreign-toplevel-
/// list reports it: no geometry, and no id of its own, so it gets one
/// here.
#[derive(Clone)]
pub(crate) struct Toplevel {
    pub id: u64,
    /// For activating it: wlr-foreign-toplevel-management's handle, or
    /// COSMIC's (the ext list alone just lists).
    pub handle: Option<ZwlrForeignToplevelHandleV1>,
    pub cosmic: Option<ZcosmicToplevelHandleV1>,
    /// Frame rect in global logical coordinates, where the compositor
    /// says (COSMIC).
    pub rect: Option<(i32, i32, i32, i32)>,
    pub title: String,
    pub app_id: String,
    /// Connector names of the outputs it's on.
    pub outputs: Vec<String>,
    pub maximized: bool,
    pub minimized: bool,
    pub activated: bool,
    pub fullscreen: bool,
}

/// What the event thread keeps current for the rest of the daemon.
#[derive(Default)]
struct Shared {
    /// The monitors, each with its wl_output.
    monitors: Vec<(Monitor, wl_output::WlOutput)>,
    /// The seat keyboard's keymap text, when it has one.
    keymap: Option<String>,
    /// Windows, where the compositor offers wlr-foreign-toplevel.
    toplevels: Vec<Toplevel>,
    /// Windows from ext-foreign-toplevel-list, for compositors without it.
    listed: Vec<Toplevel>,
    lost: bool,
}

/// The connection.
pub(crate) struct Wayland {
    conn: Connection,
    qh: QueueHandle<State>,
    globals: Globals,
    shared: Arc<Mutex<Shared>>,
    changes: tokio::sync::watch::Receiver<u64>,
    keymaps: tokio::sync::watch::Receiver<u64>,
}

impl Wayland {
    /// Connect to the compositor (WAYLAND_DISPLAY), bind the globals and
    /// read the layout.
    pub(crate) fn connect() -> Result<Arc<Wayland>> {
        let conn = Connection::connect_to_env()
            .context("connect to the Wayland compositor (is WAYLAND_DISPLAY set?)")?;
        let mut queue: EventQueue<State> = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (changes_tx, changes) = tokio::sync::watch::channel(0u64);
        let (keymaps_tx, keymaps) = tokio::sync::watch::channel(0u64);
        let mut state = State {
            shared: shared.clone(),
            changes: changes_tx,
            keymaps: keymaps_tx,
            ..State::default()
        };
        // Globals; then the outputs', xdg-outputs' and keyboard's first
        // events.
        queue.roundtrip(&mut state).context("Wayland roundtrip")?;
        queue.roundtrip(&mut state).context("Wayland roundtrip")?;
        queue.roundtrip(&mut state).context("Wayland roundtrip")?;
        if state.xdg_manager.is_none() {
            return Err(anyhow!(
                "the compositor doesn't offer xdg-output (zxdg_output_manager_v1)"
            ));
        }
        state.publish();
        let globals = state.globals.clone();

        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("wayland".into())
            .spawn(move || {
                loop {
                    if let Err(e) = queue.blocking_dispatch(&mut state) {
                        eprintln!("waytool: Wayland connection lost: {e}");
                        thread_shared.lock().unwrap().lost = true;
                        // Wake anything waiting on a change.
                        state.changes.send_modify(|g| *g += 1);
                        return;
                    }
                    state.publish();
                }
            })
            .context("start the Wayland thread")?;
        Ok(Arc::new(Wayland {
            conn,
            qh,
            globals,
            shared,
            changes,
            keymaps,
        }))
    }

    /// The current layout, in the compositor's announcement order.
    pub(crate) fn monitors(&self) -> Vec<Monitor> {
        let shared = self.shared.lock().unwrap();
        shared.monitors.iter().map(|(m, _)| m.clone()).collect()
    }

    /// The monitors with their wl_outputs.
    pub(crate) fn outputs(&self) -> Vec<(Monitor, wl_output::WlOutput)> {
        self.shared.lock().unwrap().monitors.clone()
    }

    /// A receiver that changes whenever the layout does, or the
    /// connection is lost.
    pub(crate) fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.clone()
    }

    /// The seat keyboard's keymap, and a receiver that changes with it.
    pub(crate) fn keymap(&self) -> (Option<String>, tokio::sync::watch::Receiver<u64>) {
        (
            self.shared.lock().unwrap().keymap.clone(),
            self.keymaps.clone(),
        )
    }

    /// The compositor's process id, from the Wayland socket's peer
    /// credentials: which compositor we're actually talking to, when
    /// several run (one nested in another).
    pub(crate) fn compositor_pid(&self) -> Option<u32> {
        use std::os::fd::AsRawFd;
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: SO_PEERCRED fills a ucred of `len` bytes on a valid
        // socket fd, which the connection keeps open.
        let rc = unsafe {
            libc::getsockopt(
                self.conn.as_fd().as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
    }

    /// The windows wlr-foreign-toplevel-management reports, or failing
    /// that ext-foreign-toplevel-list.
    pub(crate) fn toplevels(&self) -> Vec<Toplevel> {
        let shared = self.shared.lock().unwrap();
        if self.globals.foreign_toplevel.is_some() {
            shared.toplevels.clone()
        } else {
            shared.listed.clone()
        }
    }

    /// Whether the connection has been lost.
    pub(crate) fn is_lost(&self) -> bool {
        self.shared.lock().unwrap().lost
    }

    pub(crate) fn globals(&self) -> &Globals {
        &self.globals
    }

    /// For creating objects whose events this connection's thread reads.
    pub(crate) fn qh(&self) -> &QueueHandle<State> {
        &self.qh
    }

    /// Send the requests made so far.
    pub(crate) fn flush(&self) -> Result<()> {
        self.conn.flush().context("Wayland flush")
    }
}

/// What COSMIC adds to a listed toplevel, before it's mapped to names
/// and global coordinates.
#[derive(Default)]
struct Listed {
    outputs: Vec<wl_output::WlOutput>,
    /// Position and size relative to an output.
    geometry: Option<(wl_output::WlOutput, i32, i32, i32, i32)>,
}

#[derive(Default)]
struct Output {
    /// The registry name of its wl_output global.
    global: u32,
    wl: Option<wl_output::WlOutput>,
    xdg: Option<zxdg_output_v1::ZxdgOutputV1>,
    name: String,
    description: String,
    mode: Option<(i32, i32)>,
    transform: Option<wl_output::Transform>,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
}

/// The event thread's state. Other modules implement `Dispatch` on it
/// for the objects they create.
#[derive(Default)]
pub(crate) struct State {
    outputs: Vec<Output>,
    xdg_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    globals: Globals,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    shared: Arc<Mutex<Shared>>,
    outputs_changed: bool,
    /// Toplevels by handle, with their state so far; `done` applies it.
    toplevels: Vec<(
        ZwlrForeignToplevelHandleV1,
        Toplevel,
        Vec<wl_output::WlOutput>,
    )>,
    next_toplevel: u64,
    toplevels_changed: bool,
    /// ext-foreign-toplevel-list's windows by handle, with (from COSMIC)
    /// the outputs each is on and its geometry relative to one.
    listed: Vec<(ExtForeignToplevelHandleV1, Toplevel, Listed)>,
    listed_changed: bool,
    keymap_changed: Option<String>,
    changes: tokio::sync::watch::Sender<u64>,
    keymaps: tokio::sync::watch::Sender<u64>,
}

impl State {
    fn monitors(&self) -> Vec<(Monitor, wl_output::WlOutput)> {
        self.outputs
            .iter()
            .filter_map(|o| {
                let (x, y) = o.position?;
                let (width, height) = o.size?;
                let scale = match o.mode {
                    Some((w, h)) if width > 0 => {
                        // A quarter turn displays the mode's width as height.
                        use wl_output::Transform as T;
                        let rotated = matches!(
                            o.transform,
                            Some(T::_90 | T::_270 | T::Flipped90 | T::Flipped270)
                        );
                        let shown = if rotated { h } else { w };
                        shown as f64 / width as f64
                    }
                    _ => 1.0,
                };
                let monitor = Monitor {
                    name: o.name.clone(),
                    description: o.description.clone(),
                    x,
                    y,
                    width,
                    height,
                    scale,
                    transform: o.transform.unwrap_or(wl_output::Transform::Normal),
                };
                Some((monitor, o.wl.clone()?))
            })
            .collect()
    }

    /// Hand what changed to the rest of the daemon.
    fn publish(&mut self) {
        if std::mem::take(&mut self.outputs_changed) {
            let now = self.monitors();
            let mut shared = self.shared.lock().unwrap();
            let same = shared.monitors.len() == now.len()
                && shared.monitors.iter().zip(&now).all(|(a, b)| a.0 == b.0);
            shared.monitors = now;
            drop(shared);
            if !same {
                self.changes.send_modify(|g| *g += 1);
            }
        }
        if std::mem::take(&mut self.toplevels_changed) {
            let names = |outputs: &[wl_output::WlOutput]| -> Vec<String> {
                outputs
                    .iter()
                    .filter_map(|wl| {
                        self.outputs
                            .iter()
                            .find(|o| o.wl.as_ref() == Some(wl))
                            .map(|o| o.name.clone())
                    })
                    .collect()
            };
            let toplevels = self
                .toplevels
                .iter()
                .map(|(_, t, outputs)| Toplevel {
                    outputs: names(outputs),
                    ..t.clone()
                })
                .collect();
            self.shared.lock().unwrap().toplevels = toplevels;
        }
        if std::mem::take(&mut self.listed_changed) {
            let output =
                |wl: &wl_output::WlOutput| self.outputs.iter().find(|o| o.wl.as_ref() == Some(wl));
            let listed = self
                .listed
                .iter()
                .map(|(_, t, extra)| Toplevel {
                    outputs: extra
                        .outputs
                        .iter()
                        .filter_map(|wl| output(wl).map(|o| o.name.clone()))
                        .collect(),
                    // COSMIC gives the geometry relative to an output, in
                    // logical coordinates: offset it by the output's place
                    // in the layout.
                    rect: extra.geometry.as_ref().and_then(|(wl, x, y, w, h)| {
                        let (ox, oy) = output(wl)?.position?;
                        Some((ox + x, oy + y, *w, *h))
                    }),
                    ..t.clone()
                })
                .collect();
            self.shared.lock().unwrap().listed = listed;
        }
        if let Some(keymap) = self.keymap_changed.take() {
            self.shared.lock().unwrap().keymap = Some(keymap);
            self.keymaps.send_modify(|g| *g += 1);
        }
    }

    fn output(&mut self, global: u32) -> Option<&mut Output> {
        self.outputs.iter_mut().find(|o| o.global == global)
    }

    /// Give every output an xdg-output, once both are bound.
    fn attach_xdg(&mut self, qh: &QueueHandle<State>) {
        let Some(manager) = &self.xdg_manager else {
            return;
        };
        for o in &mut self.outputs {
            if let (None, Some(wl)) = (&o.xdg, &o.wl) {
                o.xdg = Some(manager.get_xdg_output(wl, qh, o.global));
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                let g = &mut state.globals;
                match interface.as_str() {
                    "wl_output" => {
                        let wl = registry.bind::<wl_output::WlOutput, _, _>(
                            name,
                            version.min(4),
                            qh,
                            name,
                        );
                        state.outputs.push(Output {
                            global: name,
                            wl: Some(wl),
                            ..Output::default()
                        });
                        state.attach_xdg(qh);
                    }
                    "zxdg_output_manager_v1" => {
                        state.xdg_manager = Some(registry.bind(name, version.min(3), qh, ()));
                        state.attach_xdg(qh);
                    }
                    // The first seat: there's one on every desktop.
                    "wl_seat" if g.seat.is_none() => {
                        g.seat = Some(registry.bind(name, version.min(5), qh, ()));
                    }
                    "wl_shm" => g.shm = Some(registry.bind(name, 1, qh, ())),
                    "zwp_virtual_keyboard_manager_v1" => {
                        g.virtual_keyboard = Some(registry.bind(name, 1, qh, ()))
                    }
                    "zwlr_virtual_pointer_manager_v1" => {
                        g.virtual_pointer = Some(registry.bind(name, version.min(2), qh, ()))
                    }
                    "ext_image_copy_capture_manager_v1" => {
                        g.image_copy = Some(registry.bind(name, 1, qh, ()))
                    }
                    "ext_output_image_capture_source_manager_v1" => {
                        g.output_source = Some(registry.bind(name, 1, qh, ()))
                    }
                    "zwlr_screencopy_manager_v1" => {
                        g.screencopy = Some(registry.bind(name, version.min(3), qh, ()))
                    }
                    "zcosmic_toplevel_info_v1" if version >= 2 => {
                        g.cosmic_info = Some(registry.bind(name, version.min(3), qh, ()))
                    }
                    "zcosmic_toplevel_manager_v1" => {
                        g.cosmic_manager = Some(registry.bind(name, version.min(4), qh, ()))
                    }
                    "ext_foreign_toplevel_list_v1" => {
                        g.toplevel_list = Some(registry.bind(name, 1, qh, ()))
                    }
                    "zwlr_foreign_toplevel_manager_v1" => {
                        g.foreign_toplevel = Some(registry.bind(name, version.min(3), qh, ()))
                    }
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = state.outputs.iter().position(|o| o.global == name) {
                    let o = state.outputs.remove(i);
                    if let Some(xdg) = o.xdg {
                        xdg.destroy();
                    }
                    if let Some(wl) = o.wl {
                        wl.release();
                    }
                    state.outputs_changed = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.output(*global) else {
            return;
        };
        match event {
            wl_output::Event::Geometry { transform, .. } => {
                o.transform = transform.into_result().ok();
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                if let WEnum::Value(flags) = flags
                    && flags.contains(wl_output::Mode::Current)
                {
                    o.mode = Some((width, height));
                }
            }
            wl_output::Event::Name { name } => o.name = name,
            wl_output::Event::Description { description } => o.description = description,
            wl_output::Event::Done => state.outputs_changed = true,
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.output(*global) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.position = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => o.size = Some((width, height)),
            // Before wl_output v4, the name comes from here.
            zxdg_output_v1::Event::Name { name } if o.name.is_empty() => o.name = name,
            // xdg-output v3 sends its changes with wl_output.done; older
            // versions end them with their own done.
            zxdg_output_v1::Event::Done => state.outputs_changed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        // A wl_keyboard gets the seat's keymap, focused or not.
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
            && caps.contains(wl_seat::Capability::Keyboard)
            && state.keyboard.is_none()
        {
            state.keyboard = Some(seat.get_keyboard(qh, ()));
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap {
            format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
            fd,
            size,
        } = event
        {
            match read_keymap(fd.as_fd(), size) {
                Ok(text) => state.keymap_changed = Some(text),
                Err(e) => eprintln!("waytool: couldn't read the seat's keymap: {e:#}"),
            }
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            state.next_toplevel += 1;
            let entry = Toplevel {
                id: state.next_toplevel,
                handle: Some(toplevel.clone()),
                cosmic: None,
                rect: None,
                title: String::new(),
                app_id: String::new(),
                outputs: Vec::new(),
                maximized: false,
                minimized: false,
                activated: false,
                fullscreen: false,
            };
            state.toplevels.push((toplevel, entry, Vec::new()));
        }
    }

    wayland_client::event_created_child!(State, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Event;
        let Some(i) = state.toplevels.iter().position(|(h, _, _)| h == handle) else {
            return;
        };
        let (_, t, outputs) = &mut state.toplevels[i];
        match event {
            Event::Title { title } => t.title = title,
            Event::AppId { app_id } => t.app_id = app_id,
            Event::OutputEnter { output } => outputs.push(output),
            Event::OutputLeave { output } => outputs.retain(|o| o != &output),
            Event::State { state: s } => {
                // An array of native-endian u32 states.
                let states: Vec<u32> = s
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_ne_bytes(*c))
                    .collect();
                t.maximized = states.contains(&0);
                t.minimized = states.contains(&1);
                t.activated = states.contains(&2);
                t.fullscreen = states.contains(&3);
            }
            Event::Done => state.toplevels_changed = true,
            Event::Closed => {
                state.toplevels.remove(i).0.destroy();
                state.toplevels_changed = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.next_toplevel += 1;
            let entry = Toplevel {
                id: state.next_toplevel,
                handle: None,
                cosmic: state
                    .globals
                    .cosmic_info
                    .as_ref()
                    .map(|info| info.get_cosmic_toplevel(&toplevel, qh, state.next_toplevel)),
                rect: None,
                title: String::new(),
                app_id: String::new(),
                outputs: Vec::new(),
                maximized: false,
                minimized: false,
                activated: false,
                fullscreen: false,
            };
            state.listed.push((toplevel, entry, Listed::default()));
        }
    }

    wayland_client::event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_foreign_toplevel_handle_v1::Event;
        let Some(i) = state.listed.iter().position(|(h, _, _)| h == handle) else {
            return;
        };
        let t = &mut state.listed[i].1;
        match event {
            Event::Title { title } => t.title = title,
            Event::AppId { app_id } => t.app_id = app_id,
            Event::Done => state.listed_changed = true,
            Event::Closed => {
                let (handle, t, _) = state.listed.remove(i);
                if let Some(cosmic) = t.cosmic {
                    cosmic.destroy();
                }
                handle.destroy();
                state.listed_changed = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZcosmicToplevelHandleV1, u64> for State {
    fn event(
        state: &mut Self,
        _: &ZcosmicToplevelHandleV1,
        event: zcosmic_toplevel_handle_v1::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zcosmic_toplevel_handle_v1::Event;
        let Some((_, t, extra)) = state.listed.iter_mut().find(|(_, t, _)| t.id == *id) else {
            return;
        };
        match event {
            Event::State { state: s } => {
                let states: Vec<u32> = s
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_ne_bytes(*c))
                    .collect();
                t.maximized = states.contains(&0);
                t.minimized = states.contains(&1);
                t.activated = states.contains(&2);
                t.fullscreen = states.contains(&3);
            }
            Event::OutputEnter { output } => extra.outputs.push(output),
            Event::OutputLeave { output } => {
                extra.outputs.retain(|o| o != &output);
                if extra.geometry.as_ref().is_some_and(|(o, ..)| o == &output) {
                    extra.geometry = None;
                }
            }
            Event::Geometry {
                output,
                x,
                y,
                width,
                height,
            } => extra.geometry = Some((output, x, y, width, height)),
            _ => return,
        }
        state.listed_changed = true;
    }
}

impl Dispatch<ZcosmicToplevelInfoV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZcosmicToplevelInfoV1,
        _: zcosmic_toplevel_info_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    // Only version 1 announces toplevels itself (we bind version 2 or
    // later and ask per ext handle instead); id 0 matches no window.
    wayland_client::event_created_child!(State, ZcosmicToplevelInfoV1, [
        zcosmic_toplevel_info_v1::EVT_TOPLEVEL_OPCODE => (ZcosmicToplevelHandleV1, 0u64),
    ]);
}

impl Dispatch<ZcosmicToplevelManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZcosmicToplevelManagerV1,
        _: zcosmic_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Read keymap text of `size` bytes (NUL-terminated) from `fd`.
fn read_keymap(fd: std::os::fd::BorrowedFd, size: u32) -> Result<String> {
    use std::os::unix::fs::FileExt;
    let file = std::fs::File::from(fd.try_clone_to_owned().context("duplicate keymap fd")?);
    let mut buf = vec![0; size as usize];
    file.read_exact_at(&mut buf, 0).context("read keymap")?;
    while buf.last() == Some(&0) {
        buf.pop();
    }
    String::from_utf8(buf).context("keymap isn't UTF-8")
}

/// Globals with no events.
macro_rules! no_events {
    ($($proxy:ty),* $(,)?) => {$(
        impl Dispatch<$proxy, ()> for State {
            fn event(
                _: &mut Self,
                _: &$proxy,
                _: <$proxy as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}

no_events!(
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    wl_shm::WlShm,
    ZwpVirtualKeyboardManagerV1,
    ZwlrVirtualPointerManagerV1,
    ExtImageCopyCaptureManagerV1,
    ExtOutputImageCaptureSourceManagerV1,
    ZwlrScreencopyManagerV1,
);

#[cfg(test)]
mod tests {
    /// Live: reads the running session's outputs and keymap.
    /// `cargo test -- --ignored outputs_live --nocapture`
    #[tokio::test]
    #[ignore]
    async fn outputs_live() {
        let wayland = super::Wayland::connect().unwrap();
        let monitors = wayland.monitors();
        println!("{monitors:#?}");
        let (keymap, _) = wayland.keymap();
        let keymap = keymap.expect("the seat has a keymap");
        println!("keymap: {} bytes", keymap.len());
        // The real keymap takes extra keys and still compiles.
        let grin = 0x0101_f600;
        let layouts =
            crate::keymap::compile(crate::keymap::extend(&keymap, &[grin]).unwrap()).unwrap();
        println!("grin at {:?}", layouts[0].press_for(grin, 0));
        assert!(layouts[0].press_for(grin, 0).is_some());
        assert!(!monitors.is_empty());
    }
}
