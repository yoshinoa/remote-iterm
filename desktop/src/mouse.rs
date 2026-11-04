pub const MOD_SHIFT: u8 = 4;
pub const MOD_CTRL: u8 = 16;
pub const MOTION: u8 = 32;
pub const WHEEL_UP: u8 = 64;
pub const WHEEL_DOWN: u8 = 65;

pub fn modifier_bits(shift: bool, ctrl: bool) -> u8 {
    (if shift { MOD_SHIFT } else { 0 }) | (if ctrl { MOD_CTRL } else { 0 })
}

pub fn sgr(code: u8, col: u32, row: u32, release: bool) -> String {
    let final_byte = if release { 'm' } else { 'M' };
    format!("\x1b[<{code};{col};{row}{final_byte}")
}

pub fn terminal_row(line: i64, screen_first_line: i64) -> u32 {
    u32::try_from((line - screen_first_line + 1).max(1)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgr_press_and_release() {
        assert_eq!(sgr(0, 5, 3, false), "\x1b[<0;5;3M");
        assert_eq!(sgr(0, 5, 3, true), "\x1b[<0;5;3m");
    }

    #[test]
    fn modifiers_and_motion() {
        assert_eq!(MOTION | modifier_bits(true, true), 52);
        assert_eq!(modifier_bits(false, false), 0);
        assert_eq!((WHEEL_UP, WHEEL_DOWN), (64, 65));
    }

    #[test]
    fn terminal_row_offsets_and_clamps() {
        assert_eq!(terminal_row(1040, 1000), 41);
        assert_eq!(terminal_row(990, 1000), 1);
    }
}
