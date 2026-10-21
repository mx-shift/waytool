//! Input through libei, connected with the RemoteDesktop portal's
//! ConnectToEIS.
//!
//! Why not the portal's Notify* calls: the portal docs put
//! NotifyPointerMotionAbsolute in the stream's logical coordinate space,
//! but GNOME deviates. Its portal checks the position against the
//! stream's logical size, then mutter reads it as physical pixels and
//! divides by the monitor scale (`meta-screen-cast-monitor-stream.c`),
//! which leaves only the top-left 1/scale of a scaled monitor reachable.
//! A libei absolute device instead has one region per stream — the
//! monitor's logical rect with its scale — and takes fractional logical
//! coordinates, so every physical pixel has a position:
//! `region origin + pixel / scale`.
//!
//! The portal refuses Notify* calls once a session has connected to
//! EIS, so the keyboard comes through here too. libei keyboards send
//! evdev keycodes, not keysyms, so a keysym is looked up in the XKB
//! keymap the compositor sends, and the modifier keys that select its
//! level, given the modifiers already in effect, are pressed with it. A
//! keysym that isn't on the layout can't be sent as a key at all; see
//! `daemon::type_text` for how those are typed.

use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use reis::ei;
use reis::event::{Device, DeviceCapability, EiEvent};

use crate::keymap::{Events, Keys, Layout, canonical, check_code};

/// How long to wait after connecting for the compositor to offer and
/// resume the devices.
const DEVICE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// One device the compositor offered.
struct Slot {
    device: Device,
    /// Whether the compositor has resumed it (it accepts events).
    resumed: bool,
    /// Whether we're between start_emulating and stop_emulating on it.
    emulating: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Which {
    Pointer,
    Absolute,
    Keyboard,
}

impl Which {
    fn name(self) -> &'static str {
        match self {
            Which::Pointer => "pointer",
            Which::Absolute => "absolute pointer",
            Which::Keyboard => "keyboard",
        }
    }
}

#[derive(Default)]
struct State {
    /// Relative motion, buttons and scrolling.
    pointer: Option<Slot>,
    /// Absolute motion, with one region per stream.
    absolute: Option<Slot>,
    keyboard: Option<Slot>,
    /// The keyboard's keymap, one entry per layout.
    layouts: Vec<Layout>,
    /// The keys we hold on the keyboard.
    keys: Keys,
    /// The keyboard's active layout and locked modifiers, from its
    /// modifiers events. Depressed modifiers aren't tracked from there:
    /// those events trail our own presses, so `keys` is the record of
    /// what we hold.
    group: u32,
    locked: u32,
    /// Buttons we hold down.
    held_buttons: HashSet<u32>,
    disconnected: Option<String>,
}

impl State {
    fn ready(&self) -> bool {
        [&self.pointer, &self.absolute, &self.keyboard]
            .iter()
            .all(|s| s.as_ref().is_some_and(|s| s.resumed))
    }

    fn slot(&mut self, which: Which) -> Option<&mut Slot> {
        match which {
            Which::Pointer => self.pointer.as_mut(),
            Which::Absolute => self.absolute.as_mut(),
            Which::Keyboard => self.keyboard.as_mut(),
        }
    }

    fn slot_for(&mut self, device: &Device) -> Option<&mut Slot> {
        [&mut self.pointer, &mut self.absolute, &mut self.keyboard]
            .into_iter()
            .flatten()
            .find(|s| &s.device == device)
    }

    /// The device that sends buttons: the relative pointer, or failing
    /// that the absolute one.
    fn button_device(&self) -> Option<Which> {
        [
            (Which::Pointer, &self.pointer),
            (Which::Absolute, &self.absolute),
        ]
        .into_iter()
        .find(|(_, s)| {
            s.as_ref()
                .is_some_and(|s| s.device.has_capability(DeviceCapability::Button))
        })
        .map(|(which, _)| which)
    }

    /// The current layout, or why there's none to type with.
    fn layout(&self) -> Result<&Layout> {
        current_layout(&self.keyboard, &self.layouts, self.group)
    }
}

