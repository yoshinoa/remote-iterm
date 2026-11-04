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

fn ready(cx: &mut TestAppContext) -> Harness<'_> {
    let mut h = harness(cx);
    h.send(ServerEvent::State(state()));
    h.send(ServerEvent::Content(prompt("s1", "")));
    h.sent();
    h
}

fn keys_sent(sent: &[(String, Value)]) -> Vec<String> {
    sent.iter()
        .filter(|(event, _)| event == "sendKeys")
        .map(|(_, data)| {
            assert_eq!(data["sessionId"], "s1");
            data["keys"].as_str().unwrap().to_string()
        })
        .collect()
}

fn overlay(h: &Harness) -> Vec<(i64, usize, char)> {
    h.view.read_with(h.cx, |view, _| view.predictor.overlay().iter().map(|o| (o.line, o.col, o.ch)).collect())
}

#[gpui::test]
fn typed_keys_are_sent_in_order(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    h.cx.simulate_keystrokes("a b ctrl-w enter backspace up");
    let sent = h.sent();
    assert_eq!(keys_sent(&sent), vec!["a", "b", "\x17", "\r", "\x7f", "\x1b[A"]);
}

#[gpui::test]
fn ctrl_number_switches_tab(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    h.cx.simulate_keystrokes("ctrl-2");
    let sent = h.sent();
    assert!(sent.contains(&("focus".into(), json!({"windowId": "W1", "tabIndex": 2}))));
    assert!(sent.contains(&("watch".into(), json!({"sessionIds": ["s2"]}))));
    assert!(sent.iter().all(|(event, _)| event != "sendKeys"));
    h.cx.simulate_keystrokes("ctrl-9");
    assert!(h.sent().contains(&("focus".into(), json!({"windowId": "W1", "tabIndex": 3}))));
}

#[gpui::test]
fn prediction_draws_after_first_echo_and_enter_resets(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    h.cx.simulate_keystrokes("a");
    assert!(overlay(&h).is_empty());
    h.send(ServerEvent::Content(prompt("s1", "a")));
    h.cx.simulate_keystrokes("b c");
    assert_eq!(overlay(&h), vec![(101, 3, 'b'), (101, 4, 'c')]);
    h.cx.simulate_keystrokes("enter");
    assert!(overlay(&h).is_empty());
}

#[gpui::test]
fn ctrl_v_pastes_text_bracketed(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    h.cx.write_to_clipboard(gpui::ClipboardItem::new_string("echo a\necho b".into()));
    h.cx.simulate_keystrokes("ctrl-v");
    assert_eq!(keys_sent(&h.sent()), vec!["\x1b[200~echo a\necho b\x1b[201~"]);
}

#[gpui::test]
fn ctrl_v_with_image_sends_paste_image(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    let image = gpui::Image::from_bytes(gpui::ImageFormat::Png, vec![0x89, b'P', b'N', b'G']);
    h.cx.write_to_clipboard(gpui::ClipboardItem::new_image(&image));
    h.cx.simulate_keystrokes("ctrl-v");
    let sent = h.sent();
    let (_, data) = sent.iter().find(|(event, _)| event == "pasteImage").expect("pasteImage sent");
    assert_eq!(data["sessionId"], "s1");
    assert_eq!(data["data"], "data:image/png;base64,iVBORw==");
    assert!(keys_sent(&sent).is_empty());
}

#[gpui::test]
fn ctrl_v_with_empty_clipboard_sends_literal(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    h.cx.simulate_keystrokes("ctrl-v");
    assert_eq!(keys_sent(&h.sent()), vec!["\x16"]);
}
