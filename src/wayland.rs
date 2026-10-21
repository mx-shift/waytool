//! The daemon's own Wayland connection, and the monitor layout read
//! from it: wl_output for each output's name and mode, xdg-output
//! (zxdg_output_manager_v1) for its rect in the compositor's logical
//! layout. Every desktop compositor implements both, so this works the
//! same everywhere, unlike the compositors' own layout APIs.
//!
//! The connection is read on a thread of its own, which keeps the
//! layout current and tells the daemon when it changes.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

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

/// The layout, kept current by the connection's thread.
pub(crate) struct Wayland {
    monitors: Arc<Mutex<Vec<Monitor>>>,
    changes: tokio::sync::watch::Receiver<u64>,
}

impl Wayland {
    /// Connect to the compositor (WAYLAND_DISPLAY) and read the layout.
    pub(crate) fn connect() -> Result<Arc<Wayland>> {
        let conn = Connection::connect_to_env()
            .context("connect to the Wayland compositor (is WAYLAND_DISPLAY set?)")?;
        let mut queue: EventQueue<State> = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        // One roundtrip for the globals, a second for the outputs' and
        // xdg-outputs' first events.
        queue.roundtrip(&mut state).context("Wayland roundtrip")?;
        queue.roundtrip(&mut state).context("Wayland roundtrip")?;
        if state.xdg_manager.is_none() {
            return Err(anyhow!(
                "the compositor doesn't offer xdg-output (zxdg_output_manager_v1)"
            ));
        }

        let monitors = Arc::new(Mutex::new(state.monitors()));
        let (tx, changes) = tokio::sync::watch::channel(0u64);
        let thread_monitors = monitors.clone();
        std::thread::Builder::new()
            .name("wayland".into())
            .spawn(move || {
                let mut generation = 0;
                loop {
                    if let Err(e) = queue.blocking_dispatch(&mut state) {
                        eprintln!("waytool: Wayland connection lost: {e}");
                        return;
                    }
                    if std::mem::take(&mut state.changed) {
                        let now = state.monitors();
                        let mut cur = thread_monitors.lock().unwrap();
                        if *cur != now {
                            *cur = now;
                            generation += 1;
                            let _ = tx.send(generation);
                        }
                    }
                }
            })
            .context("start the Wayland thread")?;
        Ok(Arc::new(Wayland { monitors, changes }))
    }

    /// The current layout, in the compositor's announcement order.
    pub(crate) fn monitors(&self) -> Vec<Monitor> {
        self.monitors.lock().unwrap().clone()
    }

    /// A receiver that changes whenever the layout does.
    pub(crate) fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.clone()
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
    /// Whether the transform turns the output a quarter, so the mode's
    /// width is displayed as height.
    rotated: bool,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
}

#[derive(Default)]
struct State {
    outputs: Vec<Output>,
    xdg_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    changed: bool,
}

impl State {
    fn monitors(&self) -> Vec<Monitor> {
        self.outputs
            .iter()
            .filter_map(|o| {
                let (x, y) = o.position?;
                let (width, height) = o.size?;
                let scale = match o.mode {
                    Some((w, h)) if width > 0 => {
                        let shown = if o.rotated { h } else { w };
                        shown as f64 / width as f64
                    }
                    _ => 1.0,
                };
                Some(Monitor {
                    name: o.name.clone(),
                    description: o.description.clone(),
                    x,
                    y,
                    width,
                    height,
                    scale,
                })
            })
            .collect()
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
            } => match interface.as_str() {
                "wl_output" => {
                    let wl =
                        registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, name);
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
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = state.outputs.iter().position(|o| o.global == name) {
                    let o = state.outputs.remove(i);
                    if let Some(xdg) = o.xdg {
                        xdg.destroy();
                    }
                    if let Some(wl) = o.wl {
                        wl.release();
                    }
                    state.changed = true;
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
                use wl_output::Transform as T;
                o.rotated = matches!(
                    transform,
                    WEnum::Value(T::_90 | T::_270 | T::Flipped90 | T::Flipped270)
                );
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
            wl_output::Event::Done => state.changed = true,
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zxdg_output_manager_v1::ZxdgOutputManagerV1,
        _: zxdg_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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
            zxdg_output_v1::Event::Done => state.changed = true,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    /// Live: reads the running session's outputs.
    /// `cargo test -- --ignored outputs_live --nocapture`
    #[tokio::test]
    #[ignore]
    async fn outputs_live() {
        let wayland = super::Wayland::connect().unwrap();
        let monitors = wayland.monitors();
        println!("{monitors:#?}");
        assert!(!monitors.is_empty());
    }
}
