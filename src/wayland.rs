//! The daemon's own Wayland connection: the globals it binds, the
//! monitor layout, and the seat's keyboard layout.
//!
//! The layout comes from wl_output (each output's name and mode) and
//! xdg-output (its rect in the compositor's logical layout), which
//! every desktop compositor implements, so `monitors` and display-change
//! tracking work the same everywhere. The keyboard layout is the keymap
//! the compositor sends a client's wl_keyboard. The virtual input
//! managers are what the Wayland backend (`wlr_input`) uses where
//! there's no RemoteDesktop portal.
//!
//! Events are read on a thread of its own; requests can be made from any
//! thread through the proxies here (wayland-client is thread-safe), and
//! are sent with `flush`.

use std::os::fd::AsFd;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
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
}

/// The globals the Wayland backend can use, where the compositor offers
/// them.
#[derive(Clone, Default)]
pub(crate) struct Globals {
    pub seat: Option<wl_seat::WlSeat>,
    pub virtual_keyboard: Option<ZwpVirtualKeyboardManagerV1>,
    pub virtual_pointer: Option<ZwlrVirtualPointerManagerV1>,
}

/// What the event thread keeps current for the rest of the daemon.
#[derive(Default)]
struct Shared {
    /// The monitors, each with its wl_output.
    monitors: Vec<(Monitor, wl_output::WlOutput)>,
    /// The seat keyboard's keymap text, when it has one.
    keymap: Option<String>,
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
                    "zwp_virtual_keyboard_manager_v1" => {
                        g.virtual_keyboard = Some(registry.bind(name, 1, qh, ()))
                    }
                    "zwlr_virtual_pointer_manager_v1" => {
                        g.virtual_pointer = Some(registry.bind(name, version.min(2), qh, ()))
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
    ZwpVirtualKeyboardManagerV1,
    ZwlrVirtualPointerManagerV1,
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
