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

    pub fn answer(&mut self, reply: impl Fn(&str) -> Option<Value>) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        while let Ok(cmd) = self.commands.try_recv() {
            match cmd {
                Command::Emit { event, data } => out.push((event.to_string(), data.unwrap_or(Value::Null))),
                Command::Ack { event, data, reply: tx } => {
                    let _ = tx.send(reply(event));
                    out.push((format!("ack:{event}"), data.unwrap_or(Value::Null)));
                }
                _ => {}
            }
        }
        self.cx.run_until_parked();
        out
    }

    pub fn sent(&mut self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        while let Ok(cmd) = self.commands.try_recv() {
            match cmd {
                Command::Emit { event, data } => out.push((event.to_string(), data.unwrap_or(Value::Null))),
                Command::Ack { event, data, .. } => out.push((format!("ack:{event}"), data.unwrap_or(Value::Null))),
                Command::ReconnectNow => out.push(("reconnect".into(), Value::Null)),
                _ => {}
            }
        }
        out
    }
}

fn harness_with_key<'a>(cx: &'a mut TestAppContext, key: Option<&str>) -> Harness<'a> {
    let (handle, commands) = ConnHandle::recording();
    let (events, rx) = futures::channel::mpsc::unbounded();
    let config = Config { key: key.map(str::to_string), ..Config::default() };
    let path = std::env::temp_dir()
        .join(format!("remote-iterm-ui-test-{}-{}", std::process::id(), NEXT_DIR.fetch_add(1, std::sync::atomic::Ordering::SeqCst)))
        .join("desktop.json");
    let (view, vcx) = cx.add_window_view(move |window, cx| RootView::attached(config, path, handle, rx, window, cx));
    Harness { view, cx: vcx, events, commands }
}

static NEXT_DIR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(crate) fn harness(cx: &mut TestAppContext) -> Harness<'_> {
    harness_with_key(cx, Some("test-key"))
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

fn cell_point(h: &mut Harness, row: usize, col: usize) -> gpui::Point<gpui::Pixels> {
    h.cx.run_until_parked();
    let (bounds, cw) = h.view.read_with(h.cx, |v, _| (v.term_bounds.get().expect("terminal laid out"), v.cell_width));
    gpui::point(
        bounds.origin.x + cw * (col as f32 + 0.5),
        bounds.origin.y + gpui::px(crate::ui::theme::LINE_HEIGHT) * (row as f32 + 0.5),
    )
}

fn mouse_content(session: &str) -> SessionContent {
    let mut c = prompt(session, "hi");
    c.content.mouse = true;
    c
}

fn wheel(h: &mut Harness, at: gpui::Point<gpui::Pixels>, lines: f32) {
    h.cx.simulate_event(gpui::ScrollWheelEvent {
        position: at,
        delta: gpui::ScrollDelta::Lines(gpui::point(0.0, lines)),
        modifiers: gpui::Modifiers::none(),
        touch_phase: gpui::TouchPhase::Moved,
    });
}

fn drag(h: &mut Harness, from: gpui::Point<gpui::Pixels>, to: gpui::Point<gpui::Pixels>) {
    let none = gpui::Modifiers::none();
    h.cx.simulate_mouse_down(from, gpui::MouseButton::Left, none);
    h.cx.simulate_event(gpui::MouseMoveEvent { position: to, pressed_button: Some(gpui::MouseButton::Left), modifiers: none });
    h.cx.simulate_mouse_up(to, gpui::MouseButton::Left, none);
}

#[gpui::test]
fn mouse_mode_forwards_press_drag_release_and_copies_back(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    h.send(ServerEvent::Content(mouse_content("s1")));
    h.sent();
    let start = cell_point(&mut h, 1, 2);
    let end = cell_point(&mut h, 1, 4);
    drag(&mut h, start, end);
    let first = h.answer(|event| (event == "clipboardCount").then(|| json!(7)));
    assert_eq!(keys_sent(&first), vec!["\x1b[<0;3;40M", "\x1b[<32;5;40M", "\x1b[<0;5;40m"]);
    assert!(first.iter().any(|(e, _)| e == "ack:clipboardCount"));
    let second = h.answer(|event| (event == "clipboardSince").then(|| json!("copied")));
    assert!(second.contains(&("ack:clipboardSince".into(), json!({"count": 7}))));
    assert_eq!(h.cx.read_from_clipboard().and_then(|i| i.text()).as_deref(), Some("copied"));
}

