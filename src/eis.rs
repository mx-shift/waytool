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
//! keysym that isn't on the layout can't be sent as a key at all.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use reis::ei;
use reis::event::{Device, DeviceCapability, EiEvent};
use xkbcommon::xkb;

/// How long to wait after connecting for the compositor to offer and
/// resume the devices.
const DEVICE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How a key's type maps modifiers to levels: each listed modifier
/// combination (masked by `mask`) selects its level, anything else
/// selects level 0. This is the key type's `map` from the keymap.
#[derive(Clone, Debug, Default, PartialEq)]
struct KeyType {
    map: Vec<(u32, u32)>,
    mask: u32,
}

impl KeyType {
    fn level_for(&self, mods: u32) -> u32 {
        let mods = mods & self.mask;
        self.map
            .iter()
            .find(|(m, _)| *m == mods)
            .map_or(0, |(_, level)| *level)
    }
}

/// One layout (group) of the keymap, indexed for typing.
#[derive(Default)]
struct Layout {
    /// Every place each keysym is on, as (evdev keycode, level), lowest
    /// keycode first. Keysyms are stored canonically (see `canonical`).
    places: HashMap<u32, Vec<(u32, u32)>>,
    /// Each key's type.
    types: HashMap<u32, KeyType>,
    /// The modifier keys that can be pressed to select a level, with the
    /// modifier mask each sets. Lock keys aren't among them: pressing
    /// one would toggle state rather than hold it.
    modifiers: Vec<(u32, u32)>,
}

impl Layout {
    /// The modifier mask that the modifier key `code` sets, if it's one.
    fn modifier_mask(&self, code: u32) -> u32 {
        self.modifiers
            .iter()
            .find(|(c, _)| *c == code)
            .map_or(0, |(_, m)| *m)
    }

    /// How to type `keysym` when the modifiers in `active` are already
    /// in effect: its key, and the modifier keys to press with it. The
    /// lowest keycode it can be reached on wins. If no place can be
    /// reached — a modifier the caller holds on purpose selects another
    /// level — its key is pressed with nothing added, so the held
    /// modifier applies, as a chord like Shift+a means. None if the
    /// keysym isn't on the layout.
    fn press_for(&self, keysym: u32, active: u32) -> Option<(u32, Vec<u32>)> {
        let places = self.places.get(&canonical(keysym))?;
        places
            .iter()
            .find_map(|&(key, level)| {
                let ty = self.types.get(&key)?;
                Some((key, self.modifier_keys_for(ty, level, active)?))
            })
            .or_else(|| Some((places[0].0, Vec::new())))
    }

    /// The fewest modifier keys that, added to `active`, select `level`
    /// on a key of type `ty`. Modifiers can only be added, so a locked
    /// one (Caps Lock, say) is worked around rather than undone.
    fn modifier_keys_for(&self, ty: &KeyType, level: u32, active: u32) -> Option<Vec<u32>> {
        let usable: Vec<(u32, u32)> = self
            .modifiers
            .iter()
            .copied()
            .filter(|(_, m)| m & active == 0 && m & ty.mask != 0)
            .collect();
        let mut subsets: Vec<u32> = (0..1u32 << usable.len()).collect();
        subsets.sort_by_key(|s| s.count_ones());
        subsets.into_iter().find_map(|subset| {
            let (mods, keys) = usable
                .iter()
                .enumerate()
                .filter(|(i, _)| subset & (1 << i) != 0)
                .fold((active, Vec::new()), |(mods, mut keys), (_, (code, m))| {
                    keys.push(*code);
                    (mods | m, keys)
                });
            (ty.level_for(mods) == level).then_some(keys)
        })
    }
}

/// A keysym in the form the keymap uses. A character can be written
/// either as its legacy keysym (EuroSign, Cyrillic_a) or as the Unicode
/// keysym 0x01000000 + code point; XKB keymaps use the legacy one where
/// it exists, so Unicode keysyms are mapped to it.
fn canonical(keysym: u32) -> u32 {
    if keysym & 0xff00_0000 == 0x0100_0000 {
        let sym = xkb::utf32_to_keysym(keysym & 0x00ff_ffff).raw();
        if sym != 0 {
            return sym;
        }
    }
    keysym
}

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
    Keyboard,
}

impl Which {
    fn name(self) -> &'static str {
        match self {
            Which::Keyboard => "keyboard",
        }
    }
}

