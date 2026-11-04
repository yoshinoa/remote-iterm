use crate::protocol::{merge_content, Content, SessionContent, TabState, WindowState};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Watch(Vec<String>),
    GetContent(String),
    Focus { window_id: String, tab_index: u32 },
    ResetPrediction,
}

#[derive(Debug, Default)]
pub struct Model {
    pub windows: Vec<WindowState>,
    pub window_id: Option<String>,
    pub tab_id: Option<String>,
    pub session_id: Option<String>,
    pub contents: HashMap<String, Content>,
    prev_front: Option<String>,
    explicit_session: bool,
}

fn selected_tab(window: &WindowState) -> Option<&TabState> {
    window.tabs.iter().find(|t| t.is_selected).or_else(|| window.tabs.first())
}

fn default_session(tab: &TabState) -> Option<String> {
    if !tab.current_session_id.is_empty() {
        return Some(tab.current_session_id.clone());
    }
    tab.sessions.first().map(|s| s.id.clone())
}

impl Model {
    fn window(&self) -> Option<&WindowState> {
        let id = self.window_id.as_deref()?;
        self.windows.iter().find(|w| w.id == id)
    }

    pub fn current_tabs(&self) -> &[TabState] {
        self.window().map(|w| w.tabs.as_slice()).unwrap_or(&[])
    }

    pub fn current_tab(&self) -> Option<&TabState> {
        let id = self.tab_id.as_deref()?;
        self.current_tabs().iter().find(|t| t.id == id)
    }

    pub fn shown(&self) -> Option<&Content> {
        self.contents.get(self.session_id.as_deref()?)
    }

    fn show_window_tab(&mut self, window_id: String, tab: Option<(String, Option<String>)>) {
        self.window_id = Some(window_id);
        let (tab_id, session) = tab.unzip();
        self.tab_id = tab_id;
        self.session_id = session.flatten();
        self.explicit_session = false;
    }

    fn pick(window: &WindowState) -> Option<(String, Option<String>)> {
        selected_tab(window).map(|t| (t.id.clone(), default_session(t)))
    }

    fn session_effects(&self, before: &Option<String>) -> Vec<Effect> {
        match &self.session_id {
            Some(sid) if Some(sid) != before.as_ref() => vec![
                Effect::ResetPrediction,
                Effect::Watch(vec![sid.clone()]),
                Effect::GetContent(sid.clone()),
            ],
            _ => Vec::new(),
        }
    }

    fn focus_effect(&self) -> Option<Effect> {
        Some(Effect::Focus { window_id: self.window_id.clone()?, tab_index: self.current_tab()?.index })
    }

    fn after_click(&self, before: &Option<String>) -> Vec<Effect> {
        let mut effects: Vec<Effect> = self.focus_effect().into_iter().collect();
        effects.extend(self.session_effects(before));
        effects
    }

    pub fn apply_state(&mut self, windows: Vec<WindowState>) -> Vec<Effect> {
        let before = self.session_id.clone();
        self.windows = windows;
        if self.windows.is_empty() {
            return Vec::new();
        }
        let front = self.windows.iter().find(|w| w.is_front).map(|w| w.id.clone());
        let known_window = self.window().is_some();

        if !known_window {
            let win = front.clone().unwrap_or_else(|| self.windows[0].id.clone());
            let pick = self.windows.iter().find(|w| w.id == win).and_then(Self::pick);
            self.show_window_tab(win, pick);
            self.prev_front = front;
        } else if front.is_some() && front != self.prev_front {
            self.prev_front = front.clone();
            let win = front.unwrap();
            let pick = self.windows.iter().find(|w| w.id == win).and_then(Self::pick);
            self.show_window_tab(win, pick);
        } else if let Some(win) = self.window() {
            let mac_tab = win.tabs.iter().find(|t| t.is_selected).map(|t| t.id.clone());
            let tab_exists = self.tab_id.as_ref().is_some_and(|id| win.tabs.iter().any(|t| &t.id == id));
            let follow = match mac_tab {
                Some(ref id) if Some(id) != self.tab_id.as_ref() => Some(id.clone()),
                _ if !tab_exists => selected_tab(win).map(|t| t.id.clone()),
                _ => None,
            };
            if let Some(tab_id) = follow {
                let session = win.tabs.iter().find(|t| t.id == tab_id).and_then(default_session);
                let window_id = win.id.clone();
                self.show_window_tab(window_id, Some((tab_id, session)));
            }
        }

        if let Some(tab) = self.current_tab() {
            let valid = self.session_id.as_ref().is_some_and(|sid| tab.sessions.iter().any(|s| &s.id == sid));
            if !valid {
                self.session_id = default_session(tab);
                self.explicit_session = false;
            }
        }
        self.session_effects(&before)
    }

