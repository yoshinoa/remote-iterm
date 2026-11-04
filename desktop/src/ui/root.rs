use crate::config::Config;
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
    Bounds, ClipboardEntry, DispatchPhase, ClipboardItem, Context, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Point, ScrollDelta, ScrollWheelEvent, canvas, ParentElement, Pixels, Render, ScrollStrategy, SharedString, Styled, Task,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

type Cellpos = (i64, usize);

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
    pub(crate) connected: bool,
    pub(crate) ever_connected: bool,
    pub(crate) auth_rejected: bool,
    pub(crate) term_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub(crate) selection: Option<(Cellpos, Cellpos)>,
    selecting: bool,
    drag: Option<Drag>,
    history_pending: Option<i64>,
    wheel_accum: f32,
    events_task: Option<Task<()>>,
    tick_task: Option<Task<()>>,
}

impl RootView {
    pub fn new(config: Config, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&theme::terminal_font(false));
        let cell_width = text_system
            .advance(font_id, px(theme::FONT_SIZE), 'M')
            .map(|size| size.width)
            .unwrap_or(px(theme::FONT_SIZE * 0.6));
        let focus = cx.focus_handle();
        window.focus(&focus);
        let mut view = Self {
            model: Model::default(),
            predictor: Predictor::new(),
            conn: None,
            config,
            scroll: UniformListScrollHandle::new(),
            focus,
            cell_width,
            connected: false,
            ever_connected: false,
            auth_rejected: false,
            term_bounds: Rc::new(Cell::new(None)),
            selection: None,
            selecting: false,
            drag: None,
            history_pending: None,
            wheel_accum: 0.0,
            events_task: None,
            tick_task: None,
        };
        view.connect(cx);
        view
    }

    #[cfg(test)]
    pub(crate) fn attached(
        config: Config,
        handle: ConnHandle,
        events: futures::channel::mpsc::UnboundedReceiver<ServerEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::new(Config { key: None, ..config }, window, cx);
        view.attach(handle, events, cx);
        view
    }

    pub(crate) fn connect(&mut self, cx: &mut Context<Self>) {
        if let Some(old) = self.conn.take() {
            old.shutdown();
        }
        let Some(key) = self.config.key.clone() else {
            return;
        };
        self.auth_rejected = false;
        let (handle, events) = connection::start(self.config.server_url.clone(), key);
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
                self.ever_connected = true;
                let effects = self.model.on_connected();
                self.apply_effects(effects, cx);
            }
            ServerEvent::Disconnected => self.connected = false,
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
                let session = content.session_id.clone();
                let before = self.model.shown().map(|c| c.first_line);
                if self.model.apply_history(content) {
                    self.history_pending = None;
                    let after = self.model.shown().map(|c| c.first_line);
                    if let (Some(before), Some(after)) = (before, after) {
                        let added = usize::try_from(before - after).unwrap_or(0);
                        if added > 0 {
                            let top = self.scroll.logical_scroll_top_index();
                            self.scroll.scroll_to_item(top + added, ScrollStrategy::Top);
                        }
                    }
                }
                let _ = session;
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
        if let Some(image) = image {
            let data = base64::engine::general_purpose::STANDARD.encode(&image.bytes);
            if let Some(conn) = &self.conn {
                conn.paste_image(&session, format!("data:{};base64,{data}", image.format.mime_type()));
            }
            self.predictor.reset();
            cx.notify();
            return;
        }
        match item.and_then(|item| item.text()).filter(|text| !text.is_empty()) {
            Some(text) => self.send_to_terminal(&paste_sequence(&text), None, cx),
            None => self.send_to_terminal("\x16", None, cx),
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
            theme::LINE_HEIGHT,
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

    fn wheel_lines(event: &ScrollWheelEvent) -> f32 {
        match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / theme::LINE_HEIGHT,
        }
    }

    fn forward_wheel(&mut self, event: &ScrollWheelEvent) -> bool {
        if !self.model.shown().is_some_and(|c| c.mouse) {
            return false;
        }
        let Some(at) = self.cell_at(event.position).and_then(|c| self.report_cell(c)) else {
            return true;
        };
        self.wheel_accum += Self::wheel_lines(event);
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
        if Self::wheel_lines(event) > 0.0
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
        let keystroke = &event.keystroke;
        let m = keystroke.modifiers;
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

    fn render_tab_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.model.tab_id.clone();
        let tabs: Vec<(String, SharedString)> = self
            .model
            .current_tabs()
            .iter()
            .map(|t| {
                let title = if t.title.is_empty() { format!("Tab {}", t.index) } else { t.title.clone() };
                (t.id.clone(), title.into())
            })
            .collect();
        div()
            .flex()
            .flex_row()
            .flex_none()
            .gap_1()
            .px_2()
            .py_1()
            .bg(theme::color(theme::BAR))
            .border_b_1()
            .border_color(theme::color(theme::BORDER))
            .children(tabs.into_iter().enumerate().map(|(ix, (id, title))| {
                let active = selected.as_deref() == Some(id.as_str());
                div()
                    .id(("tab", ix))
                    .px_3()
                    .py_1()
                    .max_w(px(240.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .rounded_md()
                    .text_size(px(12.0))
                    .cursor_pointer()
                    .text_color(theme::color(if active { theme::ACCENT } else { theme::MUTED }))
                    .when(active, |d| d.bg(theme::color(theme::BORDER)))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        let effects = view.model.select_tab(&id);
                        view.apply_effects(effects, cx);
                    }))
                    .child(title)
            }))
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
            .text_size(px(theme::FONT_SIZE))
            .line_height(px(theme::LINE_HEIGHT))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::color(theme::BACKGROUND))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .child(self.render_tab_bar(cx))
            .child(self.render_terminal(cx))
    }
}
