use crate::protocol::{Cell, Content};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

pub const PREDICTION_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PredictKey {
    Char(char),
    Backspace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayCell {
    pub line: i64,
    pub col: usize,
    pub ch: char,
}

#[derive(Clone, Debug)]
struct Pending {
    line: i64,
    col: usize,
    expected: char,
    before: Option<Cell>,
    made_at: Instant,
}

#[derive(Debug, Default)]
pub struct Predictor {
    session: Option<String>,
    confirmed_epoch: bool,
    pending: Vec<Pending>,
    cursor: Option<(i64, usize)>,
}

fn is_blank(cell: Option<Cell>) -> bool {
    cell.is_none_or(|c| c.ch == ' ' || c.dim)
}

impl Predictor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.pending.clear();
        self.cursor = None;
        self.confirmed_epoch = false;
    }

    fn effective_cell(&self, screen: &Content, line: i64, col: usize) -> Option<Cell> {
        self.pending
            .iter()
            .rev()
            .find(|p| p.line == line && p.col == col)
            .map(|p| Cell { ch: p.expected, dim: false })
            .or_else(|| screen.cell(line, col))
    }

    fn can_predict_at(&self, screen: &Content, line: i64, col: usize) -> bool {
        let Some(text) = screen.line_text(line) else {
            return false;
        };
        if text.chars().any(|c| c.width() != Some(1)) {
            return false;
        }
        let len = text.chars().count();
        (col..len).all(|c| is_blank(self.effective_cell(screen, line, c)))
    }

    fn push(&mut self, screen: &Content, line: i64, col: usize, expected: char, now: Instant) {
        if let Some(existing) = self.pending.iter_mut().find(|p| p.line == line && p.col == col) {
            existing.expected = expected;
            existing.made_at = now;
            return;
        }
        self.pending.push(Pending { line, col, expected, before: screen.cell(line, col), made_at: now });
    }

    pub fn on_key(&mut self, session_id: &str, key: PredictKey, screen: &Content, now: Instant) {
        if self.session.as_deref() != Some(session_id) {
            self.reset();
            self.session = Some(session_id.to_string());
        }
        let Some((line, col)) = self.cursor.or_else(|| screen.cursor()) else {
            return self.reset();
        };
        let Some(columns) = screen.columns.map(|c| c as usize) else {
            return self.reset();
        };
        if !screen.is_latest || !self.can_predict_at(screen, line, col) {
            return self.reset();
        }
        match key {
            PredictKey::Char(ch) => {
                if col + 1 >= columns || ch.width() != Some(1) {
                    return self.reset();
                }
                self.push(screen, line, col, ch, now);
                self.cursor = Some((line, col + 1));
            }
            PredictKey::Backspace => {
                let pops_pending = self
                    .pending
                    .last()
                    .is_some_and(|p| p.line == line && p.col + 1 == col && p.expected != ' ' && p.before.is_none_or(|b| b.dim || b.ch == ' '));
                if pops_pending {
                    self.pending.pop();
                    self.cursor = if self.pending.is_empty() { None } else { Some((line, col - 1)) };
                } else if col == 0 {
                    self.reset();
                } else {
                    self.push(screen, line, col - 1, ' ', now);
                    self.cursor = Some((line, col - 1));
                }
            }
        }
    }

    pub fn on_content(&mut self, session_id: &str, screen: &Content, _now: Instant) {
        if self.session.as_deref() != Some(session_id) || !screen.is_latest {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        let mut kept = Vec::with_capacity(pending.len());
        for p in pending {
            let cell = screen.cell(p.line, p.col);
            let confirmed = if p.expected == ' ' {
                is_blank(cell)
            } else {
                cell == Some(Cell { ch: p.expected, dim: false })
            };
            if confirmed {
                self.confirmed_epoch = true;
            } else if cell == p.before {
                kept.push(p);
            } else {
                self.reset();
                return;
            }
        }
        self.pending = kept;
        if self.pending.is_empty() {
            self.cursor = None;
        }
    }

    pub fn tick(&mut self, now: Instant) {
        if self.pending.iter().any(|p| now.duration_since(p.made_at) > PREDICTION_TIMEOUT) {
            self.reset();
        }
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn overlay(&self) -> Vec<OverlayCell> {
        if !self.confirmed_epoch {
            return Vec::new();
        }
        self.pending.iter().map(|p| OverlayCell { line: p.line, col: p.col, ch: p.expected }).collect()
    }

    pub fn cursor(&self) -> Option<(i64, usize)> {
        if self.confirmed_epoch { self.cursor } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Run;
    use std::time::{Duration, Instant};

    const L: i64 = 100;

    fn r(t: &str) -> Run {
        Run { t: t.into(), ..Run::default() }
    }

    fn dim(t: &str) -> Run {
        Run { t: t.into(), d: true, ..Run::default() }
    }

    fn cur() -> Run {
        Run { c: true, ..Run::default() }
    }

    fn screen(line: Vec<Run>) -> Content {
        Content {
            lines: vec![vec![r("previous output")], line],
            fg: "#fff".into(),
            bg: "#000".into(),
            first_line: L - 1,
            available_first_line: 0,
            terminal_end: L + 1,
            is_latest: true,
            mouse: false,
            screen_first_line: Some(L - 40),
            columns: Some(80),
            rows: Some(41),
        }
    }

    fn prompt(text: &str) -> Content {
        screen(vec![r(&format!("❯ {text}")), cur()])
    }

    fn cell(ch: char, col: usize) -> OverlayCell {
        OverlayCell { line: L, col, ch }
    }

    fn confirmed_at(text: &str, now: Instant) -> Predictor {
        let mut p = Predictor::new();
        let typed = text.chars().last().unwrap();
        let before: String = text.chars().take(text.chars().count() - 1).collect();
        p.on_key("s1", PredictKey::Char(typed), &prompt(&before), now);
        p.on_content("s1", &prompt(text), now);
        assert!(p.overlay().is_empty());
        p
    }

    fn assert_hidden_epoch(p: &mut Predictor, now: Instant) {
        p.on_key("s1", PredictKey::Char('z'), &prompt("a"), now);
        assert!(p.overlay().is_empty());
        assert_eq!(p.cursor(), None);
    }

    #[test]
    fn first_prediction_in_epoch_is_hidden() {
        let mut p = Predictor::new();
        p.on_key("s1", PredictKey::Char('a'), &prompt(""), Instant::now());
        assert!(p.overlay().is_empty());
        assert_eq!(p.cursor(), None);
    }

    #[test]
    fn confirmed_epoch_draws_next_key_instantly() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        p.on_key("s1", PredictKey::Char('b'), &prompt("a"), now);
        assert_eq!(p.overlay(), vec![cell('b', 3)]);
        assert_eq!(p.cursor(), Some((L, 4)));
    }

    #[test]
    fn burst_keeps_unconfirmed_predictions() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        for ch in ['b', 'c', 'd'] {
            p.on_key("s1", PredictKey::Char(ch), &prompt("a"), now);
        }
        p.on_content("s1", &prompt("ab"), now);
        assert_eq!(p.overlay(), vec![cell('c', 4), cell('d', 5)]);
        assert_eq!(p.cursor(), Some((L, 6)));
    }

    #[test]
    fn contradiction_resets_and_hides() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        p.on_key("s1", PredictKey::Char('x'), &prompt("a"), now);
        p.on_content("s1", &prompt("ay"), now);
        assert!(p.overlay().is_empty());
        assert_hidden_epoch(&mut p, now);
    }

    #[test]
    fn timeout_resets() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        p.on_key("s1", PredictKey::Char('b'), &prompt("a"), now);
        p.tick(now + Duration::from_millis(500));
        assert_eq!(p.overlay(), vec![cell('b', 3)]);
        p.tick(now + PREDICTION_TIMEOUT + Duration::from_millis(1));
        assert!(p.overlay().is_empty());
        assert_hidden_epoch(&mut p, now);
    }

    #[test]
    fn backspace_removes_pending_char() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        p.on_key("s1", PredictKey::Char('b'), &prompt("a"), now);
        p.on_key("s1", PredictKey::Backspace, &prompt("a"), now);
        assert!(p.overlay().is_empty());
        assert_eq!(p.cursor(), None);
    }

    #[test]
    fn backspace_predicts_erasing_confirmed_text() {
        let now = Instant::now();
        let mut p = confirmed_at("abc", now);
        p.on_key("s1", PredictKey::Backspace, &prompt("abc"), now);
        assert_eq!(p.overlay(), vec![cell(' ', 4)]);
        assert_eq!(p.cursor(), Some((L, 4)));
        p.on_content("s1", &prompt("ab"), now);
        assert!(p.overlay().is_empty());
        assert_eq!(p.cursor(), None);
        p.on_key("s1", PredictKey::Char('x'), &prompt("ab"), now);
        assert_eq!(p.overlay(), vec![cell('x', 4)]);
    }

    #[test]
    fn retyping_over_a_pending_erase_keeps_one_prediction() {
        let now = Instant::now();
        let mut p = confirmed_at("abc", now);
        p.on_key("s1", PredictKey::Backspace, &prompt("abc"), now);
        p.on_key("s1", PredictKey::Char('x'), &prompt("abc"), now);
        assert_eq!(p.overlay(), vec![cell('x', 4)]);
        p.on_content("s1", &prompt("abx"), now);
        assert!(p.overlay().is_empty());
        p.on_key("s1", PredictKey::Char('y'), &prompt("abx"), now);
        assert_eq!(p.overlay(), vec![cell('y', 5)]);
    }

    #[test]
    fn dim_text_counts_as_blank() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        let suggested = screen(vec![r("❯ a"), cur(), dim("fix the thing")]);
        p.on_key("s1", PredictKey::Char('x'), &suggested, now);
        assert_eq!(p.overlay(), vec![cell('x', 3)]);
    }

    #[test]
    fn dim_cell_does_not_confirm() {
        let now = Instant::now();
        let mut p = Predictor::new();
        let suggested = screen(vec![r("❯ "), cur(), dim("fix")]);
        p.on_key("s1", PredictKey::Char('f'), &suggested, now);
        p.on_content("s1", &suggested, now);
        p.on_key("s1", PredictKey::Char('i'), &suggested, now);
        assert!(p.overlay().is_empty());
        p.on_content("s1", &screen(vec![r("❯ f"), cur(), dim("ix")]), now);
        assert_eq!(p.overlay(), vec![cell('i', 3)]);
    }

    #[test]
    fn no_prediction_mid_line() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        let mid = screen(vec![r("❯ a"), cur(), r("bc")]);
        p.on_key("s1", PredictKey::Char('x'), &mid, now);
        assert!(p.overlay().is_empty());
        assert_hidden_epoch(&mut p, now);
    }

    #[test]
    fn no_prediction_at_last_column() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        let mut edge = prompt("1234567");
        edge.columns = Some(10);
        p.on_key("s1", PredictKey::Char('x'), &edge, now);
        assert!(p.overlay().is_empty());
        assert_hidden_epoch(&mut p, now);
    }

    #[test]
    fn no_prediction_without_columns_or_history_view() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        let mut unknown = prompt("a");
        unknown.columns = None;
        p.on_key("s1", PredictKey::Char('x'), &unknown, now);
        assert!(p.overlay().is_empty());

        let mut p = confirmed_at("a", now);
        let mut history = prompt("a");
        history.is_latest = false;
        p.on_key("s1", PredictKey::Char('x'), &history, now);
        assert!(p.overlay().is_empty());
        assert_hidden_epoch(&mut p, now);
    }

    #[test]
    fn no_prediction_on_wide_char_line() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        p.on_key("s1", PredictKey::Char('x'), &prompt("漢"), now);
        assert!(p.overlay().is_empty());
        assert_hidden_epoch(&mut p, now);
    }

    #[test]
    fn other_session_content_is_ignored() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        p.on_key("s1", PredictKey::Char('b'), &prompt("a"), now);
        p.on_content("s2", &prompt("zzz"), now);
        assert_eq!(p.overlay(), vec![cell('b', 3)]);
    }

    #[test]
    fn key_for_new_session_resets() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        p.on_key("s2", PredictKey::Char('b'), &prompt("a"), now);
        assert!(p.overlay().is_empty());
    }

    #[test]
    fn reset_hides_and_clears() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        p.on_key("s1", PredictKey::Char('b'), &prompt("a"), now);
        p.reset();
        assert!(p.overlay().is_empty());
        assert_hidden_epoch(&mut p, now);
    }
}
