use crate::protocol::{SessionContent, WindowState};
use futures::FutureExt;
use futures::channel::{mpsc, oneshot};
use rust_socketio::asynchronous::{Client, ClientBuilder};
use rust_socketio::{Event, Payload, TransportType};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc as tmpsc;

const ACK_TIMEOUT: Duration = Duration::from_secs(5);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(1);
const HEARTBEAT_LOST_AFTER: Duration = Duration::from_secs(4);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum ServerEvent {
    Connected,
    Disconnected,
    AuthRejected,
    State(Vec<WindowState>),
    Content(SessionContent),
    History(SessionContent),
}

#[derive(Debug)]
pub(crate) enum Command {
    Emit { event: &'static str, data: Option<Value> },
    Ack { event: &'static str, data: Option<Value>, reply: oneshot::Sender<Option<Value>> },
    ReconnectNow,
    Shutdown,
}

enum Link {
    Connected,
    Closed,
    AuthRejected,
    Pong,
    Event(ServerEvent),
}

enum Outcome {
    Lost,
    AuthRejected,
    Shutdown,
}

#[derive(Clone)]
pub struct ConnHandle {
    tx: tmpsc::UnboundedSender<Command>,
}

pub(crate) fn parse_event(event: &str, payload: &Value) -> Option<ServerEvent> {
    match event {
        "state" => serde_json::from_value(payload.clone()).ok().map(ServerEvent::State),
        "content" => serde_json::from_value(payload.clone()).ok().map(ServerEvent::Content),
        "historyContent" => serde_json::from_value(payload.clone()).ok().map(ServerEvent::History),
        _ => None,
    }
}

pub(crate) fn is_auth_error(message: &str) -> bool {
    message.contains("invalid access key")
}

#[allow(deprecated)]
fn first_value(payload: &Payload) -> Option<Value> {
    match payload {
        Payload::Text(values) => values.first().cloned(),
        Payload::String(s) => Some(serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone()))),
        Payload::Binary(_) => None,
    }
}

pub(crate) fn ack_value(payload: &Payload) -> Option<Value> {
    match first_value(payload)? {
        Value::Array(args) => args.into_iter().next().filter(|v| !v.is_null()),
        other => Some(other).filter(|v| !v.is_null()),
    }
}

fn payload_text(payload: &Payload) -> String {
    match first_value(payload) {
        Some(Value::String(s)) => s,
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn to_payload(data: Option<Value>) -> Payload {
    Payload::Text(data.into_iter().collect())
}

pub fn start(url: String, key: String) -> (ConnHandle, mpsc::UnboundedReceiver<ServerEvent>) {
    let (cmd_tx, cmd_rx) = tmpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded();
    std::thread::Builder::new()
        .name("remote-iterm-connection".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            runtime.block_on(supervise(url, key, cmd_rx, ev_tx));
        })
        .expect("connection thread");
    (ConnHandle { tx: cmd_tx }, ev_rx)
}

fn builder(url: &str, key: &str, link: tmpsc::UnboundedSender<Link>) -> ClientBuilder {
    let forward = |name: &'static str, link: tmpsc::UnboundedSender<Link>| {
        move |payload: Payload, _: Client| {
            let event = first_value(&payload).and_then(|v| parse_event(name, &v));
            if let Some(event) = event {
                let _ = link.send(Link::Event(event));
            }
            async {}.boxed()
        }
    };
    let on_connect = link.clone();
    let on_close = link.clone();
    let on_error = link.clone();
    ClientBuilder::new(url)
        .auth(json!({ "key": key }))
        .transport_type(TransportType::Websocket)
        .reconnect(false)
        .reconnect_on_disconnect(false)
        .on("state", forward("state", link.clone()))
        .on("content", forward("content", link.clone()))
        .on("historyContent", forward("historyContent", link))
        .on(Event::Connect, move |_, _| {
            let _ = on_connect.send(Link::Connected);
            async {}.boxed()
        })
        .on(Event::Close, move |_, _| {
            let _ = on_close.send(Link::Closed);
            async {}.boxed()
        })
        .on(Event::Error, move |payload, _| {
            if is_auth_error(&payload_text(&payload)) {
                let _ = on_error.send(Link::AuthRejected);
            }
            async {}.boxed()
        })
}

