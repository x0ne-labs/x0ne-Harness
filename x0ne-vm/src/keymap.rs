//! Char / key-name → X11 keysym mapping for the RFB KeyEvent message.
//!
//! Covers:
//! - all printable ASCII (keysym == ASCII code)
//! - named special keys: Return, Tab, BackSpace, Escape, arrows, F1..F12,
//!   Home/End/PageUp/PageDown/Insert/Delete, modifiers (Ctrl/Alt/Shift/Meta/Super).
//!
//! Unicode beyond ASCII is intentionally out of scope for v0 — the keysym
//! namespace for Unicode (U+XXXX → 0x01000000 + codepoint) works with most
//! modern VNC servers but VM keyboard layout will determine the actual
//! character produced. Set the guest OS to a US layout for the cleanest path.

/// Returns the X11 keysym for an ASCII printable character.
/// For 0x20..=0x7E the keysym is the character code itself.
pub fn char_keysym(c: char) -> Option<u32> {
    let code = c as u32;
    if (0x20..=0x7e).contains(&code) {
        Some(code)
    } else {
        None
    }
}

/// On a US keyboard layout, returns true when the character requires Shift
/// to type. Used by `vm_type` to wrap the keysym in Shift_L down/up — without
/// this, QEMU/VNC keysym→scancode translation produces the unshifted variant
/// (e.g. `&` shows up as `7`, `|` as `\`, `"` as `'`).
pub fn ascii_char_needs_shift(c: char) -> bool {
    c.is_ascii_uppercase()
        || matches!(
            c,
            '!' | '@'
                | '#'
                | '$'
                | '%'
                | '^'
                | '&'
                | '*'
                | '('
                | ')'
                | '_'
                | '+'
                | '{'
                | '}'
                | '|'
                | ':'
                | '"'
                | '<'
                | '>'
                | '?'
                | '~'
        )
}

/// Returns the X11 keysym for a named key (case-insensitive).
///
/// Examples: `"return"`, `"enter"`, `"esc"`, `"tab"`, `"f1"`, `"left"`,
/// `"ctrl"`, `"shift"`, `"alt"`, `"meta"`, `"super"`, `"win"`.
pub fn named_keysym(name: &str) -> Option<u32> {
    let n = name.trim().to_ascii_lowercase();
    Some(match n.as_str() {
        // Whitespace + control
        "backspace" | "bs" => 0xff08,
        "tab" => 0xff09,
        "return" | "enter" | "ret" => 0xff0d,
        "escape" | "esc" => 0xff1b,
        "space" | "spc" => 0x0020,

        // Navigation
        "home" => 0xff50,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "pageup" | "page_up" | "pgup" | "prior" => 0xff55,
        "pagedown" | "page_down" | "pgdn" | "next" => 0xff56,
        "end" => 0xff57,
        "insert" | "ins" => 0xff63,
        "delete" | "del" => 0xffff,

        // Function keys F1..F12
        "f1" => 0xffbe,
        "f2" => 0xffbf,
        "f3" => 0xffc0,
        "f4" => 0xffc1,
        "f5" => 0xffc2,
        "f6" => 0xffc3,
        "f7" => 0xffc4,
        "f8" => 0xffc5,
        "f9" => 0xffc6,
        "f10" => 0xffc7,
        "f11" => 0xffc8,
        "f12" => 0xffc9,

        // Modifiers (left variant by default; explicit _l / _r honored)
        "shift" | "shift_l" => 0xffe1,
        "shift_r" => 0xffe2,
        "ctrl" | "control" | "control_l" | "ctrl_l" => 0xffe3,
        "control_r" | "ctrl_r" => 0xffe4,
        "alt" | "alt_l" => 0xffe9,
        "alt_r" | "altgr" => 0xffea,
        "meta" | "meta_l" => 0xffe7,
        "meta_r" => 0xffe8,
        "super" | "super_l" | "win" | "windows" | "cmd" => 0xffeb,
        "super_r" => 0xffec,

        "capslock" | "caps_lock" => 0xffe5,
        "numlock" | "num_lock" => 0xff7f,
        "scrolllock" | "scroll_lock" => 0xff14,
        "menu" => 0xff67,
        "printscreen" | "print" | "prtsc" => 0xff61,
        "pause" => 0xff13,

        // Single-letter and ASCII names — fall back to char path
        other if other.chars().count() == 1 => char_keysym(other.chars().next()?)?,
        _ => return None,
    })
}

/// Returns the keysym for any token: a single character, a named key, or
/// `None` if unknown.
pub fn resolve(token: &str) -> Option<u32> {
    if token.chars().count() == 1 {
        let c = token.chars().next().unwrap();
        if let Some(k) = char_keysym(c) {
            return Some(k);
        }
    }
    named_keysym(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_keysyms_match_codes() {
        assert_eq!(char_keysym('a'), Some(0x61));
        assert_eq!(char_keysym('A'), Some(0x41));
        assert_eq!(char_keysym('0'), Some(0x30));
        assert_eq!(char_keysym(' '), Some(0x20));
        assert_eq!(char_keysym('~'), Some(0x7e));
    }

    #[test]
    fn non_ascii_chars_return_none() {
        assert_eq!(char_keysym('é'), None);
        assert_eq!(char_keysym('\n'), None); // newline is below 0x20
    }

    #[test]
    fn named_keys_resolve() {
        assert_eq!(named_keysym("return"), Some(0xff0d));
        assert_eq!(named_keysym("Enter"), Some(0xff0d));
        assert_eq!(named_keysym(" ESC  "), Some(0xff1b));
        assert_eq!(named_keysym("ctrl"), Some(0xffe3));
        assert_eq!(named_keysym("Control_R"), Some(0xffe4));
        assert_eq!(named_keysym("F7"), Some(0xffc4));
        assert_eq!(named_keysym("win"), Some(0xffeb));
        assert_eq!(named_keysym("PageDown"), Some(0xff56));
    }

    #[test]
    fn resolve_handles_both_paths() {
        assert_eq!(resolve("a"), Some(0x61));
        assert_eq!(resolve("ctrl"), Some(0xffe3));
        assert_eq!(resolve("f10"), Some(0xffc7));
        assert_eq!(resolve("nonsense"), None);
    }
}
