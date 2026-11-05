use crate::protocol::Run;
use crate::ui::theme;
use gpui::{Div, Hsla, ParentElement, Pixels, StyledText, Styled, TextRun, div};

pub struct TermStyle {
    pub fg: Hsla,
    pub cell_width: Pixels,
    pub line_height: Pixels,
    pub cursor: Hsla,
    pub selection: Hsla,
}

pub fn render_row(runs: &[Run], cursor_col: Option<usize>, highlight: Option<(usize, usize)>, style: &TermStyle) -> Div {
    let text: String = runs.iter().map(|r| r.t.as_str()).collect();
    let text_runs: Vec<TextRun> = runs
        .iter()
        .filter(|r| !r.t.is_empty())
        .map(|r| {
            let base = r.f.as_deref().and_then(theme::parse_hex).unwrap_or(style.fg);
            let color = if r.d { Hsla { a: base.a * 0.6, ..base } } else { base };
            TextRun {
                len: r.t.len(),
                font: theme::terminal_font(r.b),
                color,
                background_color: r.g.as_deref().and_then(theme::parse_hex),
                underline: None,
                strikethrough: None,
            }
        })
        .collect();
    let mut row = div()
        .relative()
        .h(style.line_height)
        .w_full()
        .whitespace_nowrap()
        .overflow_hidden();
    if let Some((from, to)) = highlight.filter(|(from, to)| to > from) {
        row = row.child(
            div()
                .absolute()
                .top_0()
                .left(style.cell_width * from as f32)
                .w(style.cell_width * (to - from) as f32)
                .h(style.line_height)
                .bg(style.selection),
        );
    }
    if let Some(col) = cursor_col {
        row = row.child(
            div()
                .absolute()
                .top_0()
                .left(style.cell_width * col as f32)
                .w(style.cell_width)
                .h(style.line_height)
                .bg(style.cursor),
        );
    }
    row.child(StyledText::new(text).with_runs(text_runs))
}

pub fn compose_row(runs: &[Run], overlay: &[(usize, char)]) -> (Vec<Run>, Option<usize>) {
    let mut styles: Vec<Run> = vec![Run::default()];
    let mut cells: Vec<(char, usize)> = Vec::new();
    let mut cursor = None;
    for run in runs {
        if run.c {
            cursor = Some(cells.len());
            continue;
        }
        styles.push(Run { t: String::new(), c: false, ..run.clone() });
        let style = styles.len() - 1;
        cells.extend(run.t.chars().map(|ch| (ch, style)));
    }
    if let Some(&(first_col, _)) = overlay.iter().min_by_key(|(col, _)| *col) {
        let base = first_col
            .checked_sub(1)
            .and_then(|left| cells.get(left))
            .map(|&(_, style)| Run { d: false, u: None, ..styles[style].clone() })
            .unwrap_or_default();
        styles.push(base);
        let style = styles.len() - 1;
        for &(col, ch) in overlay {
            if cells.len() <= col {
                cells.resize(col + 1, (' ', 0));
            }
            cells[col] = (ch, style);
        }
    }
    let mut out: Vec<Run> = Vec::new();
    let mut current: Option<usize> = None;
    for (ch, style) in cells {
        if current == Some(style) {
            out.last_mut().unwrap().t.push(ch);
        } else {
            out.push(Run { t: ch.to_string(), ..styles[style].clone() });
            current = Some(style);
        }
    }
    (out, cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(t: &str) -> Run {
        Run { t: t.into(), ..Run::default() }
    }

    fn text(runs: &[Run]) -> String {
        runs.iter().map(|r| r.t.as_str()).collect()
    }

    #[test]
    fn strips_cursor_marker_and_reports_its_column() {
        let (runs, cursor) = compose_row(&[r("❯ "), Run { c: true, ..Run::default() }, r("x")], &[]);
        assert_eq!(text(&runs), "❯ x");
        assert_eq!(cursor, Some(2));
        assert!(runs.iter().all(|r| !r.c));
    }

    #[test]
    fn overlay_overwrites_and_pads_with_undimmed_style_of_left_cell() {
        let green = Run { t: "❯ ab".into(), f: Some("#00ff00".into()), ..Run::default() };
        let ghost = Run { t: "cdef".into(), d: true, ..Run::default() };
        let (runs, _) = compose_row(&[green, Run { c: true, ..Run::default() }, ghost], &[(4, 'X'), (9, 'Y')]);
        assert_eq!(text(&runs), "❯ abXdef Y");
        let x = runs.iter().find(|r| r.t.contains('X')).unwrap();
        assert_eq!(x.f.as_deref(), Some("#00ff00"));
        assert!(!x.d);
    }
}
