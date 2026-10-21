//! Input through Wayland protocols, for compositors without a
//! RemoteDesktop portal (sway and other wlroots compositors, Hyprland,
//! niri): a virtual keyboard (virtual-keyboard-unstable-v1) and virtual
//! pointers (wlr-virtual-pointer-unstable-v1).
//!
//! A virtual keyboard's client supplies its keymap. This one uploads the
//! seat's own keymap, so every real key keeps its keycode (apps that read
//! keycodes, like emulators, see the keys they expect), extended with a
//! key for each character typed that the layout lacks (see
//! `keymap::extend`) — so any text can be typed, emoji included. The
//! virtual keyboard has its own modifier state, which we set ourselves:
//! the physical keyboard's Caps Lock doesn't apply to it.
//!
//! Absolute pointer motion takes positions as fractions of the layout's
//! bounding box, in logical coordinates; a screenshot pixel is the
//! logical point `origin + pixel / scale`, given with sub-pixel
//! precision, so every pixel is reachable at any scale.

use std::os::fd::AsFd;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use wayland_client::protocol::wl_pointer;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

use crate::keymap::{Events, Keys, Layout, canonical, check_code};
use crate::wayland::{Monitor, State, Wayland};

/// XKB_V1, the virtual keyboard's keymap format.
const KEYMAP_FORMAT_XKB_V1: u32 = 1;

/// Fractional precision for absolute moves: the protocol takes integer
/// positions within an extent, so the extent is the layout's logical
/// size times this.
const SUBPIXELS: f64 = 1000.0;

struct Keyboard {
    device: ZwpVirtualKeyboardV1,
    /// The seat keymap the uploaded one extends, and its generation.
    base: String,
    base_generation: u64,
    /// Keysyms added to it, in the order their keys were assigned.
    extras: Vec<u32>,
    /// The uploaded keymap, indexed; its first layout is the one used.
    layout: Layout,
    keys: Keys,
    /// The modifiers last sent.
    depressed: u32,
}

/// The virtual keyboard and pointers.
pub(crate) struct WlrInput {
    wayland: Arc<Wayland>,
    keyboard: Mutex<Keyboard>,
    pointer: ZwlrVirtualPointerV1,
}

impl WlrInput {
    /// Create the virtual keyboard and pointer. An error naming what's
    /// missing if the compositor doesn't offer them.
    pub(crate) fn new(wayland: Arc<Wayland>) -> Result<WlrInput> {
        let g = wayland.globals();
        let seat = g
            .seat
            .as_ref()
            .ok_or_else(|| anyhow!("the compositor has no seat"))?;
        let vk = g.virtual_keyboard.as_ref().ok_or_else(|| {
            anyhow!(
                "the compositor doesn't offer a virtual keyboard (zwp_virtual_keyboard_manager_v1)"
            )
        })?;
        let vp = g.virtual_pointer.as_ref().ok_or_else(|| {
            anyhow!(
                "the compositor doesn't offer virtual pointers (zwlr_virtual_pointer_manager_v1)"
            )
        })?;
        let (base, generation) = wayland.keymap();
        let generation = *generation.borrow();
        // A seat with no keyboard (a headless session, say) has no keymap
        // to copy; use the system's default layout.
        let base = match base.filter(|b| !crate::keymap::is_extended(b)) {
            Some(base) => base,
            None => {
                eprintln!("waytool: the seat has no keyboard layout; using the default one");
                crate::keymap::default_keymap()?
            }
        };

        let qh = wayland.qh();
        let device = vk.create_virtual_keyboard(seat, qh, ());
        let pointer = vp.create_virtual_pointer(Some(seat), qh, ());
        let (text, layout) = build_keymap(&base, &[])?;
        upload(&device, &text)?;
        wayland.flush()?;
        Ok(WlrInput {
            wayland: wayland.clone(),
            keyboard: Mutex::new(Keyboard {
                device,
                base,
                base_generation: generation,
                extras: Vec::new(),
                layout,
                keys: Keys::default(),
                depressed: 0,
            }),
            pointer,
        })
    }

    /// Every keysym can be typed: one the layout lacks gets a key of its
    /// own.
    pub(crate) fn has_keysym(&self, _keysym: u32) -> Result<bool> {
        Ok(true)
    }

    /// Press or release the key for `keysym`, as `keymap::Keys` plans it.
    pub(crate) fn keysym(&self, keysym: u32, press: bool) -> Result<()> {
        let mut kb = self.keyboard.lock().unwrap();
        self.refresh_keymap(&mut kb, press.then_some(keysym))?;
        let kb = &mut *kb;
        let events = if press {
            kb.keys.press_keysym(&kb.layout, keysym, 0)?
        } else {
            kb.keys.release_keysym(&kb.layout, keysym)?
        };
        self.send_keys(kb, events)
    }

    /// Press or release an evdev keycode as is.
    pub(crate) fn keycode(&self, code: u32, press: bool) -> Result<()> {
        check_code(code)?;
        let mut kb = self.keyboard.lock().unwrap();
        let events = if press {
            kb.keys.press_code(code)
        } else {
            kb.keys.release_code(code)
        };
        self.send_keys(&mut kb, events)
    }

