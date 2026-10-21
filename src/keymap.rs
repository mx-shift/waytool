//! Typing on an XKB keyboard layout: where each keysym is (which key,
//! which level), which modifier keys select a level given the modifiers
//! already in effect, and which keys a virtual keyboard is holding. The
//! libei keyboard (`eis`) types through this and sends the key events.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use xkbcommon::xkb;

/// One past the highest evdev code (KEY_CNT in linux/input-event-codes.h).
/// A key or button code outside the range is a protocol error that ends
/// the whole libei connection, so it's refused before it's sent.
pub(crate) const KEY_CNT: u32 = 0x300;

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
pub(crate) struct Layout {
    /// Every place each keysym is on, as (evdev keycode, level), lowest
    /// keycode first. Keysyms are stored canonically (see `canonical`).
    pub(crate) places: HashMap<u32, Vec<(u32, u32)>>,
    /// Each key's type.
    types: HashMap<u32, KeyType>,
    /// The modifier keys that can be pressed to select a level, with the
    /// modifier mask each sets. Lock keys aren't among them: pressing
    /// one would toggle state rather than hold it.
    modifiers: Vec<(u32, u32)>,
}

impl Layout {
    /// The modifier mask that the modifier key `code` sets, if it's one.
    pub(crate) fn modifier_mask(&self, code: u32) -> u32 {
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
    pub(crate) fn press_for(&self, keysym: u32, active: u32) -> Option<(u32, Vec<u32>)> {
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
pub(crate) fn canonical(keysym: u32) -> u32 {
    if keysym & 0xff00_0000 == 0x0100_0000 {
        let sym = xkb::utf32_to_keysym(keysym & 0x00ff_ffff).raw();
        if sym != 0 {
            return sym;
        }
    }
    keysym
}

pub(crate) fn check_code(code: u32) -> Result<()> {
    if code >= KEY_CNT {
        return Err(anyhow!(
            "evdev code {code} is out of range (codes go up to {})",
            KEY_CNT - 1
        ));
    }
    Ok(())
}

/// Compile XKB keymap text and index every layout on it.
pub(crate) fn compile(text: String) -> Result<Vec<Layout>> {
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
pub(crate) fn index_keymap(keymap: &xkb::Keymap) -> Vec<Layout> {
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

/// Key events to send, in order: (evdev code, pressed).
pub(crate) type Events = Vec<(u32, bool)>;

/// The keys a virtual keyboard holds down, each with the modifier keys
/// pressed for it, so a release lets go of exactly what its press
/// added. The record is updated as the events are planned: if sending
/// them fails, the device is gone anyway.
#[derive(Default)]
pub(crate) struct Keys {
    /// Keys held down, each with the modifier keys pressed for it.
    held: HashMap<u32, Vec<u32>>,
    /// How many held keys each modifier key we pressed is down for.
    modifier_holds: HashMap<u32, u32>,
}

impl Keys {
    /// Whether nothing is held.
    pub(crate) fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Forget everything held (the keyboard was replaced).
    pub(crate) fn clear(&mut self) {
        self.held.clear();
        self.modifier_holds.clear();
    }

    /// The modifiers in effect for choosing a level: `locked`, plus
    /// those of the modifier keys held.
    pub(crate) fn active_modifiers(&self, layout: &Layout, locked: u32) -> u32 {
        self.held
            .keys()
            .chain(self.modifier_holds.keys())
            .fold(locked, |mods, code| mods | layout.modifier_mask(*code))
    }

    /// Press the key for `keysym`, adding the modifier keys its level
    /// needs given the modifiers in effect. Nothing to send if its key is
    /// already held.
    pub(crate) fn press_keysym(
        &mut self,
        layout: &Layout,
        keysym: u32,
        locked: u32,
    ) -> Result<Events> {
        let active = self.active_modifiers(layout, locked);
        let (key, modifiers) = layout
            .press_for(keysym, active)
            .ok_or_else(|| not_on_layout(keysym))?;
        Ok(self.press(key, modifiers))
    }

    /// Release the key for `keysym`: the key it was pressed on if that's
    /// held, with the modifiers its press added.
    pub(crate) fn release_keysym(&mut self, layout: &Layout, keysym: u32) -> Result<Events> {
        let places = layout
            .places
            .get(&canonical(keysym))
            .ok_or_else(|| not_on_layout(keysym))?;
        let key = places
            .iter()
            .map(|(key, _)| *key)
            .find(|key| self.held.contains_key(key))
            .unwrap_or(places[0].0);
        Ok(self.release(key))
    }

    /// Press an evdev keycode as is.
    pub(crate) fn press_code(&mut self, code: u32) -> Events {
        self.press(code, Vec::new())
    }

    /// Release an evdev keycode, with any modifiers its press added.
    pub(crate) fn release_code(&mut self, code: u32) -> Events {
        self.release(code)
    }

    fn press(&mut self, key: u32, modifiers: Vec<u32>) -> Events {
        if self.held.contains_key(&key) {
            return Vec::new();
        }
        let mut events: Events = modifiers
            .iter()
            .filter(|m| !self.modifier_holds.contains_key(m) && !self.held.contains_key(m))
            .map(|m| (*m, true))
            .collect();
        events.push((key, true));
        for m in &modifiers {
            *self.modifier_holds.entry(*m).or_default() += 1;
        }
        self.held.insert(key, modifiers);
        events
    }

    fn release(&mut self, key: u32) -> Events {
        let modifiers = self.held.remove(&key).unwrap_or_default();
        let mut events = vec![(key, false)];
        for m in modifiers.iter().rev() {
            if let Some(n) = self.modifier_holds.get_mut(m) {
                *n -= 1;
                if *n == 0 {
                    self.modifier_holds.remove(m);
                    if !self.held.contains_key(m) {
                        events.push((*m, false));
                    }
                }
            }
        }
        events
    }
}

fn not_on_layout(keysym: u32) -> anyhow::Error {
    anyhow!(
        "keysym {} ({keysym:#x}) isn't on the current keyboard layout",
        xkb::keysym_get_name(xkb::Keysym::new(keysym))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small US-style keymap with Shift, Caps Lock and an AltGr level,
    /// plus `less` on both the comma key and the ISO 102nd key, compiled
    /// from source so the test doesn't depend on the system's XKB data.
    const KEYMAP: &str = r#"
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

    fn layout() -> Layout {
        let source = KEYMAP;
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

    #[test]
    fn releases_undo_exactly_what_presses_added() {
        let l = layout();
        let mut keys = Keys::default();
        // "A" adds Shift, and its release takes Shift back up.
        assert_eq!(
            keys.press_keysym(&l, 0x41, 0).unwrap(),
            [(SHIFT, true), (A, true)]
        );
        assert_eq!(
            keys.release_keysym(&l, 0x61).unwrap(),
            [(A, false), (SHIFT, false)]
        );
        assert!(keys.is_empty());
        // A Shift held on purpose isn't pressed again, or released by
        // the key typed under it.
        assert_eq!(keys.press_code(SHIFT), [(SHIFT, true)]);
        assert_eq!(keys.press_keysym(&l, 0x41, 0).unwrap(), [(A, true)]);
        assert_eq!(keys.release_keysym(&l, 0x41).unwrap(), [(A, false)]);
        assert_eq!(keys.release_code(SHIFT), [(SHIFT, false)]);
        assert!(keys.is_empty());
    }
}
