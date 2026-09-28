//! `KeyboardEvent.code` → PC/AT set-1 scan codes.
//!
//! Scan codes identify physical keys, so remote typing follows the *host's*
//! keyboard layout, the same as a local keyboard would.

/// Returns `(scan_code, extended)` for a `KeyboardEvent.code`.
pub fn scancode(code: &str) -> Option<(u16, bool)> {
    let plain = |s: u16| Some((s, false));
    let ext = |s: u16| Some((s, true));

    if let Some(letter) = code.strip_prefix("Key") {
        const LETTERS: &str = "QWERTYUIOPASDFGHJKLZXCVBNM";
        const CODES: [u16; 26] = [
            0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, // Q..P
            0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, // A..L
            0x2C, 0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x32, // Z..M
        ];
        if letter.len() == 1 {
            return LETTERS.find(letter).and_then(|i| plain(CODES[i]));
        }
    }
    if let Some(d) = code.strip_prefix("Digit") {
        return match d.parse::<u16>() {
            Ok(0) => plain(0x0B),
            Ok(n @ 1..=9) => plain(0x01 + n),
            _ => None,
        };
    }
    if let Some(n) = code.strip_prefix('F').and_then(|n| n.parse::<u16>().ok()) {
        return match n {
            1..=10 => plain(0x3A + n),
            11 => plain(0x57),
            12 => plain(0x58),
            _ => None,
        };
    }

    match code {
        "Escape" => plain(0x01),
        "Minus" => plain(0x0C),
        "Equal" => plain(0x0D),
        "Backspace" => plain(0x0E),
        "Tab" => plain(0x0F),
        "BracketLeft" => plain(0x1A),
        "BracketRight" => plain(0x1B),
        "Enter" => plain(0x1C),
        "ControlLeft" => plain(0x1D),
        "Semicolon" => plain(0x27),
        "Quote" => plain(0x28),
        "Backquote" => plain(0x29),
        "ShiftLeft" => plain(0x2A),
        "Backslash" => plain(0x2B),
        "Comma" => plain(0x33),
        "Period" => plain(0x34),
        "Slash" => plain(0x35),
        "ShiftRight" => plain(0x36),
        "NumpadMultiply" => plain(0x37),
        "AltLeft" => plain(0x38),
        "Space" => plain(0x39),
        "CapsLock" => plain(0x3A),
        "NumLock" => ext(0x45),
        "ScrollLock" => plain(0x46),
        "Numpad7" => plain(0x47),
        "Numpad8" => plain(0x48),
        "Numpad9" => plain(0x49),
        "NumpadSubtract" => plain(0x4A),
        "Numpad4" => plain(0x4B),
        "Numpad5" => plain(0x4C),
        "Numpad6" => plain(0x4D),
        "NumpadAdd" => plain(0x4E),
        "Numpad1" => plain(0x4F),
        "Numpad2" => plain(0x50),
        "Numpad3" => plain(0x51),
        "Numpad0" => plain(0x52),
        "NumpadDecimal" => plain(0x53),
        "IntlBackslash" => plain(0x56),
        "NumpadEnter" => ext(0x1C),
        "ControlRight" => ext(0x1D),
        "NumpadDivide" => ext(0x35),
        "PrintScreen" => ext(0x37),
        "AltRight" => ext(0x38),
        "Home" => ext(0x47),
        "ArrowUp" => ext(0x48),
        "PageUp" => ext(0x49),
        "ArrowLeft" => ext(0x4B),
        "ArrowRight" => ext(0x4D),
        "End" => ext(0x4F),
        "ArrowDown" => ext(0x50),
        "PageDown" => ext(0x51),
        "Insert" => ext(0x52),
        "Delete" => ext(0x53),
        "MetaLeft" => ext(0x5B),
        "MetaRight" => ext(0x5C),
        "ContextMenu" => ext(0x5D),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::scancode;

    #[test]
    fn maps_common_keys() {
        assert_eq!(scancode("KeyA"), Some((0x1E, false)));
        assert_eq!(scancode("KeyQ"), Some((0x10, false)));
        assert_eq!(scancode("KeyM"), Some((0x32, false)));
        assert_eq!(scancode("Digit1"), Some((0x02, false)));
        assert_eq!(scancode("Digit0"), Some((0x0B, false)));
        assert_eq!(scancode("F1"), Some((0x3B, false)));
        assert_eq!(scancode("F10"), Some((0x44, false)));
        assert_eq!(scancode("F12"), Some((0x58, false)));
        assert_eq!(scancode("ArrowLeft"), Some((0x4B, true)));
        assert_eq!(scancode("ControlRight"), Some((0x1D, true)));
    }

    #[test]
    fn rejects_unknown_codes() {
        assert_eq!(scancode("Key"), None);
        assert_eq!(scancode("KeyAB"), None);
        assert_eq!(scancode("F13"), None);
        assert_eq!(scancode("Digit10"), None);
        assert_eq!(scancode("Power"), None);
    }
}