#[gpui::test]
fn wheel_in_mouse_mode_sends_wheel_reports(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    h.send(ServerEvent::Content(mouse_content("s1")));
    h.sent();
    let at = cell_point(&mut h, 1, 2);
    wheel(&mut h, at, 2.0);
    wheel(&mut h, at, -1.0);
    assert_eq!(keys_sent(&h.sent()), vec!["\x1b[<64;3;40M", "\x1b[<64;3;40M", "\x1b[<65;3;40M"]);
}

#[gpui::test]
fn drag_selects_and_ctrl_c_copies_without_sending(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    let a = cell_point(&mut h, 0, 0);
    let b = cell_point(&mut h, 0, 8);
    drag(&mut h, a, b);
    h.cx.simulate_keystrokes("ctrl-c");
    assert!(keys_sent(&h.sent()).is_empty());
    assert_eq!(h.cx.read_from_clipboard().and_then(|i| i.text()).as_deref(), Some("previous"));
    h.cx.simulate_keystrokes("ctrl-c");
    assert_eq!(keys_sent(&h.sent()), vec!["\x03"]);
}

#[gpui::test]
fn typing_clears_the_selection(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    let a = cell_point(&mut h, 0, 0);
    let b = cell_point(&mut h, 0, 4);
    drag(&mut h, a, b);
    assert!(h.view.read_with(h.cx, |v, _| v.selection.is_some()));
    h.cx.simulate_keystrokes("x");
    assert!(h.view.read_with(h.cx, |v, _| v.selection.is_none()));
    assert_eq!(keys_sent(&h.sent()), vec!["x"]);
}

#[gpui::test]
fn wheel_at_top_requests_history_once_per_page(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    let at = cell_point(&mut h, 0, 0);
    wheel(&mut h, at, 3.0);
    wheel(&mut h, at, 3.0);
    let requests: Vec<Value> = h.sent().into_iter().filter(|(e, _)| e == "getEarlierContent").map(|(_, d)| d).collect();
    assert_eq!(requests, vec![json!({"sessionId": "s1", "beforeLine": 100})]);
    let mut page = prompt("s1", "");
    page.content.first_line = 90;
    page.content.is_latest = false;
    page.content.lines = (90..100).map(|n| vec![Run { t: format!("old {n}"), ..Run::default() }]).collect();
    h.send(ServerEvent::History(page));
    let first = h.view.read_with(h.cx, |v, _| v.model.shown().map(|c| c.first_line));
    assert_eq!(first, Some(90));
    let at = cell_point(&mut h, 0, 0);
    wheel(&mut h, at, 20.0);
    let requests: Vec<Value> = h.sent().into_iter().filter(|(e, _)| e == "getEarlierContent").map(|(_, d)| d).collect();
    assert_eq!(requests, vec![json!({"sessionId": "s1", "beforeLine": 90})]);
}

#[gpui::test]
fn ctrl_click_opens_link(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    let mut c = prompt("s1", "");
    c.content.lines[0] = vec![
        Run { t: "see ".into(), ..Run::default() },
        Run { t: "docs".into(), u: Some("https://example.com/docs".into()), ..Run::default() },
    ];
    h.send(ServerEvent::Content(c));
    let at = cell_point(&mut h, 0, 5);
    h.cx.simulate_click(at, gpui::Modifiers::control());
    assert_eq!(h.cx.opened_url().as_deref(), Some("https://example.com/docs"));
    assert!(keys_sent(&h.sent()).is_empty());
}

fn redraw(h: &mut Harness) {
    h.cx.run_until_parked();
    h.cx.simulate_event(gpui::MouseMoveEvent {
        position: gpui::point(gpui::px(1.0), gpui::px(1.0)),
        pressed_button: None,
        modifiers: gpui::Modifiers::none(),
    });
}

fn click(h: &mut Harness, selector: &'static str) {
    redraw(h);
    let bounds = h.cx.debug_bounds(selector).unwrap_or_else(|| panic!("no element {selector}"));
    h.cx.simulate_click(bounds.center(), gpui::Modifiers::none());
}

fn exists(h: &mut Harness, selector: &'static str) -> bool {
    redraw(h);
    h.cx.debug_bounds(selector).is_some()
}

#[gpui::test]
fn key_screen_takes_a_pasted_link_and_saves_it(cx: &mut TestAppContext) {
    let mut h = harness_with_key(cx, None);
    assert!(exists(&mut h, "key-screen"));
    h.cx.write_to_clipboard(gpui::ClipboardItem::new_string("https://h:7292/#key=abc123".into()));
    h.cx.simulate_keystrokes("ctrl-v enter");
    let (key, path) = h.view.read_with(h.cx, |v, _| (v.config.key.clone(), v.config_path.clone()));
    assert_eq!(key.as_deref(), Some("abc123"));
    assert_eq!(crate::config::load(&path).key.as_deref(), Some("abc123"));
    assert!(!h.view.read_with(h.cx, |v, _| v.key_screen_visible()));
    assert!(keys_sent(&h.sent()).is_empty());
}