#[derive(Default)]
struct State {
    keyboard: Option<Slot>,
    /// The keyboard's keymap, one entry per layout.
    layouts: Vec<Layout>,
    /// The keyboard's active layout and locked modifiers, from its
    /// modifiers events. Depressed modifiers aren't tracked from there:
    /// those events trail our own presses, so `held_keys` is the record
    /// of what we hold.
    group: u32,
    locked: u32,
    /// Keys we hold down, each with the modifier keys pressed for it.
    held_keys: HashMap<u32, Vec<u32>>,
    /// How many held keys each modifier key we pressed is down for.
    modifier_holds: HashMap<u32, u32>,
    disconnected: Option<String>,
}

impl State {
    fn ready(&self) -> bool {
        self.keyboard.as_ref().is_some_and(|s| s.resumed)
    }

    fn slot(&mut self, which: Which) -> Option<&mut Slot> {
        match which {
            Which::Keyboard => self.keyboard.as_mut(),
        }
    }

    fn slot_for(&mut self, device: &Device) -> Option<&mut Slot> {
        self.keyboard.as_mut().filter(|s| &s.device == device)
    }

    /// The current layout, or why there's none to type with.
    fn layout(&self) -> Result<&Layout> {
        if self.keyboard.is_none() {
            return Err(anyhow!("the compositor offered no keyboard device"));
        }
        self.layouts.get(self.group as usize).ok_or_else(|| {
            anyhow!(
                "no keymap for keyboard layout {} (the compositor's keymap didn't load)",
                self.group
            )
        })
    }

    /// The modifiers in effect for choosing a level: those locked, plus
    /// those of the modifier keys we hold.
    fn active_modifiers(&self, layout: &Layout) -> u32 {
        self.held_keys
            .keys()
            .chain(self.modifier_holds.keys())
            .fold(self.locked, |mods, code| mods | layout.modifier_mask(*code))
    }
}

/// A connected libei sender context.
pub(crate) struct Eis {
    connection: reis::event::Connection,
    state: Arc<Mutex<State>>,
    sequence: AtomicU32,
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
    /// seat's keyboard, and wait for the compositor to offer and resume
    /// it.
    ///
    /// libei's event stream isn't Send, so it is read on a thread of its
    /// own, which keeps the shared state current until the connection
    /// closes.
    pub(crate) async fn connect(fd: OwnedFd) -> Result<Eis> {
        let state = Arc::new(Mutex::new(State::default()));
        let socket = UnixStream::from(fd);
        let thread_socket = socket.try_clone().context("duplicate the libei socket")?;
        let (connected_tx, connected_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let thread_state = state.clone();
        std::thread::Builder::new()
            .name("libei".into())
            .spawn(move || run_events(thread_socket, thread_state, connected_tx, ready_tx))
            .context("start the libei thread")?;

        let connection = connected_rx
            .await
            .map_err(|_| anyhow!("the libei thread exited"))??;
        let eis = Eis {
            connection,
            state,
            sequence: AtomicU32::new(0),
            socket,
        };

        let _ = tokio::time::timeout(DEVICE_TIMEOUT, ready_rx).await;
        let st = eis.state.lock().unwrap();
        if let Some(why) = &st.disconnected {
            return Err(anyhow!("libei: {why}"));
        }
        if st.keyboard.is_none() {
            eprintln!("waytool: libei: the compositor offered no keyboard device");
        }
        drop(st);
        Ok(eis)
    }

    /// Send events through interface `T` of one device, as one frame.
    ///
    /// The device stays emulating (no stop_emulating) while `keep` is
    /// set — while keys are held. The protocol lets the
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

    /// Press or release the key for `keysym`. A press adds the modifier
    /// keys its level needs, given the modifiers already in effect; the
    /// release lets go of the key and of exactly the modifiers its press
    /// added (unless another held key still needs them).
    pub(crate) fn keysym(&self, keysym: u32, press: bool) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let layout = st.layout()?;
        let not_on_layout = || {
            anyhow!(
                "keysym {} ({keysym:#x}) isn't on the current keyboard layout",
                xkb::keysym_get_name(xkb::Keysym::new(keysym))
            )
        };
        if press {
            let active = st.active_modifiers(layout);
            let (key, modifiers) = layout.press_for(keysym, active).ok_or_else(not_on_layout)?;
            if st.held_keys.contains_key(&key) {
                return Ok(());
            }
            self.press_key(&mut st, key, modifiers)
        } else {
            // The key it was pressed on, if it's held.
            let places = layout
                .places
                .get(&canonical(keysym))
                .ok_or_else(not_on_layout)?;
            let key = places
                .iter()
                .map(|(key, _)| *key)
                .find(|key| st.held_keys.contains_key(key))
                .unwrap_or(places[0].0);
            self.release_key(&mut st, key)
        }
    }

