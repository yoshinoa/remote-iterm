use crate::config::Config;
use crate::connection::{Command, ConnHandle, ServerEvent};
use crate::protocol::{Content, Run, SessionContent, SessionInfo, TabState, WindowState};
use crate::ui::root::RootView;
use futures::channel::mpsc::UnboundedSender;
use gpui::{Entity, TestAppContext, VisualTestContext};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

pub(crate) struct Harness<'a> {
    pub view: Entity<RootView>,
    pub cx: &'a mut VisualTestContext,
    pub events: UnboundedSender<ServerEvent>,
    pub commands: UnboundedReceiver<Command>,
}

impl Harness<'_> {
    pub fn send(&mut self, event: ServerEvent) {
        self.events.unbounded_send(event).unwrap();
        self.cx.run_until_parked();
    }

    pub fn sent(&mut self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        while let Ok(cmd) = self.commands.try_recv() {
            if let Command::Emit { event, data } = cmd {
                out.push((event.to_string(), data.unwrap_or(Value::Null)));
            }
        }
        out
    }
}

pub(crate) fn harness(cx: &mut TestAppContext) -> Harness<'_> {
    let (handle, commands) = ConnHandle::recording();
    let (events, rx) = futures::channel::mpsc::unbounded();
    let (view, vcx) = cx.add_window_view(move |window, cx| RootView::attached(Config::default(), handle, rx, window, cx));
    Harness { view, cx: vcx, events, commands }
}

pub(crate) fn state() -> Vec<WindowState> {
    let tab = |index: u32, sid: &str, selected: bool| TabState {
        index,
        id: format!("W1-{index}"),
        tab_id: format!("t{index}"),
        title: format!("tab {index}"),
        is_selected: selected,
        current_session_id: sid.into(),
        sessions: vec![SessionInfo { id: sid.into(), name: String::new(), rect: None }],
    };
    vec![WindowState { id: "W1".into(), is_front: true, tabs: vec![tab(1, "s1", true), tab(2, "s2", false), tab(3, "s3", false)] }]
}

pub(crate) fn prompt(session: &str, typed: &str) -> SessionContent {
    let run = |t: &str| Run { t: t.into(), ..Run::default() };
    SessionContent {
        session_id: session.into(),
        content: Content {
            lines: vec![vec![run("previous output")], vec![run(&format!("❯ {typed}")), Run { c: true, ..Run::default() }]],
            fg: "#ffffff".into(),
            bg: "#000000".into(),
            first_line: 100,
            available_first_line: 0,
            terminal_end: 102,
            is_latest: true,
            mouse: false,
            screen_first_line: Some(62),
            columns: Some(80),
            rows: Some(40),
        },
    }
}

#[gpui::test]
fn state_and_content_watch_and_show_the_selected_session(cx: &mut TestAppContext) {
    let mut h = harness(cx);
    h.send(ServerEvent::State(state()));
    let sent = h.sent();
    assert!(sent.contains(&("watch".into(), json!({"sessionIds": ["s1"]}))));
    assert!(sent.contains(&("getContent".into(), json!({"sessionId": "s1"}))));
    h.send(ServerEvent::Content(prompt("s1", "hi")));
    let shown = h.view.read_with(h.cx, |view, _| view.model.shown().and_then(|c| c.line_text(101)));
    assert_eq!(shown.as_deref(), Some("❯ hi"));
}

#[gpui::test]
fn reconnect_rewatches_the_shown_session(cx: &mut TestAppContext) {
    let mut h = harness(cx);
    h.send(ServerEvent::State(state()));
    h.sent();
    h.send(ServerEvent::Disconnected);
    h.send(ServerEvent::Connected);
    let sent = h.sent();
    assert!(sent.contains(&("watch".into(), json!({"sessionIds": ["s1"]}))));
    assert!(sent.contains(&("getContent".into(), json!({"sessionId": "s1"}))));
}
