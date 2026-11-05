use crate::config::{self, Config, SavedWindow, parse_key_input};
use crate::connection::{self, ConnHandle, ServerEvent};
use crate::geometry::{point_to_cell, selection_text};
use crate::keys::{KeyInput, encode_key, paste_sequence};
use crate::mouse::{MOTION, WHEEL_DOWN, WHEEL_UP, modifier_bits, sgr, terminal_row};
use crate::model::{Effect, Model};
use crate::prediction::{PredictKey, Predictor};
use crate::ui::terminal::{TermStyle, compose_row, render_row};
use crate::ui::theme;
use futures::StreamExt;
use futures::future::{BoxFuture, FutureExt, Shared};
use base64::Engine;
use gpui::{
    App, Bounds, DragMoveEvent, Hsla, WindowControlArea, ClipboardEntry, DispatchPhase, ClipboardItem, Context, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Point, WindowBounds, ScrollDelta, ScrollWheelEvent, canvas, ParentElement, Pixels, Render, ScrollStrategy, SharedString, Styled, Task,
    ElementInputHandler, EntityInputHandler, UTF16Selection, UniformListScrollHandle, Window, div, prelude::*, px,
    uniform_list,
};
use std::cell::Cell;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

type Cellpos = (i64, usize);
type Connector = fn(String, String) -> (ConnHandle, futures::channel::mpsc::UnboundedReceiver<ServerEvent>);

#[derive(Clone)]
struct DraggedTab {
    tab_id: String,
    title: SharedString,
    width: Option<Pixels>,
}

impl Render for DraggedTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(px(theme::TITLEBAR_HEIGHT))
            .flex()
            .items_center()
            .when_some(self.width, |d, width| d.w(width))
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .shadow_md()
                    .text_size(px(12.0))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .bg(theme::color(theme::BORDER))
                    .text_color(theme::color(theme::ACCENT))
                    .child(self.title.clone()),
            )
    }
}

struct TabDrag {
    tab_id: String,
    target: usize,
}

struct Drag {
    mods: u8,
    last: (u32, u32),
    count: Shared<BoxFuture<'static, Option<i64>>>,
}

pub struct RootView {
    pub(crate) model: Model,
    pub(crate) predictor: Predictor,
    pub(crate) conn: Option<ConnHandle>,
    pub(crate) config: Config,
    pub(crate) scroll: UniformListScrollHandle,
    pub(crate) focus: FocusHandle,
    pub(crate) cell_width: Pixels,
    pub(crate) font_size: f32,
    pub(crate) custom_titlebar: bool,
    zoom_accum: f32,
    pub(crate) connected: bool,
    pub(crate) ever_connected: bool,
    pub(crate) auth_rejected: bool,
    pub(crate) term_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub(crate) selection: Option<(Cellpos, Cellpos)>,
    selecting: bool,
    drag: Option<Drag>,
    history_pending: Option<i64>,
    tab_drag: Option<TabDrag>,
    tab_bounds: Rc<std::cell::RefCell<Vec<(String, Bounds<Pixels>)>>>,
    wheel_accum: f32,
    pub(crate) config_path: PathBuf,
    connector: Connector,
    pub(crate) bitmap_reader: fn() -> Option<gpui::Image>,
    key_input: String,
    marked: Option<String>,
    save_task: Option<Task<()>>,
    zoom_save_task: Option<Task<()>>,
    events_task: Option<Task<()>>,
    tick_task: Option<Task<()>>,
}

impl RootView {
    fn build(config: Config, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let font_size = config.font_size.unwrap_or(theme::FONT_SIZE).clamp(theme::MIN_FONT_SIZE, theme::MAX_FONT_SIZE);
        let cell_width = Self::measure_cell(font_size, cx);
        let focus = cx.focus_handle();
        window.focus(&focus);
        let view = Self {
            model: Model::default(),
            predictor: Predictor::new(),
            conn: None,
            config,
            scroll: UniformListScrollHandle::new(),
            focus,
            cell_width,
            font_size,
            custom_titlebar: cfg!(target_os = "windows"),
            zoom_accum: 0.0,
            connected: false,
            ever_connected: false,
            auth_rejected: false,
            term_bounds: Rc::new(Cell::new(None)),
            selection: None,
            selecting: false,
            drag: None,
            history_pending: None,
            tab_drag: None,
            tab_bounds: Rc::new(std::cell::RefCell::new(Vec::new())),
            wheel_accum: 0.0,
            config_path: config::config_path(),
            connector: connection::start,
            bitmap_reader: crate::clipboard::read_bitmap,
            key_input: String::new(),
            marked: None,
            save_task: None,
            zoom_save_task: None,
            events_task: None,
            tick_task: None,
        };
        cx.observe_window_bounds(window, |view: &mut Self, window, cx| view.schedule_save(window, cx)).detach();
        view
    }

    pub fn new(config: Config, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self::build(config, window, cx);
        view.connect(cx);
        view
    }