    fn press_key(&self, st: &mut State, key: u32, modifiers: Vec<u32>) -> Result<()> {
        let new: Vec<u32> = modifiers
            .iter()
            .copied()
            .filter(|m| !st.modifier_holds.contains_key(m) && !st.held_keys.contains_key(m))
            .collect();
        self.emit::<ei::Keyboard>(st, Which::Keyboard, true, |keyboard| {
            for m in &new {
                keyboard.key(*m, ei::keyboard::KeyState::Press);
            }
            keyboard.key(key, ei::keyboard::KeyState::Press);
        })?;
        for m in &modifiers {
            *st.modifier_holds.entry(*m).or_default() += 1;
        }
        st.held_keys.insert(key, modifiers);
        Ok(())
    }

    fn release_key(&self, st: &mut State, key: u32) -> Result<()> {
        let modifiers = st.held_keys.remove(&key).unwrap_or_default();
        let mut up = Vec::new();
        for m in modifiers {
            if let Some(n) = st.modifier_holds.get_mut(&m) {
                *n -= 1;
                if *n == 0 {
                    st.modifier_holds.remove(&m);
                    if !st.held_keys.contains_key(&m) {
                        up.push(m);
                    }
                }
            }
        }
        let keep = !st.held_keys.is_empty();
        self.emit::<ei::Keyboard>(st, Which::Keyboard, keep, |keyboard| {
            keyboard.key(key, ei::keyboard::KeyState::Released);
            for m in up.iter().rev() {
                keyboard.key(*m, ei::keyboard::KeyState::Released);
            }
        })
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
        EiEvent::SeatAdded(e) => e.seat.bind_capabilities(DeviceCapability::Keyboard.into()),
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
                st.held_keys.clear();
                st.modifier_holds.clear();
                st.keyboard = slot;
            }
        }
        EiEvent::DeviceRemoved(e) => {
            if st.keyboard.as_ref().is_some_and(|s| s.device == e.device) {
                st.keyboard = None;
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
    let text = String::from_utf8(buf).context("keymap isn't UTF-8")?;
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_string(
        &context,
        text,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| anyhow!("couldn't compile the keymap"))?;
    Ok(index_keymap(&keymap))
}

/// Keysyms of the keys that can supply a modifier for a level, most
/// preferred first. Lock-style modifiers (Caps Lock, Num Lock) are left
/// out: pressing them would toggle state.
const MODIFIER_KEYSYMS: &[u32] = &[
    xkb::keysyms::KEY_Shift_L,
    xkb::keysyms::KEY_Shift_R,
    xkb::keysyms::KEY_ISO_Level3_Shift,
    xkb::keysyms::KEY_Mode_switch,
    xkb::keysyms::KEY_ISO_Level5_Shift,
    xkb::keysyms::KEY_Control_L,
    xkb::keysyms::KEY_Alt_L,
    xkb::keysyms::KEY_Super_L,
];

/// Index each layout of `keymap`: where every keysym is, each key's
/// type, and the modifier keys. Keycodes are stored as evdev codes
/// (XKB keycode − 8).
fn index_keymap(keymap: &xkb::Keymap) -> Vec<Layout> {
    let keycodes = keymap.min_keycode().raw()..=keymap.max_keycode().raw();
    (0..keymap.num_layouts())
        .map(|layout| {
            let mut out = Layout::default();
            for &sym in MODIFIER_KEYSYMS {
                let Some(code) = keycodes.clone().find(|&k| {
                    keymap
                        .key_get_syms_by_level(xkb::Keycode::new(k), layout, 0)
                        .iter()
                        .any(|s| s.raw() == sym)
                }) else {
                    continue;
                };
                // The modifier mask the key sets while held.
                let mut state = xkb::State::new(keymap);
                state.update_mask(0, 0, 0, 0, 0, layout);
                state.update_key(xkb::Keycode::new(code), xkb::KeyDirection::Down);
                let mask = state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
                if mask != 0 && !out.modifiers.iter().any(|(_, m)| *m == mask) {
                    out.modifiers.push((code - 8, mask));
                }
            }

            for k in keycodes.clone() {
                let keycode = xkb::Keycode::new(k);
                let levels = keymap.num_levels_for_key(keycode, layout);
                if levels == 0 || k < 8 {
                    continue;
                }
                let mut ty = KeyType::default();
                for level in 0..levels {
                    let mut masks = [0u32; 16];
                    let n = keymap.key_get_mods_for_level(keycode, layout, level, &mut masks);
                    for &mask in &masks[..n] {
                        ty.mask |= mask;
                        if mask != 0 || level != 0 {
                            ty.map.push((mask, level));
                        }
                    }
                    // A level that produces several keysyms types none
                    // of them on its own.
                    if let [sym] = keymap.key_get_syms_by_level(keycode, layout, level) {
                        out.places
                            .entry(canonical(sym.raw()))
                            .or_default()
                            .push((k - 8, level));
                    }
                }
                out.types.insert(k - 8, ty);
            }
            out
        })
        .collect()
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

    /// A small US-style keymap with Shift, Caps Lock and an AltGr level,
    /// plus `less` on both the comma key and the ISO 102nd key, compiled
    /// from source so the test doesn't depend on the system's XKB data.
    fn layout() -> Layout {
        let source = r#"
            xkb_keymap {
                xkb_keycodes { minimum = 8; maximum = 255;
                    <AC01> = 38; <AB08> = 59; <AB10> = 61; <LSGT> = 94;
                    <LFSH> = 50; <CAPS> = 66; <RALT> = 108; <SPCE> = 65; <AE04> = 13; };
                xkb_types { include "complete" };
                xkb_compat { include "complete" };
                xkb_symbols {
                    key <AC01> { type = "FOUR_LEVEL_ALPHABETIC", [ a, A, aacute, Aacute ] };
                    key <AB08> { [ comma, less ] };
                    key <AB10> { [ slash, question ] };
                    key <LSGT> { [ less, greater ] };
                    key <AE04> { type = "FOUR_LEVEL", [ 4, dollar, EuroSign, cent ] };
                    key <SPCE> { [ space ] };
                    key <LFSH> { [ Shift_L ] };
                    key <CAPS> { [ Caps_Lock ] };
                    key <RALT> { type = "ONE_LEVEL", [ ISO_Level3_Shift ] };
                    modifier_map Shift { <LFSH> };
                    modifier_map Lock { <CAPS> };
                    modifier_map Mod5 { <RALT> };
                };
            };
        "#;
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(
            &context,
            source.into(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::COMPILE_NO_FLAGS,
        )
        .expect("test keymap compiles");
        index_keymap(&keymap).remove(0)
    }

    const SHIFT: u32 = 50 - 8;
    const ALTGR: u32 = 108 - 8;
    const A: u32 = 38 - 8;
    const LOCK: u32 = 1 << 1;

    #[test]
    fn keysyms_map_to_keys_and_level_modifiers() {
        let l = layout();
        let press = |sym| l.press_for(sym, 0);
        assert_eq!(press(0x61), Some((A, vec![])));
        assert_eq!(press(0x41), Some((A, vec![SHIFT])));
        assert_eq!(press(0xe1), Some((A, vec![ALTGR])));
        assert_eq!(press(0xc1), Some((A, vec![SHIFT, ALTGR])));
        assert_eq!(press(0x3f), Some((61 - 8, vec![SHIFT])));
        // Not on the layout.
        assert_eq!(press(0x1f600 | 0x0100_0000), None);
    }

    #[test]
    fn caps_lock_is_worked_around() {
        let l = layout();
        // With Caps Lock on, a bare A key types "A", so "a" needs Shift
        // and "A" needs nothing.
        assert_eq!(l.press_for(0x61, LOCK), Some((A, vec![SHIFT])));
        assert_eq!(l.press_for(0x41, LOCK), Some((A, vec![])));
    }

    #[test]
    fn a_held_modifier_is_not_pressed_again() {
        let l = layout();
        let shift = l.modifier_mask(SHIFT);
        assert_eq!(l.press_for(0x41, shift), Some((A, vec![])));
        // Shift held on purpose, then "b"-style lowercase: the key goes
        // down with the held Shift applying, as a chord means.
        assert_eq!(l.press_for(0x61, shift), Some((A, vec![])));
    }

    #[test]
    fn the_lowest_keycode_wins() {
        // `less` is on the comma key (Shift) and on the ISO 102nd key
        // (no modifier); US keyboards have only the first.
        assert_eq!(layout().press_for(0x3c, 0), Some((59 - 8, vec![SHIFT])));
    }

    #[test]
    fn unicode_keysyms_find_legacy_ones() {
        // U+20AC is EuroSign (0x20ac) in the keymap.
        assert_eq!(
            layout().press_for(0x0100_20ac, 0),
            Some((13 - 8, vec![ALTGR]))
        );
    }
}
