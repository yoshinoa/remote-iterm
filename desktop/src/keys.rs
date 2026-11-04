pub const PLAIN_PASTE_MAX_BYTES: usize = 512;

#[derive(Clone, Copy, Debug)]
pub struct KeyInput<'a> {
    pub key: &'a str,
    pub key_char: Option<&'a str>,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub platform: bool,
}

fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

fn printable(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(char::is_control)
}

fn named_key(k: &KeyInput) -> Option<String> {
    let csi = |f: &str| if k.shift { format!("\x1b[1;2{f}") } else { format!("\x1b[{f}") };
    let tilde = |n: u8| if k.shift { format!("\x1b[{n};2~") } else { format!("\x1b[{n}~") };
    let s = match k.key {
        "enter" => (if k.shift { "\n" } else { "\r" }).to_string(),
        "backspace" => "\x7f".to_string(),
        "tab" => (if k.shift { "\x1b[Z" } else { "\t" }).to_string(),
        "escape" => "\x1b".to_string(),
        "up" => csi("A"),
        "down" => csi("B"),
        "right" => csi("C"),
        "left" => csi("D"),
        "home" => csi("H"),
        "end" => csi("F"),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "f1" => "\x1bOP".to_string(),
        "f2" => "\x1bOQ".to_string(),
        "f3" => "\x1bOR".to_string(),
        "f4" => "\x1bOS".to_string(),
        "f5" => "\x1b[15~".to_string(),
        "f6" => "\x1b[17~".to_string(),
        "f7" => "\x1b[18~".to_string(),
        "f8" => "\x1b[19~".to_string(),
        "f9" => "\x1b[20~".to_string(),
        "f10" => "\x1b[21~".to_string(),
        "f11" => "\x1b[23~".to_string(),
        "f12" => "\x1b[24~".to_string(),
        _ => return None,
    };
    Some(s)
}

fn ctrl_code(key: &str) -> Option<String> {
    if key == "space" {
        return Some("\x00".to_string());
    }
    let c = single_char(key)?;
    let lower = c.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        return Some(char::from(lower as u8 - b'a' + 1).to_string());
    }
    let code = match c {
        '[' => "\x1b",
        '\\' => "\x1c",
        ']' => "\x1d",
        '^' => "\x1e",
        '_' => "\x1f",
        _ => return None,
    };
    Some(code.to_string())
}

pub fn encode_key(k: &KeyInput) -> Option<String> {
    if k.platform {
        return None;
    }
    if k.ctrl && k.alt {
        if let Some(ch) = k.key_char.filter(|ch| printable(ch)) {
            return Some(ch.to_string());
        }
    }
    if k.ctrl && !k.alt {
        if let Some(code) = ctrl_code(k.key) {
            return Some(code);
        }
    }
    if let Some(seq) = named_key(k) {
        return Some(seq);
    }
    if k.ctrl {
        return None;
    }
    if k.key == "space" {
        return Some(if k.alt { "\x1b " } else { " " }.to_string());
    }
    if k.alt {
        let base = single_char(k.key)?;
        let base = if k.shift { base.to_ascii_uppercase() } else { base };
        return Some(format!("\x1b{base}"));
    }
    let typed = k.key_char.or_else(|| single_char(k.key).map(|_| k.key))?;
    printable(typed).then(|| typed.to_string())
}