    pub fn select_window(&mut self, window_id: &str) -> Vec<Effect> {
        let before = self.session_id.clone();
        let Some(pick) = self.windows.iter().find(|w| w.id == window_id).map(Self::pick) else {
            return Vec::new();
        };
        self.show_window_tab(window_id.to_string(), pick);
        self.after_click(&before)
    }

    pub fn select_tab(&mut self, tab_id: &str) -> Vec<Effect> {
        let before = self.session_id.clone();
        let Some(tab) = self.current_tabs().iter().find(|t| t.id == tab_id) else {
            return Vec::new();
        };
        let session = default_session(tab);
        let window_id = self.window_id.clone().unwrap_or_default();
        self.show_window_tab(window_id, Some((tab_id.to_string(), session)));
        self.after_click(&before)
    }

    pub fn select_tab_number(&mut self, n: u8) -> Vec<Effect> {
        let tabs = self.current_tabs();
        let tab = match n {
            9 => tabs.last(),
            1..=8 => tabs.get(usize::from(n) - 1),
            _ => None,
        };
        match tab.map(|t| t.id.clone()) {
            Some(id) => self.select_tab(&id),
            None => Vec::new(),
        }
    }

    pub fn select_session(&mut self, session_id: &str) -> Vec<Effect> {
        let known = self.current_tab().is_some_and(|t| t.sessions.iter().any(|s| s.id == session_id));
        if !known {
            return Vec::new();
        }
        let before = self.session_id.clone();
        self.session_id = Some(session_id.to_string());
        self.explicit_session = true;
        self.session_effects(&before)
    }

    pub fn on_connected(&self) -> Vec<Effect> {
        match &self.session_id {
            Some(sid) => vec![Effect::Watch(vec![sid.clone()]), Effect::GetContent(sid.clone())],
            None => Vec::new(),
        }
    }

    pub fn apply_content(&mut self, sc: SessionContent) -> bool {
        let merged = merge_content(self.contents.get(&sc.session_id), sc.content);
        let shown = self.session_id.as_deref() == Some(sc.session_id.as_str());
        self.contents.insert(sc.session_id, merged);
        shown
    }