/// The layout to type with, or why there's none. A free function so
/// callers can hold it while changing the other fields of `State`.
fn current_layout<'a>(
    keyboard: &Option<Slot>,
    layouts: &'a [Layout],
    group: u32,
) -> Result<&'a Layout> {
    if keyboard.is_none() {
        return Err(anyhow!("the compositor offered no keyboard device"));
    }
    layouts.get(group as usize).ok_or_else(|| {
        anyhow!("no keymap for keyboard layout {group} (the compositor's keymap didn't load)")
    })
}

/// A connected libei sender context.
pub(crate) struct Eis {
    connection: reis::event::Connection,
    state: Arc<Mutex<State>>,
    sequence: AtomicU32,
    /// Notified once when the connection is lost.
    lost: Arc<tokio::sync::Notify>,
    /// The connection's socket, shut down on drop so the libei thread
    /// sees end-of-stream and exits whether or not the compositor ever
    /// closes its side.
    socket: UnixStream,
}

impl Drop for Eis {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}

impl Eis {
    /// Run the libei handshake on `fd` (from ConnectToEIS), bind the
    /// seat's pointer, absolute pointer and keyboard, and wait for the
    /// compositor to offer and resume those devices.
    ///
    /// libei's event stream isn't Send, so it is read on a thread of its
    /// own, which keeps the shared state current until the connection
    /// closes.
    pub(crate) async fn connect(fd: OwnedFd) -> Result<Eis> {
        let state = Arc::new(Mutex::new(State::default()));
        let lost = Arc::new(tokio::sync::Notify::new());
        let socket = UnixStream::from(fd);
        let thread_socket = socket.try_clone().context("duplicate the libei socket")?;
        let (connected_tx, connected_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let thread_state = state.clone();
        let thread_lost = lost.clone();
        std::thread::Builder::new()
            .name("libei".into())
            .spawn(move || {
                run_events(thread_socket, thread_state, connected_tx, ready_tx);
                thread_lost.notify_one();
            })
            .context("start the libei thread")?;

        let connection = connected_rx
            .await
            .map_err(|_| anyhow!("the libei thread exited"))??;
        let eis = Eis {
            connection,
            state,
            sequence: AtomicU32::new(0),
            lost,
            socket,
        };

        let _ = tokio::time::timeout(DEVICE_TIMEOUT, ready_rx).await;
        let st = eis.state.lock().unwrap();
        if let Some(why) = &st.disconnected {
            return Err(anyhow!("libei: {why}"));
        }
        for (name, slot) in [
            ("pointer", &st.pointer),
            ("absolute pointer", &st.absolute),
            ("keyboard", &st.keyboard),
        ] {
            if slot.is_none() {
                eprintln!("waytool: libei: the compositor offered no {name} device");
            }
        }
        drop(st);
        Ok(eis)
    }

    /// Wait until the connection is lost: the compositor disconnected
    /// us, or the event stream failed. Input can't be sent after that.
    pub(crate) async fn lost(&self) {
        self.lost.notified().await;
    }

    /// Send events through interface `T` of one device, as one frame.
    ///
    /// The device stays emulating (no stop_emulating) while `keep` is
    /// set — while keys or buttons are held. The protocol lets the
    /// compositor return a device to neutral when emulation stops, which
    /// would release them.
    fn emit<T: ei::Interface>(
        &self,
        st: &mut State,
        which: Which,
        keep: bool,
        send: impl FnOnce(&T),
    ) -> Result<()> {
        if let Some(why) = &st.disconnected {
            return Err(anyhow!("libei: {why}"));
        }
        let what = which.name();
        let slot = st
            .slot(which)
            .ok_or_else(|| anyhow!("the compositor offered no {what} device"))?;
        if !slot.resumed {
            return Err(anyhow!("the compositor has paused the {what} device"));
        }
        let interface = slot
            .device
            .interface::<T>()
            .ok_or_else(|| anyhow!("the {what} device lacks the {} interface", T::NAME))?;
        let device = slot.device.device();
        let serial = self.connection.serial();
        if !slot.emulating {
            device.start_emulating(serial, self.sequence.fetch_add(1, Ordering::Relaxed));
            slot.emulating = true;
        }
        send(&interface);
        device.frame(serial, now_us());
        if !keep {
            device.stop_emulating(serial);
            slot.emulating = false;
        }
        self.connection.flush().context("libei flush")?;
        Ok(())
    }

    /// Whether `keysym` is on the current keyboard layout. An error if
    /// there's no keyboard or keymap to tell from.
    pub(crate) fn has_keysym(&self, keysym: u32) -> Result<bool> {
        let st = self.state.lock().unwrap();
        Ok(st.layout()?.places.contains_key(&canonical(keysym)))
    }

    /// Press or release the key for `keysym`. A press adds the modifier
    /// keys its level needs, given the modifiers already in effect; the
    /// release lets go of the key and of exactly the modifiers its press
    /// added (unless another held key still needs them).
    pub(crate) fn keysym(&self, keysym: u32, press: bool) -> Result<()> {
        let mut guard = self.state.lock().unwrap();
        let st = &mut *guard;
        let layout = current_layout(&st.keyboard, &st.layouts, st.group)?;
        let events = if press {
            st.keys.press_keysym(layout, keysym, st.locked)?
        } else {
            st.keys.release_keysym(layout, keysym)?
        };
        self.send_keys(st, events)
    }

    /// Press or release an evdev keycode as is.
    pub(crate) fn keycode(&self, code: u32, press: bool) -> Result<()> {
        check_code(code)?;
        let mut st = self.state.lock().unwrap();
        let events = if press {
            st.keys.press_code(code)
        } else {
            st.keys.release_code(code)
        };
        self.send_keys(&mut st, events)
    }

    fn send_keys(&self, st: &mut State, events: Events) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let keep = !st.keys.is_empty();
        self.emit::<ei::Keyboard>(st, Which::Keyboard, keep, |keyboard| {
            for (code, press) in events {
                let state = if press {
                    ei::keyboard::KeyState::Press
                } else {
                    ei::keyboard::KeyState::Released
                };
                keyboard.key(code, state);
            }
        })
    }

