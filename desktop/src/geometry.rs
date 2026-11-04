use crate::protocol::Content;

pub fn point_to_cell(
    x: f32,
    y: f32,
    cell_width: f32,
    line_height: f32,
    scroll_top: f32,
    first_line: i64,
) -> (i64, usize) {
    let row = ((y + scroll_top) / line_height).floor().max(0.0) as i64;
    let col = (x / cell_width).floor().max(0.0) as usize;
    (first_line + row, col)
}

pub fn selection_text(content: &Content, a: (i64, usize), b: (i64, usize)) -> String {
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    let mut out = Vec::new();
    for line in start.0..=end.0 {
        let text: Vec<char> = content.line_text(line).unwrap_or_default().chars().collect();
        let from = if line == start.0 { start.1.min(text.len()) } else { 0 };
        let to = if line == end.0 { end.1.min(text.len()) } else { text.len() };
        let piece: String = text[from..to.max(from)].iter().collect();
        out.push(piece.trim_end().to_string());
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Content, Run};

    fn three_lines() -> Content {
        let line = |t: &str| vec![Run { t: t.into(), ..Run::default() }];
        Content {
            lines: vec![line("hello world  "), line("second line"), line("third")],
            fg: "#fff".into(),
            bg: "#000".into(),
            first_line: 20,
            available_first_line: 0,
            terminal_end: 23,
            is_latest: true,
            mouse: false,
            screen_first_line: None,
            columns: Some(80),
            rows: Some(24),
        }
    }

    #[test]
    fn point_maps_to_cell() {
        assert_eq!(point_to_cell(25.0, 37.0, 8.0, 16.0, 32.0, 1000), (1004, 3));
        assert_eq!(point_to_cell(-3.0, 0.0, 8.0, 16.0, 0.0, 1000), (1000, 0));
    }

    #[test]
    fn selection_spans_lines() {
        let c = three_lines();
        assert_eq!(selection_text(&c, (20, 6), (21, 6)), "world\nsecond");
        assert_eq!(selection_text(&c, (21, 6), (20, 6)), "world\nsecond");
        assert_eq!(selection_text(&c, (20, 0), (20, 5)), "hello");
        assert_eq!(selection_text(&c, (20, 0), (22, 5)), "hello world\nsecond line\nthird");
    }
}