    #[cfg(test)]
    pub(crate) fn attached(
        config: Config,
        config_path: PathBuf,
        handle: ConnHandle,
        events: futures::channel::mpsc::UnboundedReceiver<ServerEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::build(config, window, cx);
        view.config_path = config_path;
        view.connector = |_, _| {
            let (handle, _) = ConnHandle::recording();
            (handle, futures::channel::mpsc::unbounded().1)
        };
        view.bitmap_reader = || None;
        view.attach(handle, events, cx);
        view
    }

    fn measure_cell(font_size: f32, cx: &App) -> Pixels {
        let text_system = cx.text_system();
        let font_id = text_system.resolve_font(&theme::terminal_font(false));
        text_system
            .advance(font_id, px(font_size), 'M')
            .map(|size| size.width)
            .unwrap_or(px(font_size * 0.6))
    }

    pub(crate) fn line_height(&self) -> f32 {
        theme::line_height(self.font_size)
    }

    fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        let size = size.clamp(theme::MIN_FONT_SIZE, theme::MAX_FONT_SIZE);
        if size == self.font_size {
            return;
        }
        let bottom = self.at_bottom();
        self.font_size = size;
        self.cell_width = Self::measure_cell(size, cx);
        if bottom {
            self.scroll_to_bottom();
        }
        let path = self.config_path.clone();
        self.zoom_save_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_millis(500)).await;
            let _ = config::remember_font_size(&path, size);
        }));
        cx.notify();
    }

    fn zoom_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        self.zoom_accum += self.wheel_lines(event);
        let steps = self.zoom_accum.trunc();
        if steps != 0.0 {
            self.zoom_accum -= steps;
            self.set_font_size(self.font_size + steps, cx);
        }
    }

    pub(crate) fn key_screen_visible(&self) -> bool {
        self.config.key.is_none() || self.auth_rejected
    }

    fn schedule_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let saved = match window.window_bounds() {
            WindowBounds::Windowed(b) => SavedWindow {
                x: f32::from(b.origin.x),
                y: f32::from(b.origin.y),
                width: f32::from(b.size.width),
                height: f32::from(b.size.height),
                maximized: false,
            },
            WindowBounds::Maximized(b) | WindowBounds::Fullscreen(b) => SavedWindow {
                x: f32::from(b.origin.x),
                y: f32::from(b.origin.y),
                width: f32::from(b.size.width),
                height: f32::from(b.size.height),
                maximized: true,
            },
        };
        let path = self.config_path.clone();
        self.save_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let _ = config::remember_window(&path, saved);
        }));
    }

    fn submit_key(&mut self, cx: &mut Context<Self>) {
        let Some(key) = parse_key_input(&self.key_input) else {
            return;
        };
        self.key_input.clear();
        self.config.key = Some(key.clone());
        let mut stored = config::load(&self.config_path);
        stored.key = Some(key);
        let _ = config::save(&self.config_path, &stored);
        self.connect(cx);
        cx.notify();
    }

    fn on_key_screen_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let m = keystroke.modifiers;
        if (m.control || m.platform) && keystroke.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.key_input.push_str(text.trim());
            }
        } else if keystroke.key == "enter" {
            self.submit_key(cx);
        } else if keystroke.key == "backspace" {
            self.key_input.pop();
        } else if !m.control && !m.platform {
            if let Some(ch) = keystroke.key_char.as_deref().or_else(|| (keystroke.key.chars().count() == 1).then_some(keystroke.key.as_str())) {
                self.key_input.push_str(ch);
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(crate) fn connect(&mut self, cx: &mut Context<Self>) {
        if let Some(old) = self.conn.take() {
            old.shutdown();
        }
        let Some(key) = self.config.key.clone() else {
            return;
        };
        self.auth_rejected = false;
        self.connected = false;
        self.ever_connected = false;
        let (handle, events) = (self.connector)(self.config.server_url.clone(), key);
        self.attach(handle, events, cx);
    }

    pub(crate) fn attach(
        &mut self,
        handle: ConnHandle,
        mut events: futures::channel::mpsc::UnboundedReceiver<ServerEvent>,
        cx: &mut Context<Self>,
    ) {
        self.conn = Some(handle);
        self.events_task = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this.update(cx, |view, cx| view.handle_event(event, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn at_bottom(&self) -> bool {
        let state = self.scroll.0.borrow();
        let handle = &state.base_handle;
        let offset = handle.offset().y;
        let max = handle.max_offset().height;
        max <= px(0.0) || -offset >= max - px(2.0)
    }

    fn scroll_to_bottom(&self) {
        if let Some(content) = self.model.shown() {
            if !content.lines.is_empty() {
                self.scroll.scroll_to_item(content.lines.len() - 1, ScrollStrategy::Bottom);
            }
        }
    }

    fn handle_event(&mut self, event: ServerEvent, cx: &mut Context<Self>) {
        match event {
            ServerEvent::Connected => {
                self.connected = true;
                self.history_pending = None;
                self.ever_connected = true;
                let effects = self.model.on_connected();
                self.apply_effects(effects, cx);
            }
            ServerEvent::Disconnected => {
                self.connected = false;
                self.history_pending = None;
            }
            ServerEvent::AuthRejected => {
                self.connected = false;
                self.auth_rejected = true;
            }
            ServerEvent::State(windows) => {
                let effects = self.model.apply_state(windows);
                self.apply_effects(effects, cx);
            }
            ServerEvent::Content(content) => {
                let follow = self.at_bottom();
                let session = content.session_id.clone();
                if self.model.apply_content(content) {
                    if let Some(shown) = self.model.shown() {
                        self.predictor.on_content(&session, shown, Instant::now());
                    }
                    if follow {
                        self.scroll_to_bottom();
                    }
                }
            }
            ServerEvent::History(content) => {
                let before = self.model.shown().map(|c| c.first_line);
                if self.model.apply_history(content) {
                    self.history_pending = None;
                    let after = self.model.shown().map(|c| c.first_line);
                    if let (Some(before), Some(after)) = (before, after) {
                        let added = usize::try_from(before - after).unwrap_or(0);
                        if added > 0 {
                            let offset = -f32::from(self.scroll.0.borrow().base_handle.offset().y);
                            let top = (offset / self.line_height()).floor().max(0.0) as usize;
                            self.scroll.scroll_to_item(top + added, ScrollStrategy::Top);
                        }
                    }
                }
            }
        }
        cx.notify();
    }

    pub fn apply_effects(&mut self, effects: Vec<Effect>, cx: &mut Context<Self>) {
        let mut switched = false;
        for effect in effects {
            match effect {
                Effect::ResetPrediction => {
                    self.predictor.reset();
                    self.history_pending = None;
                    switched = true;
                }
                Effect::Watch(ids) => {
                    if let Some(conn) = &self.conn {
                        conn.watch(ids);
                    }
                }
                Effect::GetContent(id) => {
                    if let Some(conn) = &self.conn {
                        conn.get_content(&id);
                    }
                }
                Effect::Focus { window_id, tab_index } => {
                    if let Some(conn) = &self.conn {
                        conn.focus(&window_id, tab_index);
                    }
                }
            }
        }
        if switched {
            self.scroll_to_bottom();
        }
        cx.notify();
    }

    pub(crate) fn send_to_terminal(&mut self, keys: &str, predict: Option<PredictKey>, cx: &mut Context<Self>) {
        let Some(session) = self.model.session_id.clone() else {
            return;
        };
        if let Some(conn) = &self.conn {
            conn.send_keys(&session, keys);
        }
        self.selection = None;
        match (predict, self.model.shown()) {
            (Some(key), Some(screen)) => self.predictor.on_key(&session, key, screen, Instant::now()),
            _ => self.predictor.reset(),
        }
        self.scroll_to_bottom();
        self.ensure_tick(cx);
        cx.notify();
    }

    fn type_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(session) = self.model.session_id.clone() else {
            return;
        };
        if let Some(conn) = &self.conn {
            conn.send_keys(&session, text);
        }
        self.selection = None;
        let now = Instant::now();
        for ch in text.chars() {
            match self.model.shown() {
                Some(screen) if !ch.is_control() => self.predictor.on_key(&session, PredictKey::Char(ch), screen, now),
                _ => self.predictor.reset(),
            }
        }
        self.scroll_to_bottom();
        self.ensure_tick(cx);
        cx.notify();
    }

    fn ensure_tick(&mut self, cx: &mut Context<Self>) {
        if self.tick_task.is_some() || !self.predictor.has_pending() {
            return;
        }
        self.tick_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let keep = this.update(cx, |view, cx| {
                    view.predictor.tick(Instant::now());
                    cx.notify();
                    if !view.predictor.has_pending() {
                        view.tick_task = None;
                        return false;
                    }
                    true
                });
                if !matches!(keep, Ok(true)) {
                    break;
                }
            }
        }));
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.model.session_id.clone() else {
            return;
        };
        let item = cx.read_from_clipboard();
        let image = item.as_ref().and_then(|item| {
            item.entries().iter().find_map(|entry| match entry {
                ClipboardEntry::Image(image) => Some(image.clone()),
                _ => None,
            })
        });
        let text = item.and_then(|item| item.text()).filter(|text| !text.is_empty());
        let image = image.or_else(|| if text.is_none() { (self.bitmap_reader)() } else { None });
        if let Some(image) = image {
            let data = base64::engine::general_purpose::STANDARD.encode(&image.bytes);
            if let Some(conn) = &self.conn {
                conn.paste_image(&session, format!("data:{};base64,{data}", image.format.mime_type()));
            }
            self.predictor.reset();
            cx.notify();
            return;
        }
        if let Some(text) = text {
            self.send_to_terminal(&paste_sequence(&text), None, cx);
        }
    }

    fn send_raw(&mut self, keys: &str) {
        let Some(session) = self.model.session_id.clone() else {
            return;
        };
        if let Some(conn) = &self.conn {
            conn.send_keys(&session, keys);
        }
        self.predictor.reset();
    }

    fn cell_at(&self, position: Point<Pixels>) -> Option<Cellpos> {
        let bounds = self.term_bounds.get()?;
        let content = self.model.shown()?;
        let scroll_top = -self.scroll.0.borrow().base_handle.offset().y;
        Some(point_to_cell(
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
            f32::from(self.cell_width),
            self.line_height(),
            f32::from(scroll_top),
            content.first_line,
        ))
    }

    fn report_cell(&self, cell: Cellpos) -> Option<(u32, u32)> {
        let content = self.model.shown()?;
        let screen_first = content.screen_first_line.unwrap_or(content.first_line);
        Some((u32::try_from(cell.1 + 1).ok()?, terminal_row(cell.0, screen_first)))
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let (Some((a, b)), Some(content)) = (self.selection, self.model.shown()) else {
            return false;
        };
        let text = selection_text(content, a, b);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let Some(cell) = self.cell_at(event.position) else {
            return;
        };
        let Some(content) = self.model.shown() else {
            return;
        };
        if content.mouse {
            let Some(at) = self.report_cell(cell) else {
                return;
            };
            let mods = modifier_bits(event.modifiers.shift, event.modifiers.control);
            let count = match self.conn.clone() {
                Some(conn) => async move { conn.clipboard_count().await }.boxed().shared(),
                None => async { None }.boxed().shared(),
            };
            let driver = count.clone();
            cx.spawn(async move |_, _| {
                driver.await;
            })
            .detach();
            self.send_raw(&sgr(mods, at.0, at.1, false));
            self.drag = Some(Drag { mods, last: at, count });
            return;
        }
        if event.modifiers.control {
            if let Some(url) = content.link_at(cell.0, cell.1) {
                cx.open_url(&url);
                return;
            }
        }
        self.selection = Some((cell, cell));
        self.selecting = true;
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some(cell) = self.cell_at(event.position) else {
            return;
        };
        if let Some(drag) = &self.drag {
            let Some(at) = self.report_cell(cell) else {
                return;
            };
            if at != drag.last {
                let code = MOTION | drag.mods;
                self.send_raw(&sgr(code, at.0, at.1, false));
                if let Some(drag) = &mut self.drag {
                    drag.last = at;
                }
            }
            return;
        }
        if self.selecting {
            if let Some((anchor, _)) = self.selection {
                self.selection = Some((anchor, cell));
                cx.notify();
            }
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(drag) = self.drag.take() {
            let at = self.cell_at(event.position).and_then(|c| self.report_cell(c)).unwrap_or(drag.last);
            self.send_raw(&sgr(drag.mods, at.0, at.1, true));
            let Some(conn) = self.conn.clone() else {
                return;
            };
            let count = drag.count;
            cx.spawn(async move |_, cx| {
                let Some(count) = count.await else {
                    return;
                };
                let Some(text) = conn.clipboard_since(count).await.filter(|t| !t.is_empty()) else {
                    return;
                };
                let _ = cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(text)));
            })
            .detach();
            return;
        }
        if self.selecting {
            self.selecting = false;
            if let Some((a, b)) = self.selection {
                if a == b {
                    self.selection = None;
                }
            }
            cx.notify();
        }
    }

    fn at_top(&self) -> bool {
        self.scroll.0.borrow().base_handle.offset().y >= px(-1.0)
    }

    fn wheel_lines(&self, event: &ScrollWheelEvent) -> f32 {
        match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / self.line_height(),
        }
    }

    fn forward_wheel(&mut self, event: &ScrollWheelEvent) -> bool {
        if !self.model.shown().is_some_and(|c| c.mouse) {
            return false;
        }
        let Some(at) = self.cell_at(event.position).and_then(|c| self.report_cell(c)) else {
            return true;
        };
        self.wheel_accum += self.wheel_lines(event);
        while self.wheel_accum >= 1.0 {
            self.wheel_accum -= 1.0;
            self.send_raw(&sgr(WHEEL_UP, at.0, at.1, false));
        }
        while self.wheel_accum <= -1.0 {
            self.wheel_accum += 1.0;
            self.send_raw(&sgr(WHEEL_DOWN, at.0, at.1, false));
        }
        true
    }

    fn on_scroll_wheel(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(content) = self.model.shown() else {
            return;
        };
        if content.mouse {
            return;
        }
        if self.wheel_lines(event) > 0.0
            && self.at_top()
            && content.first_line > content.available_first_line
            && self.history_pending.is_none()
        {
            let first = content.first_line;
            if let (Some(conn), Some(session)) = (&self.conn, &self.model.session_id) {
                conn.get_earlier(session, first);
                self.history_pending = Some(first);
            }
        }
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.key_screen_visible() {
            self.on_key_screen_key(event, cx);
            return;
        }
        let keystroke = &event.keystroke;
        let m = keystroke.modifiers;
        if m.control && !m.alt && !m.platform {
            let zoom = match keystroke.key.as_str() {
                "=" | "+" => Some(self.font_size + 1.0),
                "-" if !m.shift => Some(self.font_size - 1.0),
                "0" if !m.shift => Some(theme::FONT_SIZE),
                _ => None,
            };
            if let Some(size) = zoom {
                self.set_font_size(size, cx);
                cx.stop_propagation();
                return;
            }
        }
        if m.control && !m.alt && !m.platform && keystroke.key == "tab" {
            let effects = self.model.select_adjacent_tab(!m.shift);
            self.apply_effects(effects, cx);
            cx.stop_propagation();
            return;
        }
        let plain_ctrl = m.control && !m.alt && !m.platform && !m.shift;
        if m.control && m.shift && !m.alt && !m.platform && keystroke.key == "c" {
            self.copy_selection(cx);
            cx.stop_propagation();
            return;
        }
        if plain_ctrl && keystroke.key == "c" && self.copy_selection(cx) {
            self.selection = None;
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if plain_ctrl {
            if let Some(n) = keystroke.key.parse::<u8>().ok().filter(|n| (1..=9).contains(n)) {
                let effects = self.model.select_tab_number(n);
                self.apply_effects(effects, cx);
                cx.stop_propagation();
                return;
            }
            if keystroke.key == "v" {
                self.paste(cx);
                cx.stop_propagation();
                return;
            }
            if keystroke.key == "t" {
                if let Some(conn) = &self.conn {
                    conn.new_tab();
                }
                cx.stop_propagation();
                return;
            }
        }
        let typed_char = keystroke
            .key_char
            .as_deref()
            .filter(|c| !c.is_empty() && !c.chars().any(char::is_control));
        let plain = !m.control && !m.alt && !m.platform;
        let altgr = m.control && m.alt && !m.platform;
        if typed_char.is_some() && (plain || altgr) {
            return;
        }
        let input = KeyInput {
            key: &keystroke.key,
            key_char: keystroke.key_char.as_deref(),
            ctrl: m.control,
            alt: m.alt,
            shift: m.shift,
            platform: m.platform,
        };
        let Some(seq) = encode_key(&input) else {
            return;
        };
        let mut chars = seq.chars();
        let predict = match (chars.next(), chars.next()) {
            (Some('\x7f'), None) => Some(PredictKey::Backspace),
            (Some(c), None) if !c.is_control() && !m.control && !m.alt => Some(PredictKey::Char(c)),
            _ => None,
        };
        self.send_to_terminal(&seq, predict, cx);
        cx.stop_propagation();
    }

    fn render_window_buttons(&self, window: &Window) -> impl IntoElement {
        let button = |id: &'static str, glyph: &'static str, area: WindowControlArea, hover: Hsla| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .occlude()
                .window_control_area(area)
                .w(px(46.0))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .font_family("Segoe MDL2 Assets")
                .text_size(px(10.0))
                .text_color(theme::color(theme::TEXT))
                .hover(move |d| d.bg(hover).text_color(gpui::white()))
                .child(glyph)
        };
        let subtle = gpui::rgba(0xffffff1a).into();
        let maximize = if window.is_maximized() { "\u{E923}" } else { "\u{E922}" };
        div()
            .flex()
            .flex_row()
            .h_full()
            .child(button("window-min", "\u{E921}", WindowControlArea::Min, subtle))
            .child(button("window-max", maximize, WindowControlArea::Max, subtle))
            .child(button("window-close", "\u{E8BB}", WindowControlArea::Close, theme::color(theme::CLOSE_HOVER)))
    }

    fn render_drag_area(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("titlebar-drag")
            .debug_selector(|| "titlebar-drag".into())
            .flex_1()
            .min_w(px(theme::DRAG_AREA_MIN_WIDTH))
            .h_full()
            .occlude()
            .when(self.custom_titlebar, |d| d.window_control_area(WindowControlArea::Drag))
            .on_drop(cx.listener(|view, _: &DraggedTab, _, cx| view.finish_tab_drag(cx)))
    }

    fn render_resize_strip(&self, window: &Window) -> Option<impl IntoElement> {
        (self.custom_titlebar && !window.is_maximized()).then(|| {
            div()
                .id("resize-strip")
                .occlude()
                .absolute()
                .top_0()
                .left_0()
                .w_full()
                .h(px(theme::RESIZE_STRIP_HEIGHT))
        })
    }

    fn update_tab_drag(&mut self, tab_id: &str, x: Pixels, cx: &mut Context<Self>) {
        let target = self
            .tab_bounds
            .borrow()
            .iter()
            .filter(|(id, bounds)| id != tab_id && bounds.center().x < x)
            .count();
        let changed = match &mut self.tab_drag {
            Some(drag) if drag.tab_id == tab_id => std::mem::replace(&mut drag.target, target) != target,
            _ => {
                self.tab_drag = Some(TabDrag { tab_id: tab_id.to_string(), target });
                true
            }
        };
        if changed {
            cx.notify();
        }
    }

    fn finish_tab_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.tab_drag.take() else {
            return;
        };
        let tabs = self.model.current_tabs();
        if let Some(from) = tabs.iter().position(|t| t.tab_id == drag.tab_id) {
            let to = drag.target.min(tabs.len() - 1);
            if from != to {
                self.model.move_tab(&drag.tab_id, to);
                if let Some(conn) = &self.conn {
                    conn.move_tab(&drag.tab_id, to as u32 + 1);
                }
            }
        }
        cx.notify();
    }

    fn render_tab_bar(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bar = div()
            .relative()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap_1()
            .pl_2()
            .h(px(theme::TITLEBAR_HEIGHT))
            .when(!self.custom_titlebar, |d| d.pr_2())
            .bg(theme::color(theme::BAR))
            .border_b_1()
            .border_color(theme::color(theme::BORDER));
        if self.key_screen_visible() {
            return bar
                .child(self.render_drag_area(cx))
                .child(self.render_window_buttons(window))
                .children(self.render_resize_strip(window))
                .into_any_element();
        }
        let slot = |id: SharedString| div().id(id).h_full().flex().items_center().cursor_pointer();
        let pill = |active: bool| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded_md()
                .text_size(px(12.0))
                .whitespace_nowrap()
                .text_color(theme::color(if active { theme::ACCENT } else { theme::MUTED }))
                .when(active, |d| d.bg(theme::color(theme::BORDER)))
        };
        let total_tabs: usize = self.model.windows.iter().map(|w| w.tabs.len()).sum();
        let windows: Vec<(usize, String, bool)> = self
            .model
            .windows
            .iter()
            .enumerate()
            .map(|(ix, w)| (ix + 1, w.id.clone(), self.model.window_id.as_deref() == Some(w.id.as_str())))
            .collect();
        let selected = self.model.tab_id.clone();
        let mut tabs: Vec<(u32, String, String, SharedString)> = self
            .model
            .current_tabs()
            .iter()
            .map(|t| {
                let title = if t.title.is_empty() { format!("Tab {}", t.index) } else { t.title.clone() };
                (t.index, t.id.clone(), t.tab_id.clone(), title.into())
            })
            .collect();
        if self.tab_drag.is_some() && !cx.has_active_drag() {
            self.tab_drag = None;
        }
        let dragging = self.tab_drag.as_ref().map(|d| d.tab_id.clone());
        if let Some(drag) = &self.tab_drag {
            if let Some(from) = tabs.iter().position(|t| t.2 == drag.tab_id) {
                let tab = tabs.remove(from);
                tabs.insert(drag.target.min(tabs.len()), tab);
            }
        }
        let last_widths: std::collections::HashMap<String, Pixels> =
            self.tab_bounds.borrow().iter().map(|(id, b)| (id.clone(), b.size.width)).collect();
        self.tab_bounds.borrow_mut().clear();
        let shown = self.model.session_id.clone();
        let panes: Vec<(usize, String, SharedString)> = self
            .model
            .current_tab()
            .map(|t| {
                t.sessions
                    .iter()
                    .enumerate()
                    .map(|(ix, s)| {
                        let label = if s.name.is_empty() { format!("Pane {}", ix + 1) } else { s.name.clone() };
                        (ix + 1, s.id.clone(), label.into())
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut bar = bar;
        if windows.len() > 1 {
            for (number, id, active) in windows {
                bar = bar.child(
                    slot(format!("window-{number}").into())
                        .debug_selector(move || format!("window-{number}"))
                        .flex_none()
                        .child(pill(active).child(format!("W{number}")))
                        .on_click(cx.listener(move |view, _, _, cx| {
                            let effects = view.model.select_window(&id);
                            view.apply_effects(effects, cx);
                        })),
                );
            }
            bar = bar.child(div().flex_none().w(px(1.0)).h(px(16.0)).mx_1().bg(theme::color(theme::BORDER)));
        }
        let mut strip = div()
            .id("tab-strip")
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_initial()
            .min_w(px(0.0))
            .overflow_hidden()
            .on_drag_move(cx.listener(|view, event: &DragMoveEvent<DraggedTab>, _, cx| {
                let tab_id = event.drag(cx).tab_id.clone();
                view.update_tab_drag(&tab_id, event.event.position.x, cx);
            }));
        for (index, id, tab_id, title) in tabs {
            let active = selected.as_deref() == Some(id.as_str());
            let ghost = dragging.as_deref() == Some(tab_id.as_str());
            let dragged = DraggedTab { tab_id: tab_id.clone(), title: title.clone(), width: last_widths.get(&tab_id).copied() };
            let bounds_store = self.tab_bounds.clone();
            let bounds_id = tab_id.clone();
            let mut label = pill(active)
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .child(div().flex_1().min_w(px(0.0)).overflow_hidden().text_ellipsis().child(title));
            if total_tabs > 1 {
                label = label.child(
                    div()
                        .id(SharedString::from(format!("close-tab-{index}")))
                        .debug_selector(move || format!("close-tab-{index}"))
                        .flex_none()
                        .size(px(18.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .text_size(px(15.0))
                        .text_color(theme::color(theme::MUTED))
                        .hover(|d| d.bg(theme::color(theme::HOVER)).text_color(theme::color(theme::TEXT)))
                        .child("×")
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if let Some(conn) = &view.conn {
                                conn.close_tab(&tab_id);
                            }
                            cx.stop_propagation();
                        })),
                );
            }
            let tab = slot(format!("tab-{index}").into())
                .debug_selector(move || format!("tab-{index}"))
                .relative()
                .min_w(px(theme::TAB_MIN_WIDTH))
                .max_w(px(260.0))
                .overflow_hidden()
                .when(ghost, |d| d.opacity(0.3))
                .child(label)
                .child(
                    canvas(move |bounds, _, _| bounds_store.borrow_mut().push((bounds_id.clone(), bounds)), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    let effects = view.model.select_tab(&id);
                    view.apply_effects(effects, cx);
                }))
                .on_drag(dragged, |dragged: &DraggedTab, _, _, cx| cx.new(|_| dragged.clone()));
            strip = strip.child(tab);
        }
        bar = bar.child(strip).child(
            slot("new-tab".into())
                .debug_selector(|| "new-tab".into())
                .group("new-tab")
                .flex_none()
                .child(
                    div()
                        .size(px(26.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .text_size(px(18.0))
                        .text_color(theme::color(theme::MUTED))
                        .group_hover("new-tab", |d| d.bg(theme::color(theme::HOVER)).text_color(theme::color(theme::TEXT)))
                        .child("+"),
                )
                .on_click(cx.listener(|view, _, _, _| {
                    if let Some(conn) = &view.conn {
                        conn.new_tab();
                    }
                })),
        );
        bar = bar.child(self.render_drag_area(cx));
        if panes.len() > 1 {
            for (number, id, label) in panes {
                let active = shown.as_deref() == Some(id.as_str());
                bar = bar.child(
                    slot(format!("pane-{number}").into())
                        .debug_selector(move || format!("pane-{number}"))
                        .flex_none()
                        .child(pill(active).child(label))
                        .on_click(cx.listener(move |view, _, _, cx| {
                            let effects = view.model.select_session(&id);
                            view.apply_effects(effects, cx);
                        })),
                );
            }
        }
        bar.on_drop(cx.listener(|view, _: &DraggedTab, _, cx| view.finish_tab_drag(cx)))
            .when(self.custom_titlebar, |d| d.child(self.render_window_buttons(window)))
            .children(self.render_resize_strip(window))
            .into_any_element()
    }

    fn render_key_screen(&self) -> impl IntoElement {
        let masked: SharedString = if self.key_input.is_empty() {
            "Paste the QR link or the key, then press Enter".into()
        } else {
            "•".repeat(self.key_input.chars().count().min(48)).into()
        };
        div()
            .id("key-screen")
            .debug_selector(|| "key-screen".into())
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::color(theme::BACKGROUND))
            .child(
                div()
                    .w(px(420.0))
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded_xl()
                    .border_1()
                    .border_color(theme::color(theme::BORDER))
                    .bg(theme::color(theme::BAR))
                    .child(div().text_size(px(12.0)).text_color(theme::color(theme::TEXT)).child("ACCESS KEY"))
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .border_1()
                            .border_color(theme::color(theme::BORDER))
                            .text_size(px(12.0))
                            .text_color(theme::color(if self.key_input.is_empty() { theme::MUTED } else { theme::TEXT }))
                            .child(masked),
                    )
                    .when(self.auth_rejected, |d| {
                        d.child(
                            div()
                                .id("key-rejected")
                                .debug_selector(|| "key-rejected".into())
                                .text_size(px(11.0))
                                .text_color(theme::color(theme::DANGER))
                                .child("That key was not accepted."),
                        )
                    }),
            )
    }

    fn render_reconnecting(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(gpui::rgba(0x000000dd))
            .child(div().text_color(theme::color(theme::TEXT)).child("Reconnecting…"))
            .child(
                div()
                    .id("retry")
                    .debug_selector(|| "retry".into())
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(theme::color(theme::ACCENT))
                    .text_color(theme::color(theme::BACKGROUND))
                    .child("Retry now")
                    .on_click(cx.listener(|view, _, _, _| {
                        if let Some(conn) = &view.conn {
                            conn.reconnect_now();
                        }
                    })),
            )
    }

    fn render_terminal(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(content) = self.model.shown() else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::color(theme::MUTED))
                .child(if self.config.key.is_some() { "Connecting…" } else { "No access key" })
                .into_any_element();
        };
        let fg = theme::parse_hex(&content.fg).unwrap_or(theme::color(theme::TEXT));
        let bg = theme::parse_hex(&content.bg).unwrap_or(theme::color(theme::BACKGROUND));
        let count = content.lines.len();
        let bounds_cell = self.term_bounds.clone();
        let weak = cx.entity().downgrade();
        div()
            .flex_1()
            .min_h_0()
            .bg(bg)
            .px(px(theme::TEXT_PADDING))
            .font_family(theme::font_family())
            .text_size(px(self.font_size))
            .line_height(px(self.line_height()))
            .text_color(fg)
            .child(
                div()
                    .relative()
                    .size_full()
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                    .child(
                        canvas(
                            move |bounds, _, _| bounds_cell.set(Some(bounds)),
                            move |bounds, _, window, _| {
                                window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _window, cx| {
                                    if phase != DispatchPhase::Capture || !bounds.contains(&event.position) {
                                        return;
                                    }
                                    if weak.update(cx, |view, cx| {
                                        if event.modifiers.control {
                                            view.zoom_wheel(event, cx);
                                            return true;
                                        }
                                        let handled = view.forward_wheel(event);
                                        if handled {
                                            cx.notify();
                                        }
                                        handled
                                    })
                                    .unwrap_or(false)
                                    {
                                        cx.stop_propagation();
                                    }
                                });
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                uniform_list(
                    "terminal",
                    count,
                    cx.processor(move |view, range: std::ops::Range<usize>, _window, _cx| {
                        let Some(content) = view.model.shown() else {
                            return Vec::new();
                        };
                        let style = TermStyle {
                            fg: theme::parse_hex(&content.fg).unwrap_or(theme::color(theme::TEXT)),
                            cell_width: view.cell_width,
                            line_height: px(view.line_height()),
                            cursor: theme::color(theme::ACCENT),
                            selection: theme::color(theme::SELECTION),
                        };
                        let selection = view.selection.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
                        let overlay = view.predictor.overlay();
                        let predicted_cursor = view.predictor.cursor();
                        range
                            .filter_map(|ix| {
                                let runs = content.lines.get(ix)?;
                                let line = content.first_line + ix as i64;
                                let cells: Vec<(usize, char)> =
                                    overlay.iter().filter(|o| o.line == line).map(|o| (o.col, o.ch)).collect();
                                let (row, real_cursor) = compose_row(runs, &cells);
                                let cursor = match predicted_cursor {
                                    Some((l, col)) => (l == line).then_some(col),
                                    None => real_cursor,
                                };
                                let highlight = selection.and_then(|(a, b)| {
                                    if line < a.0 || line > b.0 {
                                        return None;
                                    }
                                    let from = if line == a.0 { a.1 } else { 0 };
                                    let to = if line == b.0 { b.1 } else { usize::MAX / 2 };
                                    let width = row.iter().map(|r| r.t.chars().count()).sum::<usize>();
                                    Some((from, to.min(width.max(from))))
                                });
                                Some(render_row(&row, cursor, highlight, &style))
                            })
                            .collect()
                    }),
                )
                .track_scroll(self.scroll.clone())
                .size_full(),
                    ),
            )
            .into_any_element()
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let key_screen = self.key_screen_visible();
        let reconnecting = !key_screen && self.ever_connected && !self.connected;
        let show_bar = !key_screen || self.custom_titlebar;
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::color(theme::BACKGROUND))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .child({
                let entity = cx.entity();
                let focus = self.focus.clone();
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, cx| window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx),
                )
                .absolute()
                .size_full()
            })
            .when(show_bar, |d| d.child(self.render_tab_bar(window, cx)))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(self.render_terminal(cx))
                    .when(reconnecting, |d| d.child(self.render_reconnecting(cx)))
                    .when(key_screen, |d| d.child(self.render_key_screen())),
            )
    }
}

impl EntityInputHandler for RootView {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: 0..0, reversed: false })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = None;
        if text.is_empty() {
            return;
        }
        if self.key_screen_visible() {
            self.key_input.push_str(text);
            cx.notify();
            return;
        }
        self.type_text(text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = (!new_text.is_empty()).then(|| new_text.to_string());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let area = self.term_bounds.get()?;
        let content = self.model.shown()?;
        let (line, col) = self.predictor.cursor().or_else(|| content.cursor())?;
        let scroll_top = -self.scroll.0.borrow().base_handle.offset().y;
        let row = (line - content.first_line) as f32;
        let line_height = self.line_height();
        Some(Bounds::new(
            gpui::point(
                area.origin.x + self.cell_width * col as f32,
                area.origin.y + px(row * line_height) - scroll_top,
            ),
            gpui::size(self.cell_width, px(line_height)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}