    /// Press or release an evdev pointer button.
    pub(crate) fn button(&self, code: u32, press: bool) -> Result<()> {
        check_code(code)?;
        let mut st = self.state.lock().unwrap();
        let which = st
            .button_device()
            .ok_or_else(|| anyhow!("the compositor offered no device with buttons"))?;
        if press {
            st.held_buttons.insert(code);
        } else {
            st.held_buttons.remove(&code);
        }
        let keep = !st.held_buttons.is_empty();
        self.emit::<ei::Button>(&mut st, which, keep, |button| {
            let state = if press {
                ei::button::ButtonState::Press
            } else {
                ei::button::ButtonState::Released
            };
            button.button(code, state);
        })
    }

    /// Relative motion in logical pixels.
    pub(crate) fn move_rel(&self, dx: f64, dy: f64) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let keep = st.button_device() == Some(Which::Pointer) && !st.held_buttons.is_empty();
        self.emit::<ei::Pointer>(&mut st, Which::Pointer, keep, |pointer| {
            pointer.motion_relative(dx as f32, dy as f32);
        })
    }

    /// Absolute motion to global logical coordinates, which must fall
    /// inside one of the absolute device's regions.
    pub(crate) fn move_abs(&self, x: f64, y: f64) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let region = st
            .absolute
            .as_ref()
            .and_then(|s| {
                s.device
                    .regions()
                    .iter()
                    .map(Region::from)
                    .find(|r| r.contains(x, y))
            })
            .ok_or_else(|| anyhow!("({x}, {y}) is outside every monitor's region"))?;
        // The compositor drops a position outside every region, and a
        // point just inside the far edge can round onto it as f32.
        let (x, y) = (region.inside_f32(x, true), region.inside_f32(y, false));
        let keep = st.button_device() == Some(Which::Absolute) && !st.held_buttons.is_empty();
        self.emit::<ei::PointerAbsolute>(&mut st, Which::Absolute, keep, |pointer| {
            pointer.motion_absolute(x, y);
        })
    }

    /// The region for a stream: by `mapping_id` when the portal gave
    /// one, otherwise the region with the stream's logical rect.
    pub(crate) fn region_for(
        &self,
        mapping_id: Option<&str>,
        rect: Option<((i32, i32), (i32, i32))>,
    ) -> Option<Region> {
        let st = self.state.lock().unwrap();
        let regions = st.absolute.as_ref()?.device.regions();
        let found = match mapping_id {
            Some(id) => regions.iter().find(|r| r.mapping_id.as_deref() == Some(id)),
            None => None,
        }
        .or_else(|| {
            let ((x, y), (w, h)) = rect?;
            regions.iter().find(|r| {
                (r.x as i64, r.y as i64, r.width as i64, r.height as i64)
                    == (x as i64, y as i64, w as i64, h as i64)
            })
        })?;
        Some(Region::from(found))
    }

    /// Whether global logical point (x, y) is inside some region.
    pub(crate) fn in_any_region(&self, x: f64, y: f64) -> bool {
        let st = self.state.lock().unwrap();
        st.absolute.as_ref().is_some_and(|s| {
            s.device
                .regions()
                .iter()
                .any(|r| Region::from(r).contains(x, y))
        })
    }
}