#[gpui::test]
fn key_screen_typing_and_backspace(cx: &mut TestAppContext) {
    let mut h = harness_with_key(cx, None);
    h.cx.simulate_keystrokes("a b x backspace c enter");
    assert_eq!(h.view.read_with(h.cx, |v, _| v.config.key.clone()).as_deref(), Some("abc"));
}

#[gpui::test]
fn rejected_key_brings_back_the_key_screen(cx: &mut TestAppContext) {
    let mut h = harness(cx);
    assert!(!exists(&mut h, "key-screen"));
    h.send(ServerEvent::AuthRejected);
    assert!(exists(&mut h, "key-screen"));
    assert!(exists(&mut h, "key-rejected"));
}

#[gpui::test]
fn reconnect_overlay_appears_after_first_connect_and_retries(cx: &mut TestAppContext) {
    let mut h = harness(cx);
    h.send(ServerEvent::Disconnected);
    assert!(!exists(&mut h, "retry"));
    h.send(ServerEvent::Connected);
    assert!(!exists(&mut h, "retry"));
    h.send(ServerEvent::Disconnected);
    click(&mut h, "retry");
    assert!(h.sent().contains(&("reconnect".into(), Value::Null)));
}

#[gpui::test]
fn tab_bar_close_and_new_buttons(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    click(&mut h, "close-tab-2");
    let sent = h.sent();
    assert!(sent.contains(&("closeTab".into(), json!({"tabId": "t2"}))));
    assert!(sent.iter().all(|(e, _)| e != "focus"));
    click(&mut h, "new-tab");
    assert!(h.sent().contains(&("newTab".into(), Value::Null)));
}

#[gpui::test]
fn lone_tab_has_no_close_button(cx: &mut TestAppContext) {
    let mut h = harness(cx);
    let mut s = state();
    s[0].tabs.truncate(1);
    h.send(ServerEvent::State(s));
    assert!(exists(&mut h, "new-tab"));
    assert!(!exists(&mut h, "close-tab-1"));
}

#[gpui::test]
fn pane_switcher_changes_the_watched_session(cx: &mut TestAppContext) {
    let mut h = harness(cx);
    let mut s = state();
    s[0].tabs[0].sessions.push(SessionInfo { id: "s1b".into(), name: "right".into(), rect: None });
    h.send(ServerEvent::State(s));
    h.sent();
    click(&mut h, "pane-2");
    assert!(h.sent().contains(&("watch".into(), json!({"sessionIds": ["s1b"]}))));
}

#[gpui::test]
fn window_picker_switches_window(cx: &mut TestAppContext) {
    let mut h = harness(cx);
    let mut s = state();
    s.push(WindowState {
        id: "W2".into(),
        is_front: false,
        tabs: vec![TabState {
            index: 1,
            id: "W2-1".into(),
            tab_id: "u1".into(),
            title: "other".into(),
            is_selected: true,
            current_session_id: "s9".into(),
            sessions: vec![SessionInfo { id: "s9".into(), name: String::new(), rect: None }],
        }],
    });
    h.send(ServerEvent::State(s));
    h.sent();
    click(&mut h, "window-2");
    assert!(h.sent().contains(&("focus".into(), json!({"windowId": "W2", "tabIndex": 1}))));
}

#[gpui::test]
fn mac_clipboard_button_copies_text(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    click(&mut h, "mac-clipboard");
    let sent = h.answer(|event| (event == "clipboardText").then(|| json!("from pbcopy")));
    assert!(sent.iter().any(|(e, _)| e == "ack:clipboardText"));
    assert_eq!(h.cx.read_from_clipboard().and_then(|i| i.text()).as_deref(), Some("from pbcopy"));
    assert_eq!(h.view.read_with(h.cx, |v, _| v.clip_status), Some(true));
}

#[gpui::test]
fn mac_clipboard_button_reports_nothing_to_copy(cx: &mut TestAppContext) {
    let mut h = ready(cx);
    click(&mut h, "mac-clipboard");
    h.answer(|_| None);
    assert_eq!(h.cx.read_from_clipboard(), None);
    assert_eq!(h.view.read_with(h.cx, |v, _| v.clip_status), Some(false));
}