pub fn paste_sequence(text: &str) -> String {
    if !text.contains(['\r', '\n']) && text.len() <= PLAIN_PASTE_MAX_BYTES {
        return text.to_string();
    }
    let cleaned = text.replace('\x1b', "").replace("\r\n", "\n").replace('\r', "\n");
    format!("\x1b[200~{cleaned}\x1b[201~")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: &str) -> KeyInput<'_> {
        KeyInput { key, key_char: None, ctrl: false, alt: false, shift: false, platform: false }
    }

    fn typed<'a>(k: &'a str, ch: &'a str) -> KeyInput<'a> {
        KeyInput { key_char: Some(ch), ..key(k) }
    }

    fn enc(k: KeyInput) -> Option<String> {
        encode_key(&k)
    }

    #[test]
    fn printable_uses_key_char() {
        assert_eq!(enc(KeyInput { shift: true, ..typed("a", "A") }).as_deref(), Some("A"));
    }

    #[test]
    fn space() {
        assert_eq!(enc(typed("space", " ")).as_deref(), Some(" "));
    }

    #[test]
    fn ctrl_letters_are_c0() {
        assert_eq!(enc(KeyInput { ctrl: true, ..key("w") }).as_deref(), Some("\x17"));
        assert_eq!(enc(KeyInput { ctrl: true, ..key("c") }).as_deref(), Some("\x03"));
    }

    #[test]
    fn ctrl_punctuation() {
        assert_eq!(enc(KeyInput { ctrl: true, ..key("[") }).as_deref(), Some("\x1b"));
        assert_eq!(enc(KeyInput { ctrl: true, ..key("space") }).as_deref(), Some("\x00"));
    }

    #[test]
    fn ctrl_digit_is_unhandled() {
        assert_eq!(enc(KeyInput { ctrl: true, ..typed("1", "1") }), None);
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(enc(KeyInput { alt: true, ..typed("b", "∫") }).as_deref(), Some("\x1bb"));
    }

    #[test]
    fn altgr_sends_typed_character() {
        assert_eq!(enc(KeyInput { ctrl: true, alt: true, ..typed("q", "@") }).as_deref(), Some("@"));
    }

    #[test]
    fn enter_and_shift_enter() {
        assert_eq!(enc(typed("enter", "\n")).as_deref(), Some("\r"));
        assert_eq!(enc(KeyInput { shift: true, ..typed("enter", "\n") }).as_deref(), Some("\n"));
    }

    #[test]
    fn editing_keys() {
        assert_eq!(enc(key("backspace")).as_deref(), Some("\x7f"));
        assert_eq!(enc(key("tab")).as_deref(), Some("\t"));
        assert_eq!(enc(KeyInput { shift: true, ..key("tab") }).as_deref(), Some("\x1b[Z"));
        assert_eq!(enc(key("escape")).as_deref(), Some("\x1b"));
    }

    #[test]
    fn arrows_with_shift() {
        assert_eq!(enc(key("up")).as_deref(), Some("\x1b[A"));
        assert_eq!(enc(KeyInput { shift: true, ..key("up") }).as_deref(), Some("\x1b[1;2A"));
    }

    #[test]
    fn navigation() {
        assert_eq!(enc(key("home")).as_deref(), Some("\x1b[H"));
        assert_eq!(enc(key("end")).as_deref(), Some("\x1b[F"));
        assert_eq!(enc(key("pageup")).as_deref(), Some("\x1b[5~"));
        assert_eq!(enc(key("delete")).as_deref(), Some("\x1b[3~"));
    }

    #[test]
    fn function_keys() {
        assert_eq!(enc(key("f1")).as_deref(), Some("\x1bOP"));
        assert_eq!(enc(key("f5")).as_deref(), Some("\x1b[15~"));
    }

    #[test]
    fn platform_modifier_is_unhandled() {
        assert_eq!(enc(KeyInput { platform: true, ..typed("c", "c") }), None);
    }

    #[test]
    fn short_paste_is_plain() {
        assert_eq!(paste_sequence("ls -la"), "ls -la");
    }

    #[test]
    fn multiline_paste_is_bracketed() {
        assert_eq!(paste_sequence("a\r\nb\x1b"), "\x1b[200~a\nb\x1b[201~");
    }

    #[test]
    fn long_paste_is_bracketed() {
        assert!(paste_sequence(&"x".repeat(PLAIN_PASTE_MAX_BYTES + 1)).starts_with("\x1b[200~"));
    }
}
