use crate::config::Config;
use crate::connection::{self, ConnHandle, ServerEvent};
use crate::keys::{KeyInput, encode_key, paste_sequence};
use crate::model::{Effect, Model};
use crate::prediction::{PredictKey, Predictor};
use crate::ui::terminal::{TermStyle, compose_row, render_row};
use crate::ui::theme;
use futures::StreamExt;
use base64::Engine;
use gpui::{
    ClipboardEntry, Context, FocusHandle, IntoElement, KeyDownEvent, ParentElement, Pixels, Render, ScrollStrategy, SharedString, Styled, Task,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use std::time::{Duration, Instant};

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
                self.model.apply_history(content);
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

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let m = keystroke.modifiers;
        let plain_ctrl = m.control && !m.alt && !m.platform && !m.shift;
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
                        };
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
                                Some(render_row(&row, cursor, &style))
                            })
                            .collect()
                    }),
                )
                .track_scroll(self.scroll.clone())
                .h_full(),
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
