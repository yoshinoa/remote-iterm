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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Typed,
    Erased,
}

#[derive(Clone, Debug)]
struct Pending {
    line: i64,
    col: usize,
    kind: Kind,
    expected: char,
    before: Option<Cell>,
    made_at: Instant,
}

impl Pending {
    fn matches(&self, cell: Option<Cell>) -> bool {
        if self.expected == ' ' {
            is_blank(cell)
        } else {
            cell == Some(Cell { ch: self.expected, dim: false })
        }
    }

    fn judged_by(&self, cursor_col: usize) -> bool {
        match self.kind {
            Kind::Typed => cursor_col > self.col,
            Kind::Erased => cursor_col <= self.col,
        }
    }
}

#[derive(Debug, Default)]
pub struct Predictor {
    session: Option<String>,
    confirmed_epoch: bool,
    pending: Vec<Pending>,
    cursor: Option<(i64, usize)>,
    stale: bool,
    input_start: Option<(String, i64, usize)>,
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
        self.stale = false;
    }

    fn in_input(&self, session_id: &str, screen: &Content, line: i64, col: usize) -> bool {
        !is_blank(screen.cell(line, col))
            || self
                .input_start
                .as_ref()
                .is_some_and(|(session, start_line, start)| session == session_id && *start_line == line && col >= *start)
    }

    fn can_predict_on(&self, screen: &Content, line: i64) -> bool {
        screen
            .line_text(line)
            .is_some_and(|text| text.chars().all(|c| c.width() == Some(1)))
    }

    fn upsert(&mut self, screen: &Content, line: i64, col: usize, kind: Kind, expected: char, now: Instant) {
        if let Some(existing) = self.pending.iter_mut().find(|p| p.line == line && p.col == col) {
            existing.kind = kind;
            existing.expected = expected;
            existing.made_at = now;
            return;
        }
        self.pending.push(Pending { line, col, kind, expected, before: screen.cell(line, col), made_at: now });
    }

    pub fn on_key(&mut self, session_id: &str, key: PredictKey, screen: &Content, now: Instant) {
        if self.session.as_deref() != Some(session_id) {
            self.reset();
            self.session = Some(session_id.to_string());
        }
        if self.stale {
            return;
        }
        let Some((line, col)) = self.cursor.or_else(|| screen.cursor()) else {
            return self.reset();
        };
        let Some(columns) = screen.columns.map(|c| c as usize) else {
            return self.reset();
        };
        if !screen.is_latest || !self.can_predict_on(screen, line) {
            return self.reset();
        }
        match key {
            PredictKey::Char(ch) => {
                if col + 1 >= columns || ch.width() != Some(1) {
                    return self.reset();
                }
                self.upsert(screen, line, col, Kind::Typed, ch, now);
                self.cursor = Some((line, col + 1));
            }
            PredictKey::Backspace => {
                let Some(target) = col.checked_sub(1) else {
                    return self.reset();
                };
                let pending_at_target = self.pending.iter().position(|p| p.line == line && p.col == target);
                match pending_at_target {
                    Some(ix) if self.pending[ix].kind == Kind::Typed && is_blank(self.pending[ix].before) && ix + 1 == self.pending.len() => {
                        self.pending.pop();
                        self.cursor = if self.pending.is_empty() { None } else { Some((line, target)) };
                    }
                    None if !self.in_input(session_id, screen, line, target) => {
                        if self.pending.is_empty() {
                            self.reset();
                        } else {
                            self.stale = true;
                        }
                    }
                    _ => {
                        self.upsert(screen, line, target, Kind::Erased, ' ', now);
                        self.cursor = Some((line, target));
                    }
                }
            }
        }
    }

    pub fn on_content(&mut self, session_id: &str, screen: &Content, _now: Instant) {
        if !screen.is_latest {
            return;
        }
        let Some((cursor_line, cursor_col)) = screen.cursor() else {
            return;
        };
        let text_before = screen
            .line_text(cursor_line)
            .is_some_and(|text| text.chars().take(cursor_col).any(|c| !c.is_whitespace()));
        match &mut self.input_start {
            Some((session, line, start)) if session == session_id && *line == cursor_line => {
                if text_before {
                    *start = (*start).min(cursor_col);
                }
            }
            _ => self.input_start = text_before.then(|| (session_id.to_string(), cursor_line, cursor_col)),
        }
        if self.session.as_deref() != Some(session_id) {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        let mut kept = Vec::with_capacity(pending.len());
        for p in pending {
            if p.line != cursor_line {
                self.reset();
                return;
            }
            if !p.judged_by(cursor_col) {
                kept.push(p);
                continue;
            }
            if !p.matches(screen.cell(p.line, p.col)) {
                self.reset();
                return;
            }
            if !p.matches(p.before) {
                self.confirmed_epoch = true;
            }
        }
        self.pending = kept;
        if self.pending.is_empty() {
            self.cursor = None;
            if self.stale {
                self.stale = false;
                self.confirmed_epoch = false;
            }
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
    fn predicts_over_text_right_of_the_cursor() {
        let now = Instant::now();
        let mut p = confirmed_at("e", now);
        let suggested = screen(vec![r("❯ e"), cur(), r("xit"), r("          ✔ │ 17:18:33")]);
        p.on_key("s1", PredictKey::Char('x'), &suggested, now);
        assert_eq!(p.overlay(), vec![cell('x', 3)]);
        p.on_content("s1", &screen(vec![r("❯ ex"), cur(), r("it"), r("          ✔ │ 17:18:33")]), now);
        assert!(p.overlay().is_empty());
        p.on_key("s1", PredictKey::Char('q'), &screen(vec![r("❯ ex"), cur(), r("it")]), now);
        assert_eq!(p.overlay(), vec![cell('q', 4)]);
    }

    #[test]
    fn mid_line_insert_confirms_as_the_cursor_passes() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        let mid = screen(vec![r("❯ a"), cur(), r("bc")]);
        p.on_key("s1", PredictKey::Char('X'), &mid, now);
        p.on_key("s1", PredictKey::Char('Y'), &mid, now);
        assert_eq!(p.overlay(), vec![cell('X', 3), cell('Y', 4)]);
        p.on_content("s1", &screen(vec![r("❯ aX"), cur(), r("bc")]), now);
        assert_eq!(p.overlay(), vec![cell('Y', 4)]);
        p.on_content("s1", &screen(vec![r("❯ aXY"), cur(), r("bc")]), now);
        assert!(p.overlay().is_empty());
    }

    fn lines(rows: Vec<Vec<Run>>) -> Content {
        Content { lines: rows, first_line: L, terminal_end: L + 3, ..screen(vec![]) }
    }

    fn at(text: &str, col: usize) -> Vec<Run> {
        let chars: Vec<char> = text.chars().collect();
        vec![r(&chars[..col].iter().collect::<String>()), cur(), r(&chars[col..].iter().collect::<String>())]
    }

    #[test]
    fn vim_motion_after_coincidental_match_is_never_drawn() {
        let now = Instant::now();
        let mut p = Predictor::new();
        let s0 = lines(vec![at("hello world", 2), vec![r("second line")], vec![r("")]]);
        p.on_key("s1", PredictKey::Char('l'), &s0, now);
        let s1 = lines(vec![at("hello world", 3), vec![r("second line")], vec![r("")]]);
        p.on_content("s1", &s1, now);
        p.on_key("s1", PredictKey::Char('j'), &s1, now);
        assert!(p.overlay().is_empty());
        let s2 = lines(vec![vec![r("hello world")], at("second line", 3), vec![r("")]]);
        p.on_content("s1", &s2, now);
        assert!(p.overlay().is_empty());
        assert_eq!(p.cursor(), None);
        p.on_key("s1", PredictKey::Char('k'), &s2, now);
        assert!(p.overlay().is_empty());
    }

    #[test]
    fn cursor_leaving_the_predicted_line_resets() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        p.on_key("s1", PredictKey::Char('j'), &prompt("a"), now);
        assert_eq!(p.overlay(), vec![cell('j', 3)]);
        let moved = Content { lines: vec![vec![r("previous output"), cur()], vec![r("❯ a")]], ..prompt("a") };
        p.on_content("s1", &moved, now);
        assert!(p.overlay().is_empty());
        assert_hidden_epoch(&mut p, now);
    }

    #[test]
    fn unrelated_snapshot_never_confirms_password_chars() {
        let now = Instant::now();
        let mut p = Predictor::new();
        let pw = lines(vec![vec![r("Password:"), cur()], vec![r("")], vec![r("")]]);
        for ch in ['c', 'o', ' '] {
            p.on_key("s1", PredictKey::Char(ch), &pw, now);
        }
        p.on_content("s1", &pw, now);
        let clock = lines(vec![vec![r("Password:"), cur()], vec![r("12:01")], vec![r("")]]);
        p.on_content("s1", &clock, now);
        p.on_key("s1", PredictKey::Char('x'), &clock, now);
        assert!(p.overlay().is_empty());
        assert_eq!(p.cursor(), None);
    }

    #[test]
    fn backspace_into_a_blank_cell_is_not_predicted() {
        let now = Instant::now();
        let mut p = confirmed_at("a", now);
        p.on_key("s1", PredictKey::Backspace, &prompt("a"), now);
        assert_eq!(p.overlay(), vec![cell(' ', 2)]);
        p.on_content("s1", &prompt(""), now);
        assert!(p.overlay().is_empty());
        p.on_key("s1", PredictKey::Backspace, &prompt(""), now);
        assert!(p.overlay().is_empty());
        assert_eq!(p.cursor(), None);
        p.on_key("s1", PredictKey::Char('x'), &prompt(""), now);
        p.on_content("s1", &prompt("x"), now);
        assert!(p.overlay().is_empty());
    }

    #[test]
    fn burst_survives_a_withdrawn_suggestion() {
        let now = Instant::now();
        let mut p = confirmed_at("g", now);
        let suggested = screen(vec![r("❯ g"), cur(), r("it status")]);
        for ch in ['o', ' ', 'b'] {
            p.on_key("s1", PredictKey::Char(ch), &suggested, now);
        }
        p.on_content("s1", &prompt("go"), now);
        assert_eq!(p.overlay(), vec![cell(' ', 4), cell('b', 5)]);
        assert_eq!(p.cursor(), Some((L, 6)));
    }

    #[test]
    fn typed_space_waits_for_the_cursor_before_confirming() {
        let now = Instant::now();
        let mut p = confirmed_at("ab", now);
        p.on_key("s1", PredictKey::Char(' '), &prompt("ab"), now);
        let spinner = Content { lines: vec![vec![r("✽ Thinking")], vec![r("❯ ab"), cur()]], ..prompt("ab") };
        p.on_content("s1", &spinner, now);
        p.on_key("s1", PredictKey::Char('x'), &spinner, now);
        assert_eq!(p.overlay(), vec![cell(' ', 4), cell('x', 5)]);
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