    pub fn apply_history(&mut self, sc: SessionContent) -> bool {
        self.apply_content(sc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Content, Run, SessionInfo};

    fn tab(window: &str, index: u32, selected: bool, sessions: &[&str], current: &str) -> TabState {
        TabState {
            index,
            id: format!("{window}-{index}"),
            tab_id: format!("t{window}{index}"),
            title: format!("tab {index}"),
            is_selected: selected,
            current_session_id: current.into(),
            sessions: sessions.iter().map(|id| SessionInfo { id: (*id).into(), name: String::new(), rect: None }).collect(),
        }
    }

    fn window(id: &str, front: bool, tabs: Vec<TabState>) -> WindowState {
        WindowState { id: id.into(), is_front: front, tabs }
    }

    fn state_a() -> Vec<WindowState> {
        vec![
            window("W0", false, vec![tab("W0", 1, true, &["s0"], "s0")]),
            window("W1", true, vec![
                tab("W1", 1, true, &["s1a", "s1b"], "s1b"),
                tab("W1", 2, false, &["s2"], "s2"),
                tab("W1", 3, false, &["s3"], "s3"),
            ]),
        ]
    }

    fn loaded() -> Model {
        let mut m = Model::default();
        m.apply_state(state_a());
        m
    }

    fn content(text: &str) -> Content {
        Content {
            lines: vec![vec![Run { t: text.into(), ..Run::default() }]],
            fg: "#fff".into(),
            bg: "#000".into(),
            first_line: 10,
            available_first_line: 0,
            terminal_end: 11,
            is_latest: true,
            mouse: false,
            screen_first_line: None,
            columns: Some(80),
            rows: Some(24),
        }
    }

    fn has_focus(effects: &[Effect]) -> bool {
        effects.iter().any(|e| matches!(e, Effect::Focus { .. }))
    }

    #[test]
    fn first_state_selects_front_window_tab_and_session() {
        let mut m = Model::default();
        let effects = m.apply_state(state_a());
        assert_eq!(m.window_id.as_deref(), Some("W1"));
        assert_eq!(m.tab_id.as_deref(), Some("W1-1"));
        assert_eq!(m.session_id.as_deref(), Some("s1b"));
        assert!(effects.contains(&Effect::Watch(vec!["s1b".into()])));
        assert!(effects.contains(&Effect::GetContent("s1b".into())));
        assert!(effects.contains(&Effect::ResetPrediction));
        assert!(!has_focus(&effects));
    }

    #[test]
    fn follows_mac_tab_switch_without_focus_effect() {
        let mut m = loaded();
        let mut next = state_a();
        next[1].tabs[0].is_selected = false;
        next[1].tabs[1].is_selected = true;
        let effects = m.apply_state(next);
        assert_eq!(m.tab_id.as_deref(), Some("W1-2"));
        assert_eq!(m.session_id.as_deref(), Some("s2"));
        assert!(effects.contains(&Effect::Watch(vec!["s2".into()])));
        assert!(!has_focus(&effects));
    }

    #[test]
    fn follows_front_window_change() {
        let mut m = loaded();
        let mut next = state_a();
        next[1].is_front = false;
        next.push(window("W2", true, vec![tab("W2", 1, true, &["s9"], "s9")]));
        m.apply_state(next);
        assert_eq!(m.window_id.as_deref(), Some("W2"));
        assert_eq!(m.session_id.as_deref(), Some("s9"));
    }

    #[test]
    fn keeps_explicit_pane_while_tab_unchanged() {
        let mut m = loaded();
        let effects = m.select_session("s1a");
        assert!(effects.contains(&Effect::Watch(vec!["s1a".into()])));
        let effects = m.apply_state(state_a());
        assert_eq!(m.session_id.as_deref(), Some("s1a"));
        assert!(effects.is_empty());
    }

    #[test]
    fn falls_back_when_shown_pane_vanishes() {
        let mut m = loaded();
        m.select_session("s1a");
        let mut next = state_a();
        next[1].tabs[0].sessions.retain(|s| s.id == "s1b");
        m.apply_state(next);
        assert_eq!(m.session_id.as_deref(), Some("s1b"));
    }

    #[test]
    fn tab_number_selects_nth_and_nine_is_last() {
        let mut m = loaded();
        let effects = m.select_tab_number(9);
        assert_eq!(m.tab_id.as_deref(), Some("W1-3"));
        assert!(effects.contains(&Effect::Focus { window_id: "W1".into(), tab_index: 3 }));
        let effects = m.select_tab_number(2);
        assert_eq!(m.tab_id.as_deref(), Some("W1-2"));
        assert!(effects.contains(&Effect::Focus { window_id: "W1".into(), tab_index: 2 }));
        assert_eq!(m.select_tab_number(5), vec![]);
        assert_eq!(m.select_tab_number(0), vec![]);
    }

    #[test]
    fn select_tab_emits_focus_and_watch() {
        let mut m = loaded();
        let effects = m.select_tab("W1-2");
        assert!(effects.contains(&Effect::Focus { window_id: "W1".into(), tab_index: 2 }));
        assert!(effects.contains(&Effect::Watch(vec!["s2".into()])));
        assert!(effects.contains(&Effect::GetContent("s2".into())));
        assert!(effects.contains(&Effect::ResetPrediction));
    }

    #[test]
    fn select_window_follows_its_selected_tab() {
        let mut m = loaded();
        let effects = m.select_window("W0");
        assert_eq!(m.session_id.as_deref(), Some("s0"));
        assert!(effects.contains(&Effect::Focus { window_id: "W0".into(), tab_index: 1 }));
    }

    #[test]
    fn apply_content_merges_and_reports_shown() {
        let mut m = loaded();
        let shown = SessionContent { session_id: "s1b".into(), content: content("hello") };
        assert!(m.apply_content(shown));
        assert_eq!(m.shown().and_then(|c| c.line_text(10)).as_deref(), Some("hello"));
        let other = SessionContent { session_id: "s2".into(), content: content("bg") };
        assert!(!m.apply_content(other));
        assert!(m.contents.contains_key("s2"));
    }

    #[test]
    fn connected_rewatches_shown_session() {
        let m = loaded();
        assert_eq!(m.on_connected(), vec![Effect::Watch(vec!["s1b".into()]), Effect::GetContent("s1b".into())]);
    }
}