async fn emit_with_reply(client: &Client, event: &'static str, data: Option<Value>, reply: oneshot::Sender<Option<Value>>) {
    let slot = Arc::new(Mutex::new(Some(reply)));
    let callback_slot = slot.clone();
    let sent = client
        .emit_with_ack(event, to_payload(data), ACK_TIMEOUT, move |payload: Payload, _: Client| {
            if let Some(reply) = callback_slot.lock().ok().and_then(|mut s| s.take()) {
                let _ = reply.send(ack_value(&payload));
            }
            async {}.boxed()
        })
        .await;
    if sent.is_err() {
        if let Some(reply) = slot.lock().ok().and_then(|mut s| s.take()) {
            let _ = reply.send(None);
        }
    }
}

async fn run_session(
    client: &Client,
    cmds: &mut tmpsc::UnboundedReceiver<Command>,
    link_rx: &mut tmpsc::UnboundedReceiver<Link>,
    link_tx: &tmpsc::UnboundedSender<Link>,
    events: &mpsc::UnboundedSender<ServerEvent>,
) -> Outcome {
    let started = Instant::now();
    let mut connected = false;
    let mut last_pong = Instant::now();
    let mut heartbeat = tokio::time::interval(HEARTBEAT_EVERY);
    loop {
        tokio::select! {
            link = link_rx.recv() => match link {
                Some(Link::Connected) => {
                    if !connected {
                        connected = true;
                        last_pong = Instant::now();
                        let _ = events.unbounded_send(ServerEvent::Connected);
                    }
                }
                Some(Link::Event(event)) => {
                    if !connected {
                        connected = true;
                        last_pong = Instant::now();
                        let _ = events.unbounded_send(ServerEvent::Connected);
                    }
                    let _ = events.unbounded_send(event);
                }
                Some(Link::Pong) => last_pong = Instant::now(),
                Some(Link::AuthRejected) => return Outcome::AuthRejected,
                Some(Link::Closed) | None => return Outcome::Lost,
            },
            cmd = cmds.recv() => match cmd {
                None | Some(Command::Shutdown) => return Outcome::Shutdown,
                Some(Command::ReconnectNow) => return Outcome::Lost,
                Some(Command::Emit { event, data }) => {
                    if connected {
                        let _ = client.emit(event, to_payload(data)).await;
                    }
                }
                Some(Command::Ack { event, data, reply }) => {
                    if connected {
                        emit_with_reply(client, event, data, reply).await;
                    } else {
                        let _ = reply.send(None);
                    }
                }
            },
            _ = heartbeat.tick() => {
                if connected {
                    if last_pong.elapsed() > HEARTBEAT_LOST_AFTER {
                        return Outcome::Lost;
                    }
                    let pong = link_tx.clone();
                    let _ = client
                        .emit_with_ack("ping", Payload::Text(vec![]), HEARTBEAT_LOST_AFTER, move |_: Payload, _: Client| {
                            let _ = pong.send(Link::Pong);
                            async {}.boxed()
                        })
                        .await;
                } else if started.elapsed() > CONNECT_TIMEOUT {
                    return Outcome::Lost;
                }
            }
        }
    }
}

async fn wait_for_retry(delay: Duration, cmds: &mut tmpsc::UnboundedReceiver<Command>, forever: bool) -> bool {
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep, if !forever => return true,
            cmd = cmds.recv() => match cmd {
                None | Some(Command::Shutdown) => return false,
                Some(Command::ReconnectNow) => return true,
                Some(Command::Ack { reply, .. }) => {
                    let _ = reply.send(None);
                }
                Some(Command::Emit { .. }) => {}
            },
        }
    }
}

enum Control {
    Retry,
    Shutdown,
}