/// An absolute device's region: a monitor's logical rect and scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Region {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
}

impl From<&reis::event::Region> for Region {
    fn from(r: &reis::event::Region) -> Self {
        Region {
            x: r.x.into(),
            y: r.y.into(),
            width: r.width.into(),
            height: r.height.into(),
            scale: if r.scale > 0.0 { r.scale.into() } else { 1.0 },
        }
    }
}

impl Region {
    pub(crate) fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    /// The physical size of the region: what a screenshot of it measures.
    pub(crate) fn physical_size(&self) -> (f64, f64) {
        (self.width * self.scale, self.height * self.scale)
    }

    /// The global logical position of physical pixel (px, py) — a point
    /// read off this monitor's screenshot.
    pub(crate) fn pixel_to_global(&self, px: f64, py: f64) -> (f64, f64) {
        (self.x + px / self.scale, self.y + py / self.scale)
    }

    /// Coordinate `v` (x when `horizontal`, else y), which is inside the
    /// region, as an f32 that is still inside it.
    fn inside_f32(&self, v: f64, horizontal: bool) -> f32 {
        let end = if horizontal {
            self.x + self.width
        } else {
            self.y + self.height
        } as f32;
        let v = v as f32;
        if v >= end { end.next_down() } else { v }
    }
}

/// The libei thread: handshake, then keep `state` up to date from the
/// event stream until the connection closes. After a successful
/// handshake, `state.disconnected` is always set by the time it returns.
fn run_events(
    socket: UnixStream,
    state: Arc<Mutex<State>>,
    connected: tokio::sync::oneshot::Sender<Result<reis::event::Connection>>,
    ready: tokio::sync::oneshot::Sender<()>,
) {
    let handshake = ei::Context::new(socket)
        .context("libei socket")
        .and_then(|context| {
            context
                .handshake_blocking("waytool", ei::handshake::ContextType::Sender)
                .map_err(|e| anyhow!("libei handshake: {e}"))
        });
    let (connection, events) = match handshake {
        Ok(v) => v,
        Err(e) => {
            let _ = connected.send(Err(e));
            return;
        }
    };
    let _ = connection.flush();
    let _ = connected.send(Ok(connection.clone()));

    let mut ready = Some(ready);
    for event in events {
        let mut st = state.lock().unwrap();
        match event {
            Ok(event) => handle_event(&mut st, event),
            Err(e) => st.disconnected = Some(format!("libei error: {e}")),
        }
        // Handling an event can queue requests (binding the seat's
        // capabilities); they reach the compositor only once flushed.
        if let Err(e) = connection.flush() {
            st.disconnected = Some(format!("libei flush: {e}"));
        }
        if (st.ready() || st.disconnected.is_some())
            && let Some(tx) = ready.take()
        {
            let _ = tx.send(());
        }
        if st.disconnected.is_some() {
            return;
        }
    }
    state
        .lock()
        .unwrap()
        .disconnected
        .get_or_insert_with(|| "the compositor closed the libei connection".into());
}

