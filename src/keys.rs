//! Name tables for keys.

/// The keysym a key name means: any XKB keysym name (`Return`,
/// `KP_Enter`, `F13`, `Print`, `period` — see xkbcommon-keysyms.h, case
/// ignored where unambiguous), a few common aliases (`Enter`, `Esc`,
/// `Ctrl`, `Shift`, `Alt`, `Super`, `Meta`, `PageUp`, `PageDown`,
/// `Backspace`; the modifier aliases are the left-hand keys), or a
/// single character.
pub(crate) fn keysym_from_name(name: &str) -> Option<u32> {
    use xkbcommon::xkb;
    let alias = match name.to_ascii_lowercase().as_str() {
        "enter" => Some("Return"),
        "esc" => Some("Escape"),
        "ctrl" | "control" => Some("Control_L"),
        "shift" => Some("Shift_L"),
        "alt" => Some("Alt_L"),
        "super" => Some("Super_L"),
        "meta" => Some("Meta_L"),
        "pageup" => Some("Page_Up"),
        "pagedown" => Some("Page_Down"),
        "backspace" => Some("BackSpace"),
        _ => None,
    };
    let lookup = |name: &str, flags| match xkb::keysym_from_name(name, flags).raw() {
        0 => None,
        sym => Some(sym),
    };
    if let Some(alias) = alias {
        return lookup(alias, xkb::KEYSYM_NO_FLAGS);
    }
    let mut chars = name.chars();
    let single = match (chars.next(), chars.next()) {
        (Some(ch), None) => Some(ch),
        _ => None,
    };
    lookup(name, xkb::KEYSYM_NO_FLAGS)
        .or_else(|| single.and_then(keysym_for_char))
        .or_else(|| lookup(name, xkb::KEYSYM_CASE_INSENSITIVE))
}

/// The keysym that types `ch`: the one XKB keymaps use for it
/// (`xkb_utf32_to_keysym` — the legacy keysym such as EuroSign or
/// Cyrillic_a where there is one, else 0x01000000 + code point). The
/// control characters that have keys map to them; other control
/// characters have no keysym.
pub(crate) fn keysym_for_char(ch: char) -> Option<u32> {
    Some(match ch {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        '\u{8}' => 0xff08,
        '\u{1b}' => 0xff1b,
        '\u{7f}' => 0xffff,
        c if c.is_control() => return None,
        c => match xkbcommon::xkb::utf32_to_keysym(c as u32).raw() {
            0 => return None,
            sym => sym,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_single_characters() {
        assert_eq!(keysym_from_name("space"), Some(0x20));
        assert_eq!(keysym_from_name("RETURN"), Some(0xff0d));
        assert_eq!(keysym_from_name("a"), Some(0x61));
        assert_eq!(keysym_from_name("€"), Some(0x20ac));
        assert_eq!(keysym_from_name("nope"), None);
        assert_eq!(keysym_from_name("KP_Enter"), Some(0xff8d));
        assert_eq!(keysym_from_name("f13"), Some(0xffca));
        assert_eq!(keysym_from_name("Ctrl"), Some(0xffe3));
        assert_eq!(keysym_from_name("PageDown"), Some(0xff56));
        assert_eq!(keysym_from_name("period"), Some(0x2e));
        assert_eq!(keysym_from_name("A"), Some(0x41));
    }

    #[test]
    fn characters() {
        assert_eq!(keysym_for_char('é'), Some(0xe9));
        assert_eq!(keysym_for_char('\r'), Some(0xff0d));
        assert_eq!(keysym_for_char('\u{1b}'), Some(0xff1b));
        assert_eq!(keysym_for_char('\u{1}'), None);
        assert_eq!(keysym_for_char('\u{85}'), None);
        assert_eq!(keysym_for_char('😀'), Some(0x101f600));
        assert_eq!(keysym_for_char('€'), Some(0x20ac));
        assert_eq!(keysym_for_char('а'), Some(0x6c1));
    }
}