async fn next_control(cmds: &mut tmpsc::UnboundedReceiver<Command>) -> Control {
    loop {
        match cmds.recv().await {
            None | Some(Command::Shutdown) => return Control::Shutdown,
            Some(Command::ReconnectNow) => return Control::Retry,
            Some(Command::Ack { reply, .. }) => {
                let _ = reply.send(None);
            }
            Some(Command::Emit { .. }) => {}
        }
    }
}

async fn supervise(
    url: String,
    key: String,
    mut cmds: tmpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<ServerEvent>,
) {
    let mut delay = BACKOFF_MIN;
    loop {
        let (link_tx, mut link_rx) = tmpsc::unbounded_channel();
        let mut auth_rejected = false;
        let attempt = tokio::select! {
            result = tokio::time::timeout(CONNECT_TIMEOUT, builder(&url, &key, link_tx.clone()).connect()) => {
                result.ok().and_then(Result::ok)
            }
            control = next_control(&mut cmds) => match control {
                Control::Shutdown => return,
                Control::Retry => {
                    delay = Duration::ZERO;
                    None
                }
            },
        };
        match attempt {
            Some(client) => {
                let outcome = run_session(&client, &mut cmds, &mut link_rx, &link_tx, &events).await;
                let _ = client.disconnect().await;
                match outcome {
                    Outcome::Shutdown => return,
                    Outcome::AuthRejected => {
                        auth_rejected = true;
                        let _ = events.unbounded_send(ServerEvent::AuthRejected);
                    }
                    Outcome::Lost => {
                        delay = BACKOFF_MIN;
                        let _ = events.unbounded_send(ServerEvent::Disconnected);
                    }
                }
            }
            None => {
                let _ = events.unbounded_send(ServerEvent::Disconnected);
            }
        }
        if !wait_for_retry(delay, &mut cmds, auth_rejected).await {
            return;
        }
        delay = (delay * 2).clamp(BACKOFF_MIN, BACKOFF_MAX);
    }
}

impl ConnHandle {
    #[cfg(test)]
    pub(crate) fn recording() -> (ConnHandle, tmpsc::UnboundedReceiver<Command>) {
        let (tx, rx) = tmpsc::unbounded_channel();
        (ConnHandle { tx }, rx)
    }

    fn emit(&self, event: &'static str, data: Option<Value>) {
        let _ = self.tx.send(Command::Emit { event, data });
    }

    async fn ack(&self, event: &'static str, data: Option<Value>) -> Option<Value> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Command::Ack { event, data, reply }).ok()?;
        rx.await.ok().flatten()
    }

    pub fn send_keys(&self, session_id: &str, keys: &str) {
        self.emit("sendKeys", Some(json!({ "sessionId": session_id, "keys": keys })));
    }

    pub fn watch(&self, ids: Vec<String>) {
        self.emit("watch", Some(json!({ "sessionIds": ids })));
    }

    pub fn get_content(&self, session_id: &str) {
        self.emit("getContent", Some(json!({ "sessionId": session_id })));
    }

    pub fn get_earlier(&self, session_id: &str, before_line: i64) {
        self.emit("getEarlierContent", Some(json!({ "sessionId": session_id, "beforeLine": before_line })));
    }

    pub fn paste_image(&self, session_id: &str, data_url: String) {
        self.emit("pasteImage", Some(json!({ "sessionId": session_id, "data": data_url })));
    }

    pub fn focus(&self, window_id: &str, tab_index: u32) {
        self.emit("focus", Some(json!({ "windowId": window_id, "tabIndex": tab_index })));
    }

    pub fn new_tab(&self) {
        self.emit("newTab", None);
    }

    pub fn close_tab(&self, tab_id: &str) {
        self.emit("closeTab", Some(json!({ "tabId": tab_id })));
    }

    pub fn move_tab(&self, tab_id: &str, to_index: u32) {
        self.emit("moveTab", Some(json!({ "tabId": tab_id, "toIndex": to_index })));
    }

    pub async fn clipboard_count(&self) -> Option<i64> {
        self.ack("clipboardCount", None).await?.as_i64()
    }

