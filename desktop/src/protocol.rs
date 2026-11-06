use serde::Deserialize;

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct Run {
    #[serde(default)]
    pub t: String,
    pub f: Option<String>,
    pub g: Option<String>,
    #[serde(default)]
    pub b: bool,
    #[serde(default)]
    pub d: bool,
    #[serde(default)]
    pub c: bool,
    pub u: Option<String>,
}

impl Run {
    pub fn same_style(&self, other: &Run) -> bool {
        self.f == other.f && self.g == other.g && self.b == other.b && self.d == other.d && self.u == other.u
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Content {
    pub lines: Vec<Vec<Run>>,
    pub fg: String,
    pub bg: String,
    pub first_line: i64,
    pub available_first_line: i64,
    pub terminal_end: i64,
    pub is_latest: bool,
    #[serde(default)]
    pub mouse: bool,
    pub screen_first_line: Option<i64>,
    pub columns: Option<u32>,
    pub rows: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionContent {
    pub session_id: String,
    #[serde(flatten)]
    pub content: Content,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowState {
    pub id: String,
    pub is_front: bool,
    pub tabs: Vec<TabState>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TabState {
    pub index: u32,
    pub id: String,
    pub tab_id: String,
    #[serde(default)]
    pub title: String,
    pub is_selected: bool,
    #[serde(default)]
    pub current_session_id: String,
    pub sessions: Vec<SessionInfo>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub rect: Option<Rect>,
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub dim: bool,
}

impl Content {
    pub fn end_line(&self) -> i64 {
        self.first_line + self.lines.len() as i64
    }

    pub fn line(&self, line: i64) -> Option<&[Run]> {
        let index = usize::try_from(line - self.first_line).ok()?;
        self.lines.get(index).map(Vec::as_slice)
    }

    pub fn cursor(&self) -> Option<(i64, usize)> {
        self.lines.iter().enumerate().rev().find_map(|(index, runs)| {
            let at = runs.iter().position(|r| r.c)?;
            let col = runs[..at].iter().map(|r| r.t.chars().count()).sum();
            Some((self.first_line + index as i64, col))
        })
    }

    pub fn suggestion(&self, line: i64) -> Option<std::ops::Range<usize>> {
        let runs = self.line(line)?;
        let at = runs.iter().position(|r| r.c)?;
        let start: usize = runs[..at].iter().map(|r| r.t.chars().count()).sum();
        let left = runs[..at].iter().rev().find(|r| !r.t.is_empty())?;
        let mut after = runs[at + 1..].iter().filter(|r| !r.t.is_empty());
        let first = after.next()?;
        if first.t.trim().is_empty() || (!first.d && first.same_style(left)) {
            return None;
        }
        let mut end = start + first.t.chars().count();
        for run in after.take_while(|r| r.same_style(first)) {
            end += run.t.chars().count();
        }
        Some(start..end)
    }

    pub fn cell(&self, line: i64, col: usize) -> Option<Cell> {
        let suggestion = self.suggestion(line);
        let mut remaining = col;
        for run in self.line(line)? {
            let len = run.t.chars().count();
            if remaining < len {
                let dim = run.d || suggestion.as_ref().is_some_and(|s| s.contains(&col));
                return run.t.chars().nth(remaining).map(|ch| Cell { ch, dim });
            }
            remaining -= len;
        }
        None
    }

    pub fn link_at(&self, line: i64, col: usize) -> Option<String> {
        let mut remaining = col;
        for run in self.line(line)? {
            let len = run.t.chars().count();
            if remaining < len {
                return run.u.clone();
            }
            remaining -= len;
        }
        None
    }

    pub fn line_text(&self, line: i64) -> Option<String> {
        Some(self.line(line)?.iter().map(|r| r.t.as_str()).collect())
    }
}

pub fn merge_content(current: Option<&Content>, incoming: Content) -> Content {
    let Some(current) = current else {
        return incoming;
    };
    let current_end = current.end_line();
    let incoming_end = incoming.end_line();
    if incoming.first_line > current_end || current.first_line > incoming_end {
        return incoming;
    }

    let first_line = incoming
        .available_first_line
        .max(current.first_line.min(incoming.first_line));
    let end_line = incoming.terminal_end.min(current_end.max(incoming_end));
    let mut lines: Vec<Vec<Run>> = vec![Vec::new(); usize::try_from(end_line - first_line).unwrap_or(0)];

    let mut copy = |source: &[Vec<Run>], source_first: i64, remove_cursor: bool| {
        for (index, runs) in source.iter().enumerate() {
            let Ok(target) = usize::try_from(source_first + index as i64 - first_line) else {
                continue;
            };
            if let Some(slot) = lines.get_mut(target) {
                *slot = if remove_cursor {
                    runs.iter().filter(|r| !r.c).cloned().collect()
                } else {
                    runs.clone()
                };
            }
        }
    };
    copy(&current.lines, current.first_line, incoming.is_latest);
    copy(&incoming.lines, incoming.first_line, false);

    Content {
        lines,
        fg: incoming.fg,
        bg: incoming.bg,
        first_line,
        available_first_line: incoming.available_first_line,
        terminal_end: incoming.terminal_end,
        is_latest: current.is_latest || incoming.is_latest,
        mouse: incoming.mouse,
        screen_first_line: incoming.screen_first_line,
        columns: incoming.columns.or(current.columns),
        rows: incoming.rows.or(current.rows),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(t: &str) -> Run {
        Run { t: t.into(), ..Run::default() }
    }

    fn cursor_run() -> Run {
        Run { c: true, ..Run::default() }
    }

    fn content(first_line: i64, lines: Vec<Vec<Run>>, is_latest: bool, terminal_end: i64) -> Content {
        Content {
            lines,
            fg: "#ffffff".into(),
            bg: "#000000".into(),
            first_line,
            available_first_line: 0,
            terminal_end,
            is_latest,
            mouse: false,
            screen_first_line: None,
            columns: Some(100),
            rows: Some(40),
        }
    }

    fn numbered(first: i64, last: i64, cursor_at: Option<i64>) -> Vec<Vec<Run>> {
        (first..last)
            .map(|n| {
                let mut runs = vec![run(&n.to_string())];
                if cursor_at == Some(n) {
                    runs.push(cursor_run());
                }
                runs
            })
            .collect()
    }

    fn cursor_count(c: &Content) -> usize {
        c.lines.iter().flatten().filter(|r| r.c).count()
    }

    #[test]
    fn deserializes_server_content() {
        let value = json!({
            "sessionId": "S1",
            "lines": [[{"t": "❯ ", "f": "#999999"}, {"t": "", "c": true}, {"t": "fix", "d": true}]],
            "fg": "#ffffff", "bg": "#000000",
            "firstLine": 1000, "availableFirstLine": 0, "terminalEnd": 1001,
            "isLatest": true, "mouse": false, "screenFirstLine": 960,
            "columns": 100, "rows": 40
        });
        let parsed: SessionContent = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.session_id, "S1");
        assert_eq!(parsed.content.columns, Some(100));
        assert_eq!(parsed.content.first_line, 1000);
        assert!(parsed.content.lines[0][1].c);
        assert!(parsed.content.lines[0][2].d);
        assert_eq!(parsed.content.lines[0][0].f.as_deref(), Some("#999999"));
    }

    #[test]
    fn deserializes_state_without_optional_fields() {
        let value = json!([{
            "id": "W1", "isFront": true,
            "tabs": [{
                "index": 1, "id": "W1-1", "tabId": "27", "isSelected": true,
                "sessions": [{"id": "S1"}]
            }]
        }]);
        let parsed: Vec<WindowState> = serde_json::from_value(value).unwrap();
        let tab = &parsed[0].tabs[0];
        assert_eq!(tab.title, "");
        assert_eq!(tab.current_session_id, "");
        assert_eq!(tab.sessions[0].name, "");
        assert_eq!(tab.sessions[0].rect, None);
    }

    #[test]
    fn cursor_reports_absolute_line_and_char_index() {
        let c = content(50, vec![vec![run("x")], vec![run("❯ "), cursor_run()]], true, 52);
        assert_eq!(c.cursor(), Some((51, 2)));
    }

    #[test]
    fn cell_reads_across_runs_and_dim() {
        let dim = Run { t: "fix".into(), d: true, ..Run::default() };
        let c = content(10, vec![vec![run("❯ "), cursor_run(), dim]], true, 11);
        assert_eq!(c.cell(10, 0), Some(Cell { ch: '❯', dim: false }));
        assert_eq!(c.cell(10, 2), Some(Cell { ch: 'f', dim: true }));
        assert_eq!(c.cell(10, 9), None);
        assert_eq!(c.cell(11, 0), None);
        assert_eq!(c.line_text(10).as_deref(), Some("❯ fix"));
    }

    #[test]
    fn link_at_finds_the_run_url_under_a_cell() {
        let link = Run { t: "docs".into(), u: Some("https://example.com/docs".into()), ..Run::default() };
        let c = content(5, vec![vec![run("see "), link, run(" now")]], true, 6);
        assert_eq!(c.link_at(5, 4).as_deref(), Some("https://example.com/docs"));
        assert_eq!(c.link_at(5, 7).as_deref(), Some("https://example.com/docs"));
        assert_eq!(c.link_at(5, 8), None);
        assert_eq!(c.link_at(5, 0), None);
    }

    #[test]
    fn merge_without_current_returns_incoming() {
        let incoming = content(1000, numbered(1000, 1010, Some(1009)), true, 1010);
        assert_eq!(merge_content(None, incoming.clone()), incoming);
    }

    #[test]
    fn merge_live_tail_replaces_tail_and_strips_old_cursor() {
        let current = content(1000, numbered(1000, 1010, Some(1009)), true, 1010);
        let mut tail = numbered(1005, 1012, Some(1011));
        tail[0] = vec![run("new-1005")];
        let incoming = content(1005, tail, true, 1012);
        let merged = merge_content(Some(&current), incoming);
        assert_eq!(merged.first_line, 1000);
        assert_eq!(merged.lines.len(), 12);
        assert_eq!(cursor_count(&merged), 1);
        assert_eq!(merged.cursor(), Some((1011, 4)));
        assert_eq!(merged.line_text(1004).as_deref(), Some("1004"));
        assert_eq!(merged.line_text(1005).as_deref(), Some("new-1005"));
    }

    #[test]
    fn merge_gap_returns_incoming() {
        let current = content(1000, numbered(1000, 1010, None), true, 1010);
        let incoming = content(1020, numbered(1020, 1030, Some(1029)), true, 1030);
        assert_eq!(merge_content(Some(&current), incoming.clone()), incoming);
    }

    #[test]
    fn merge_history_prepends() {
        let current = content(1000, numbered(1000, 1010, Some(1009)), true, 1010);
        let history = content(900, numbered(900, 1000, None), false, 1010);
        let merged = merge_content(Some(&current), history);
        assert_eq!(merged.first_line, 900);
        assert_eq!(merged.lines.len(), 110);
        assert!(merged.is_latest);
        assert_eq!(merged.cursor(), Some((1009, 4)));
        assert_eq!(merged.line_text(900).as_deref(), Some("900"));
    }
}