fn handle_event(st: &mut State, event: EiEvent) {
    match event {
        EiEvent::SeatAdded(e) => e.seat.bind_capabilities(
            DeviceCapability::Pointer
                | DeviceCapability::PointerAbsolute
                | DeviceCapability::Keyboard
                | DeviceCapability::Button
                | DeviceCapability::Scroll,
        ),
        EiEvent::DeviceAdded(e) => {
            let device = e.device;
            let slot = Some(Slot {
                device: device.clone(),
                resumed: false,
                emulating: false,
            });
            if device.has_capability(DeviceCapability::Keyboard) {
                st.layouts = match device.keymap().map(load_keymap) {
                    Some(Ok(layouts)) => layouts,
                    Some(Err(e)) => {
                        eprintln!("waytool: libei keymap: {e:#}");
                        Vec::new()
                    }
                    None => {
                        eprintln!("waytool: libei keyboard came without a keymap");
                        Vec::new()
                    }
                };
                // A new keyboard (the compositor re-adds it when the
                // keymap changes) holds nothing.
                st.keys.clear();
                st.keyboard = slot;
            } else if device.has_capability(DeviceCapability::PointerAbsolute) {
                st.absolute = slot;
            } else if device.has_capability(DeviceCapability::Pointer) {
                st.pointer = slot;
            }
        }
        EiEvent::DeviceRemoved(e) => {
            for slot in [&mut st.pointer, &mut st.absolute, &mut st.keyboard] {
                if slot.as_ref().is_some_and(|s| s.device == e.device) {
                    *slot = None;
                }
            }
        }
        // Emulation doesn't survive a pause: it starts afresh on resume.
        EiEvent::DeviceResumed(e) => {
            if let Some(slot) = st.slot_for(&e.device) {
                slot.resumed = true;
                slot.emulating = false;
            }
        }
        EiEvent::DevicePaused(e) => {
            if let Some(slot) = st.slot_for(&e.device) {
                slot.resumed = false;
                slot.emulating = false;
            }
        }
        EiEvent::KeyboardModifiers(e) => {
            st.group = e.group;
            st.locked = e.locked;
        }
        EiEvent::Disconnected(e) => {
            st.disconnected = Some(
                e.explanation
                    .unwrap_or_else(|| format!("disconnected ({:?})", e.reason)),
            );
        }
        _ => {}
    }
}

/// Read the keymap the compositor sent and index every layout on it.
fn load_keymap(keymap: &reis::event::Keymap) -> Result<Vec<Layout>> {
    if keymap.type_ != ei::keyboard::KeymapType::Xkb {
        return Err(anyhow!("unsupported keymap type {:?}", keymap.type_));
    }
    let file = std::fs::File::from(keymap.fd.try_clone().context("duplicate keymap fd")?);
    let mut buf = vec![0; keymap.size as usize];
    file.read_exact_at(&mut buf, 0).context("read keymap")?;
    while buf.last() == Some(&0) {
        buf.pop();
    }
    crate::keymap::compile(String::from_utf8(buf).context("keymap isn't UTF-8")?)
}

/// CLOCK_MONOTONIC in microseconds, for frame timestamps.
fn now_us() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes the current time into `ts`, which
    // is a valid timespec; CLOCK_MONOTONIC is always supported.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_map_pixels_into_logical_space() {
        let r = Region {
            x: 1920.0,
            y: 0.0,
            width: 2560.0,
            height: 1440.0,
            scale: 1.5,
        };
        assert_eq!(r.physical_size(), (3840.0, 2160.0));
        // The far corner pixel lands just inside the logical rect.
        let (x, y) = r.pixel_to_global(3839.0, 2159.0);
        assert!(r.contains(x, y), "({x}, {y})");
        assert_eq!(r.pixel_to_global(0.0, 0.0), (1920.0, 0.0));
        assert!(!r.contains(1919.5, 10.0));
        // A point that rounds onto the far edge as f32 is kept inside.
        assert!(r.inside_f32(4_479.999_999_9, true) < 4480.0);
    }
}