    pub async fn clipboard_since(&self, count: i64) -> Option<String> {
        self.ack("clipboardSince", Some(json!({ "count": count }))).await?.as_str().map(str::to_string)
    }

    pub fn reconnect_now(&self) {
        let _ = self.tx.send(Command::ReconnectNow);
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn parses_state_content_history() {
        let state = json!([{"id": "W1", "isFront": true, "tabs": []}]);
        assert!(matches!(parse_event("state", &state), Some(ServerEvent::State(w)) if w[0].id == "W1"));
        let content = json!({
            "sessionId": "S1", "lines": [[{"t": "hi"}]], "fg": "#fff", "bg": "#000",
            "firstLine": 5, "availableFirstLine": 0, "terminalEnd": 6, "isLatest": true
        });
        assert!(matches!(parse_event("content", &content), Some(ServerEvent::Content(c)) if c.session_id == "S1"));
        assert!(matches!(parse_event("historyContent", &content), Some(ServerEvent::History(c)) if c.content.first_line == 5));
    }

    #[test]
    fn unknown_event_is_none() {
        assert!(parse_event("screenSize", &json!({"width": 1, "height": 1})).is_none());
        assert!(parse_event("state", &json!("garbage")).is_none());
    }

    #[test]
    fn ack_payload_unwraps_the_args_array() {
        assert_eq!(ack_value(&Payload::from("[123]".to_string())), Some(json!(123)));
        assert_eq!(ack_value(&Payload::from("[\"copied\"]".to_string())), Some(json!("copied")));
        assert_eq!(ack_value(&Payload::from("[]".to_string())), None);
    }

    fn black_hole() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming().flatten() {
                held.push(stream);
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn shutdown_is_prompt_while_a_connect_hangs() {
        let (handle, mut events) = start(black_hole(), "key".into());
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.shutdown();
        let ended = tokio::time::timeout(Duration::from_secs(2), async {
            while events.next().await.is_some() {}
        })
        .await;
        assert!(ended.is_ok(), "connection thread ignored shutdown during a hung connect");
    }

    #[tokio::test]
    async fn retry_now_interrupts_a_hung_connect() {
        let (handle, mut events) = start(black_hole(), "key".into());
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.reconnect_now();
        let got = tokio::time::timeout(Duration::from_secs(2), events.next()).await;
        assert!(matches!(got, Ok(Some(ServerEvent::Disconnected))), "retry did not abandon the hung attempt: {got:?}");
        handle.shutdown();
    }

    #[test]
    fn detects_auth_error() {
        assert!(is_auth_error("Received an ConnectError frame: {\"message\":\"invalid access key\"}"));
        assert!(!is_auth_error("connection reset by peer"));
    }

    fn live_key() -> String {
        std::env::var("REMOTE_ITERM_KEY").unwrap_or_else(|_| {
            let path = dirs::home_dir().unwrap().join("Library/Application Support/remote-iterm/access-key");
            std::fs::read_to_string(path).unwrap().trim().to_string()
        })
    }

    async fn next_matching(
        events: &mut futures::channel::mpsc::UnboundedReceiver<ServerEvent>,
        want: fn(&ServerEvent) -> bool,
    ) -> bool {
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(ev) = events.next().await {
                if want(&ev) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false)
    }

    #[tokio::test]
    #[ignore]
    async fn live_connect_receives_state() {
        let (handle, mut events) = start("http://localhost:7291".into(), live_key());
        assert!(next_matching(&mut events, |e| matches!(e, ServerEvent::State(_))).await);
        assert!(handle.clipboard_count().await.is_some());
        handle.shutdown();
    }

    #[tokio::test]
    #[ignore]
    async fn live_wrong_key_is_rejected() {
        let (handle, mut events) = start("http://localhost:7291".into(), "wrong-key".into());
        assert!(next_matching(&mut events, |e| matches!(e, ServerEvent::AuthRejected)).await);
        handle.shutdown();
    }
}