    /// Upload a new keymap if the seat's changed, or `needed` isn't on
    /// the current one yet. Keys held stay valid: the new keymap keeps
    /// every key of the old.
    fn refresh_keymap(&self, kb: &mut Keyboard, needed: Option<u32>) -> Result<()> {
        let (base, generation) = self.wayland.keymap();
        let generation = *generation.borrow();
        let mut changed = false;
        if generation != kb.base_generation {
            kb.base_generation = generation;
            // Once the virtual keyboard has typed, the seat may report
            // its keymap — ours — as the seat's; that's no new layout.
            if let Some(base) = base.filter(|b| !crate::keymap::is_extended(b)) {
                kb.base = base;
                changed = true;
            }
        }
        if let Some(sym) = needed {
            let sym = canonical(sym);
            if !kb.layout.places.contains_key(&sym) && !kb.extras.contains(&sym) {
                kb.extras.push(sym);
                changed = true;
            }
        }
        if changed {
            let (text, layout) = build_keymap(&kb.base, &kb.extras)?;
            upload(&kb.device, &text)?;
            kb.layout = layout;
        }
        Ok(())
    }

    fn send_keys(&self, kb: &mut Keyboard, events: Events) -> Result<()> {
        let time = now_ms();
        for (code, press) in events {
            kb.device.key(time, code, press as u32);
            // The virtual keyboard's modifier state is ours to keep.
            let depressed = kb.keys.active_modifiers(&kb.layout, 0);
            if depressed != kb.depressed {
                kb.device.modifiers(depressed, 0, 0, 0);
                kb.depressed = depressed;
            }
        }
        self.wayland.flush()
    }

    /// Press or release an evdev pointer button.
    pub(crate) fn button(&self, code: u32, press: bool) -> Result<()> {
        check_code(code)?;
        let state = if press {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        self.pointer.button(now_ms(), code, state);
        self.pointer.frame();
        self.wayland.flush()
    }

    /// Relative pointer motion in logical pixels.
    pub(crate) fn move_rel(&self, dx: f64, dy: f64) -> Result<()> {
        self.pointer.motion(now_ms(), dx, dy);
        self.pointer.frame();
        self.wayland.flush()
    }

    /// Move to pixel (x, y) of monitor `index`'s screenshot: the global
    /// logical point `origin + pixel / scale`.
    pub(crate) fn move_to_pixel(&self, index: u32, x: f64, y: f64) -> Result<()> {
        let monitor = self
            .wayland
            .monitors()
            .into_iter()
            .nth(index as usize)
            .ok_or_else(|| anyhow!("no monitor {index}; `waytool monitors` lists them"))?;
        let (w, h) = physical_size(&monitor);
        if !(0.0..w as f64).contains(&x) || !(0.0..h as f64).contains(&y) {
            return Err(anyhow!(
                "({x}, {y}) is outside monitor {index}'s {w}x{h} screenshot"
            ));
        }
        self.move_to_global(
            monitor.x as f64 + x / monitor.scale,
            monitor.y as f64 + y / monitor.scale,
        )
    }

    /// Move to a global logical coordinate. Absolute motion from a
    /// virtual pointer bound to no output is a fraction of the whole
    /// layout's bounding box, which is logical space: the same on every
    /// compositor, whatever an output's transform.
    pub(crate) fn move_to_global(&self, x: f64, y: f64) -> Result<()> {
        let monitors = self.wayland.monitors();
        if !monitors.iter().any(|m| contains(m, x, y)) {
            return Err(anyhow!("no monitor covers global point ({x}, {y})"));
        }
        let (x0, y0, x1, y1) = monitors.iter().fold(
            (i32::MAX, i32::MAX, i32::MIN, i32::MIN),
            |(x0, y0, x1, y1), m| {
                (
                    x0.min(m.x),
                    y0.min(m.y),
                    x1.max(m.x + m.width),
                    y1.max(m.y + m.height),
                )
            },
        );
        let at = |v: f64, origin: i32| ((v - origin as f64) * SUBPIXELS) as u32;
        let extent = |from: i32, to: i32| ((to - from) as f64 * SUBPIXELS) as u32;
        self.pointer.motion_absolute(
            now_ms(),
            at(x, x0),
            at(y, y0),
            extent(x0, x1),
            extent(y0, y1),
        );
        self.pointer.frame();
        self.wayland.flush()
    }
}

/// A monitor's size in physical pixels: what a screenshot of it measures.
pub(crate) fn physical_size(m: &Monitor) -> (u32, u32) {
    (
        (m.width as f64 * m.scale).round() as u32,
        (m.height as f64 * m.scale).round() as u32,
    )
}

fn contains(m: &Monitor, x: f64, y: f64) -> bool {
    x >= m.x as f64 && x < (m.x + m.width) as f64 && y >= m.y as f64 && y < (m.y + m.height) as f64
}

/// The seat keymap `base` with keys for `extras`, as text and indexed.
fn build_keymap(base: &str, extras: &[u32]) -> Result<(String, Layout)> {
    let text = crate::keymap::extend(base, extras)?;
    let layout = crate::keymap::compile(text.clone())?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("the keymap has no layout"))?;
    Ok((text, layout))
}

/// Give the virtual keyboard `text` as its keymap, NUL-terminated in a
/// memfd as the protocol asks.
fn upload(device: &ZwpVirtualKeyboardV1, text: &str) -> Result<()> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    let fd = crate::daemon::memfd_with(&bytes).context("keymap memfd")?;
    device.keymap(KEYMAP_FORMAT_XKB_V1, fd.as_fd(), bytes.len() as u32);
    Ok(())
}

/// CLOCK_MONOTONIC in milliseconds, for event timestamps.
fn now_ms() -> u32 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes the current time into `ts`, which
    // is a valid timespec; CLOCK_MONOTONIC is always supported.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000) as u32
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
