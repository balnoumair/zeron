use std::collections::HashMap;
use std::time::Duration;

use base64::Engine as _;
use gpui::{
    App, Context, Entity, FocusHandle, IntoElement, KeyBinding, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Render, ScrollDelta, SharedString,
    Subscription, Task, Window, actions, div, prelude::*, px,
};

use zeron_proto::{TerminalEvent, TerminalSession};
use zeron_rpc::methods;

use crate::motion::{self, AnimationExt as _, TAB_SLIDE};
use crate::settings::{TERMINAL_MAX_VH, TERMINAL_MIN_HEIGHT};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

use super::emulator::{CellSnapshot, CursorSnapshot, Emulator, GridPoint, SelectionType, Side};
use super::view::{
    COALESCE_MS, GridGeometry, GridSnapshot, InputCoalescer, RESIZE_DEBOUNCE_MS,
    SELECTION_DRAG_THRESHOLD, TerminalElement, TerminalGridHost, cell_at, keystroke_bytes,
    paste_bytes, terminal_panel_bg,
};

pub const TAB_WIDTH: f32 = 118.0;
pub const TAB_BAR_HEIGHT: f32 = 40.0;

actions!(terminal, [ToggleTerminal]);

pub fn init(cx: &mut App) {
    let toggle = if cfg!(target_os = "macos") {
        "cmd-j"
    } else {
        "ctrl-j"
    };
    cx.bind_keys([KeyBinding::new(toggle, ToggleTerminal, None)]);
}

pub fn clamp_terminal_height(height: f32, viewport_h: f32) -> f32 {
    let max = (viewport_h * TERMINAL_MAX_VH).max(TERMINAL_MIN_HEIGHT);
    if height.is_finite() {
        height.clamp(TERMINAL_MIN_HEIGHT, max)
    } else {
        TERMINAL_MIN_HEIGHT
    }
}

pub fn backoff_ms(attempt: u32) -> u64 {
    (500u64 << attempt.min(4)).min(8_000)
}

pub fn reorder_tabs<T>(tabs: &mut Vec<T>, from: usize, to: usize) {
    if from >= tabs.len() || to >= tabs.len() || from == to {
        return;
    }
    let tab = tabs.remove(from);
    tabs.insert(to, tab);
}

pub fn drop_index(rel_x: f32, tab_w: f32, count: usize) -> usize {
    if count == 0 || tab_w <= 0.0 {
        return 0;
    }
    ((rel_x / tab_w).floor().max(0.0) as usize).min(count - 1)
}

pub fn slide_offset(ix: usize, from: usize, over: usize) -> f32 {
    if from < over && ix > from && ix <= over {
        -1.0
    } else if over < from && ix >= over && ix < from {
        1.0
    } else {
        0.0
    }
}

pub fn active_after_reorder(active: usize, from: usize, to: usize) -> usize {
    if active == from {
        to
    } else if from < active && to >= active {
        active - 1
    } else if from > active && to <= active {
        active + 1
    } else {
        active
    }
}

fn with_target(mut params: serde_json::Value, target: &Option<String>) -> serde_json::Value {
    if let (Some(target), Some(object)) = (target, params.as_object_mut()) {
        object.insert(
            "targetDeviceId".into(),
            serde_json::Value::String(target.clone()),
        );
    }
    params
}

pub fn active_after_close(active: usize, closed: usize, len_after: usize) -> usize {
    let shifted = if closed < active { active - 1 } else { active };
    if len_after == 0 {
        0
    } else {
        shifted.min(len_after - 1)
    }
}

pub fn exit_message(code: i32) -> Vec<u8> {
    format!("\r\n\x1b[90m[process exited {code}]\x1b[0m\r\n").into_bytes()
}

pub fn shell_title(shell: &str) -> String {
    let name = shell.rsplit(['/', '\\']).next().unwrap_or(shell).trim();
    if name.is_empty() {
        "terminal".to_string()
    } else {
        name.to_string()
    }
}

fn decode_base64(data: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(data))
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "terminal: dropping undecodable data frame");
            Vec::new()
        })
}

fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[derive(Debug, Clone, Copy)]
struct SelectionDrag {
    origin: gpui::Point<Pixels>,
    armed: bool,
}

struct TerminalTab {
    key: u64,
    title: SharedString,
    terminal_id: Option<String>,
    emulator: Emulator,
    exited: Option<i32>,
    last_seq: u64,
    coalescer: InputCoalescer,
    flush_task: Option<Task<()>>,
    resize_task: Option<Task<()>>,
    _run: Option<Task<()>>,
}

#[derive(Default)]
struct ChatTabs {
    tabs: Vec<TerminalTab>,
    active: usize,
}

struct DragState {
    from: usize,
    over: usize,
    epoch: usize,
    prev_over: usize,
}

struct TabDragPayload {
    chat: String,
    from: usize,
    title: SharedString,
}

struct TabGhost {
    title: SharedString,
}

impl Render for TabGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .w(px(TAB_WIDTH))
            .h(px(28.0))
            .px(px(Theme::SPACE_SM))
            .flex()
            .items_center()
            .rounded(px(Theme::CONTROL_RADIUS))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(px(12.0))
            .text_color(theme.text)
            .opacity(0.85)
            .child(div().truncate().child(self.title.clone()))
    }
}

pub struct TerminalPanel {
    state: Entity<AppState>,
    focus_handle: FocusHandle,
    chats: HashMap<String, ChatTabs>,
    open: bool,
    embedded: bool,
    tab_seq: u64,
    drag: Option<DragState>,
    last_selected: Option<String>,
    geometry: Option<GridGeometry>,
    selection_drag: Option<SelectionDrag>,
    _observe: Subscription,
}

impl TerminalPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.on_state_changed(cx));
        Self {
            state,
            focus_handle: cx.focus_handle(),
            chats: HashMap::new(),
            open: false,
            embedded: false,
            tab_seq: 0,
            drag: None,
            last_selected: None,
            geometry: None,
            selection_drag: None,
            _observe: observe,
        }
    }

    pub fn new_embedded(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut panel = Self::new(state, cx);
        panel.embedded = true;
        panel
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    pub fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.open = open;
        if open && !self.embedded {
            self.ensure_tab(cx);
        }
        cx.notify();
    }

    fn display_title(tab: &TerminalTab) -> SharedString {
        match tab.emulator.title().map(str::trim) {
            Some(title) if !title.is_empty() => title.to_string().into(),
            _ => tab.title.clone(),
        }
    }

    pub fn tab_summaries(&self, cx: &App) -> Vec<(u64, SharedString, bool)> {
        let Some(chat) = self.selected_chat(cx) else {
            return Vec::new();
        };
        self.chats
            .get(&chat)
            .map(|tabs| {
                tabs.tabs
                    .iter()
                    .map(|t| (t.key, Self::display_title(t), t.exited.is_some()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn open_tab_for_selected(&mut self, cx: &mut Context<Self>) -> Option<u64> {
        let chat = self.selected_chat(cx)?;
        self.open_tab(chat, cx);
        Some(self.tab_seq)
    }

    pub fn select_tab_by_key(&mut self, key: u64, cx: &mut Context<Self>) {
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        let Some(ix) = self
            .chats
            .get(&chat)
            .and_then(|tabs| tabs.tabs.iter().position(|t| t.key == key))
        else {
            return;
        };
        self.select_tab(&chat, ix, cx);
    }

    pub fn close_tab_by_key(&mut self, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        self.close_tab(&chat, key, window, cx);
    }

    fn on_state_changed(&mut self, cx: &mut Context<Self>) {
        let selected = self.state.read(cx).selected_chat.clone();
        let switched = selected != self.last_selected;
        if switched {
            self.last_selected = selected;
            self.drag = None;
        }
        if self.open && !self.embedded {
            self.ensure_tab(cx);
        }
        if switched {
            cx.notify();
        }
    }

    fn engine(&self, cx: &App) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    fn chat_target(&self, chat: &str, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        let device = state.chats.iter().find(|c| c.id == chat)?.device_id.clone();
        (state.local_device_id.as_deref() != Some(device.as_str())).then_some(device)
    }

    fn selected_chat(&self, cx: &App) -> Option<String> {
        self.state.read(cx).selected_chat.clone()
    }

    fn ensure_tab(&mut self, cx: &mut Context<Self>) {
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        if self.chats.get(&chat).is_none_or(|c| c.tabs.is_empty()) {
            self.open_tab(chat, cx);
        }
    }

    fn tab_mut(&mut self, chat: &str, key: u64) -> Option<&mut TerminalTab> {
        self.chats
            .get_mut(chat)?
            .tabs
            .iter_mut()
            .find(|t| t.key == key)
    }

    fn active_tab(&self, cx: &App) -> Option<&TerminalTab> {
        let chat = self.state.read(cx).selected_chat.clone()?;
        let tabs = self.chats.get(&chat)?;
        tabs.tabs.get(tabs.active)
    }

    fn open_tab(&mut self, chat: String, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.tab_seq += 1;
        let key = self.tab_seq;
        let entry = self.chats.entry(chat.clone()).or_default();
        let tab_no = entry.tabs.len() + 1;
        entry.tabs.push(TerminalTab {
            key,
            title: format!("Terminal {tab_no}").into(),
            terminal_id: None,
            emulator: Emulator::new(80, 24),
            exited: None,
            last_seq: 0,
            coalescer: InputCoalescer::default(),
            flush_task: None,
            resize_task: None,
            _run: None,
        });
        entry.active = entry.tabs.len() - 1;

        let target = self.chat_target(&chat, cx);
        let run = Self::spawn_session(chat.clone(), key, engine, target, cx);
        if let Some(tab) = self.tab_mut(&chat, key) {
            tab._run = Some(run);
        }
        cx.notify();
    }

    fn spawn_session(
        chat: String,
        key: u64,
        engine: EngineHandle,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let (cols, rows) = this
                .update(cx, |panel, _| {
                    panel
                        .tab_mut(&chat, key)
                        .map(|t| (t.emulator.cols() as u16, t.emulator.rows() as u16))
                        .unwrap_or((80, 24))
                })
                .unwrap_or((80, 24));

            let opened = engine
                .client()
                .call_as::<TerminalSession>(
                    methods::OPEN_TERMINAL,
                    with_target(
                        serde_json::json!({ "chatId": chat, "cols": cols, "rows": rows }),
                        &target,
                    ),
                )
                .await;
            let session = match opened {
                Ok(session) => session,
                Err(err) => {
                    tracing::warn!(error = %err, "OpenTerminal failed");
                    let _ = this.update(cx, |panel, cx| {
                        if let Some(tab) = panel.tab_mut(&chat, key) {
                            tab.emulator.feed(
                                format!("\x1b[31mfailed to open terminal: {err}\x1b[0m\r\n")
                                    .as_bytes(),
                            );
                            tab.exited = Some(-1);
                            cx.notify();
                        }
                    });
                    return;
                }
            };
            let terminal_id = session.id.clone();
            let attached = this
                .update(cx, |panel, cx| {
                    if let Some(tab) = panel.tab_mut(&chat, key) {
                        tab.terminal_id = Some(terminal_id.clone());
                        cx.notify();
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if !attached {
                let _ = engine
                    .client()
                    .call(
                        methods::CLOSE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": terminal_id }),
                            &target,
                        ),
                    )
                    .await;
                return;
            }

            let mut attempt: u32 = 0;
            loop {
                let Ok(after_seq) = this.update(cx, |panel, _| {
                    panel.tab_mut(&chat, key).map(|t| t.last_seq)
                }) else {
                    return;
                };
                let Some(after_seq) = after_seq else { return };

                let subscribed = engine
                    .client()
                    .subscribe(
                        methods::SUBSCRIBE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": terminal_id, "afterSeq": after_seq }),
                            &target,
                        ),
                    )
                    .await;
                let mut rx = match subscribed {
                    Ok(rx) => rx,
                    Err(err) => {
                        tracing::debug!(error = %err, attempt, "SubscribeTerminal failed; backing off");
                        cx.background_executor()
                            .timer(Duration::from_millis(backoff_ms(attempt)))
                            .await;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                };

                while let Some(value) = rx.recv().await {
                    let event: TerminalEvent = match serde_json::from_value(value) {
                        Ok(event) => event,
                        Err(err) => {
                            tracing::warn!(error = %err, "terminal: malformed stream frame");
                            continue;
                        }
                    };
                    attempt = 0;
                    let outcome = this.update(cx, |panel, cx| {
                        panel.apply_stream_event(&chat, key, &engine, event, cx)
                    });
                    match outcome {
                        Ok(StreamDisposition::Continue) => {}
                        Ok(StreamDisposition::Stop) => return,
                        Err(_) => return,
                    }
                }

                let done = this
                    .update(cx, |panel, _| {
                        panel.tab_mut(&chat, key).map(|t| t.exited.is_some()).unwrap_or(true)
                    })
                    .unwrap_or(true);
                if done {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(backoff_ms(attempt)))
                    .await;
                attempt = attempt.saturating_add(1);
            }
        })
    }

    fn apply_stream_event(
        &mut self,
        chat: &str,
        key: u64,
        engine: &EngineHandle,
        event: TerminalEvent,
        cx: &mut Context<Self>,
    ) -> StreamDisposition {
        let target = self.chat_target(chat, cx);
        let Some(tab) = self.tab_mut(chat, key) else {
            return StreamDisposition::Stop;
        };
        match event {
            TerminalEvent::Data { seq, data } => {
                tab.last_seq = seq;
                let responses = tab.emulator.feed(&decode_base64(&data));
                if !responses.is_empty()
                    && let Some(id) = tab.terminal_id.clone()
                {
                    let engine = engine.clone();
                    let data = encode_base64(&responses);
                    cx.spawn(async move |_, _| {
                        let _ = engine
                            .client()
                            .call(
                                methods::WRITE_TERMINAL,
                                with_target(
                                    serde_json::json!({ "terminalId": id, "data": data }),
                                    &target,
                                ),
                            )
                            .await;
                    })
                    .detach();
                }
                cx.notify();
                StreamDisposition::Continue
            }
            TerminalEvent::Exit { seq, exit_code, .. } => {
                tab.last_seq = seq;
                tab.exited = Some(exit_code);
                tab.emulator.feed(&exit_message(exit_code));
                cx.notify();
                StreamDisposition::Stop
            }
        }
    }

    fn queue_input(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        let Some(tabs) = self.chats.get_mut(&chat) else {
            return;
        };
        let active = tabs.active;
        let Some(tab) = tabs.tabs.get_mut(active) else {
            return;
        };
        if tab.exited.is_some() {
            return;
        }
        if tab.emulator.display_offset() > 0 {
            tab.emulator.scroll_to_bottom();
        }
        let key = tab.key;
        if tab.coalescer.push(bytes) {
            tab.flush_task = Some(Self::schedule_flush(chat, key, cx));
        }
    }

    fn schedule_flush(chat: String, key: u64, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(COALESCE_MS))
                .await;
            let _ = this.update(cx, |panel, cx| panel.flush_input(chat, key, cx));
        })
    }

    fn flush_input(&mut self, chat: String, key: u64, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let target = self.chat_target(&chat, cx);
        let Some(tab) = self.tab_mut(&chat, key) else {
            return;
        };
        if tab.coalescer.is_empty() {
            return;
        }
        let Some(id) = tab.terminal_id.clone() else {
            if tab.exited.is_none() {
                tab.flush_task = Some(Self::schedule_flush(chat, key, cx));
            }
            return;
        };
        let data = encode_base64(&tab.coalescer.take());
        cx.spawn(async move |_, _| {
            let _ = engine
                .client()
                .call(
                    methods::WRITE_TERMINAL,
                    with_target(
                        serde_json::json!({ "terminalId": id, "data": data }),
                        &target,
                    ),
                )
                .await;
        })
        .detach();
    }

    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed = self
            .active_tab(cx)
            .map(|tab| tab.emulator.bracketed_paste_mode())
            .unwrap_or(false);
        let bytes = paste_bytes(&text, bracketed);
        self.queue_input(&bytes, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let mods = &ks.modifiers;
        if ks.key == "v" && (mods.platform || (mods.control && mods.shift)) {
            self.paste_clipboard(cx);
            cx.stop_propagation();
            return;
        }
        if ks.key == "c"
            && (mods.platform || (mods.control && mods.shift))
            && self.copy_selection(cx)
        {
            cx.stop_propagation();
            return;
        }
        let app_cursor = self
            .active_tab(cx)
            .map(|tab| tab.emulator.app_cursor_mode())
            .unwrap_or(false);
        if let Some(bytes) = keystroke_bytes(&ks.key, ks.key_char.as_deref(), mods, app_cursor) {
            self.queue_input(&bytes, cx);
            cx.stop_propagation();
        }
    }

    pub fn on_grid_metrics(&mut self, geometry: GridGeometry, cx: &mut Context<Self>) {
        self.geometry = Some(geometry);
        let (cols, rows) = (geometry.cols, geometry.rows);
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        let Some(tabs) = self.chats.get_mut(&chat) else {
            return;
        };
        let active = tabs.active;
        let Some(tab) = tabs.tabs.get_mut(active) else {
            return;
        };
        if tab.emulator.cols() == cols as usize && tab.emulator.rows() == rows as usize {
            return;
        }
        tab.emulator.resize(cols, rows);
        let key = tab.key;
        let engine = self.engine(cx);
        let target = self.chat_target(&chat, cx);
        if let (Some(engine), Some(tab)) = (engine, self.tab_mut(&chat, key)) {
            let id = tab.terminal_id.clone();
            tab.resize_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(RESIZE_DEBOUNCE_MS))
                    .await;
                let Ok(current) = this.update(cx, |panel, _| {
                    panel
                        .tab_mut(&chat, key)
                        .map(|t| (t.terminal_id.clone(), t.emulator.cols(), t.emulator.rows()))
                }) else {
                    return;
                };
                let Some((stored_id, cols, rows)) = current else {
                    return;
                };
                let Some(id) = stored_id.or(id) else { return };
                let _ = engine
                    .client()
                    .call(
                        methods::RESIZE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": id, "cols": cols, "rows": rows }),
                            &target,
                        ),
                    )
                    .await;
            }));
        }
    }

    pub fn active_grid_snapshot(&self, cx: &App) -> Option<GridSnapshot> {
        let tab = self.active_tab(cx)?;
        Some(GridSnapshot {
            lines: tab.emulator.lines(),
            cursor: tab.emulator.cursor(),
        })
    }

    fn with_active_emulator<R>(
        &mut self,
        cx: &App,
        f: impl FnOnce(&mut Emulator) -> R,
    ) -> Option<R> {
        let chat = self.selected_chat(cx)?;
        let tabs = self.chats.get_mut(&chat)?;
        let active = tabs.active;
        tabs.tabs.get_mut(active).map(|tab| f(&mut tab.emulator))
    }

    fn grid_point_at(
        &mut self,
        position: gpui::Point<Pixels>,
        cx: &App,
    ) -> Option<(GridPoint, Side)> {
        let geometry = self.geometry?;
        let hit = cell_at(
            f32::from(position.x - geometry.origin.x),
            f32::from(position.y - geometry.origin.y),
            geometry.cell_w,
            geometry.line_h,
            geometry.cols as usize,
            geometry.rows as usize,
        );
        let point = self.with_active_emulator(cx, |emu| emu.grid_point(hit.row, hit.col))?;
        Some((point, hit.side))
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        let Some((point, side)) = self.grid_point_at(event.position, cx) else {
            return;
        };
        let ty = match event.click_count {
            0 => return,
            1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        let shift = event.modifiers.shift;
        if ty == SelectionType::Simple {
            let extended = shift
                && self
                    .with_active_emulator(cx, |emu| {
                        let extend = emu.has_selection();
                        if extend {
                            emu.update_selection(point, side);
                        }
                        extend
                    })
                    .unwrap_or(false);
            if extended {
                self.selection_drag = Some(SelectionDrag {
                    origin: event.position,
                    armed: true,
                });
                cx.notify();
                return;
            }
            self.with_active_emulator(cx, |emu| emu.clear_selection());
            self.selection_drag = Some(SelectionDrag {
                origin: event.position,
                armed: false,
            });
        } else {
            self.with_active_emulator(cx, |emu| emu.start_selection(ty, point, side));
            self.selection_drag = Some(SelectionDrag {
                origin: event.position,
                armed: true,
            });
        }
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.dragging() {
            return;
        }
        let Some(drag) = self.selection_drag else {
            return;
        };
        if !drag.armed {
            let dx = f32::from(event.position.x - drag.origin.x);
            let dy = f32::from(event.position.y - drag.origin.y);
            if dx.hypot(dy) < SELECTION_DRAG_THRESHOLD {
                return;
            }
            let Some((anchor, side)) = self.grid_point_at(drag.origin, cx) else {
                return;
            };
            self.with_active_emulator(cx, |emu| {
                emu.start_selection(SelectionType::Simple, anchor, side)
            });
            self.selection_drag = Some(SelectionDrag {
                armed: true,
                ..drag
            });
        }
        let Some((point, side)) = self.grid_point_at(event.position, cx) else {
            return;
        };
        self.with_active_emulator(cx, |emu| emu.update_selection(point, side));
        cx.notify();
    }

    fn on_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.selection_drag = None;
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self
            .with_active_emulator(cx, |emu| emu.selection_text())
            .flatten()
        else {
            return false;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        true
    }

    fn scroll_active(&mut self, delta_lines: i32, cx: &mut Context<Self>) {
        if delta_lines == 0 {
            return;
        }
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        let Some(tabs) = self.chats.get_mut(&chat) else {
            return;
        };
        let active = tabs.active;
        if let Some(tab) = tabs.tabs.get_mut(active) {
            tab.emulator.scroll(delta_lines);
            cx.notify();
        }
    }

    fn select_tab(&mut self, chat: &str, ix: usize, cx: &mut Context<Self>) {
        if let Some(tabs) = self.chats.get_mut(chat)
            && ix < tabs.tabs.len()
        {
            tabs.active = ix;
            cx.notify();
        }
    }

    fn close_tab(&mut self, chat: &str, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine(cx);
        let target = self.chat_target(chat, cx);
        let Some(tabs) = self.chats.get_mut(chat) else {
            return;
        };
        let Some(ix) = tabs.tabs.iter().position(|t| t.key == key) else {
            return;
        };
        let tab = tabs.tabs.remove(ix);
        tabs.active = active_after_close(tabs.active, ix, tabs.tabs.len());
        let now_empty = tabs.tabs.is_empty();
        self.drag = None;
        if now_empty && self.open && !self.embedded {
            window.dispatch_action(Box::new(ToggleTerminal), cx);
        }
        if let (Some(engine), Some(id)) = (engine, tab.terminal_id.clone()) {
            cx.spawn(async move |_, _| {
                let _ = engine
                    .client()
                    .call(
                        methods::CLOSE_TERMINAL,
                        with_target(serde_json::json!({ "terminalId": id }), &target),
                    )
                    .await;
            })
            .detach();
        }
        cx.notify();
    }

    fn commit_reorder(&mut self, chat: &str, from: usize, to: usize, cx: &mut Context<Self>) {
        if let Some(tabs) = self.chats.get_mut(chat) {
            let active = tabs.active;
            reorder_tabs(&mut tabs.tabs, from, to);
            tabs.active = active_after_reorder(active, from, to);
        }
        self.drag = None;
        cx.notify();
    }

    fn update_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.drag {
            Some(drag) if drag.over != over => {
                drag.prev_over = drag.over;
                drag.over = over;
                drag.epoch += 1;
                cx.notify();
            }
            Some(_) => {}
            None => {
                self.drag = Some(DragState {
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }

    fn render_tab_bar(&mut self, chat: &str, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = Theme::of(cx).clone();
        let tabs = self.chats.get(chat);
        let (active, count) = tabs.map(|t| (t.active, t.tabs.len())).unwrap_or((0, 0));
        let drag = self
            .drag
            .as_ref()
            .map(|d| (d.from, d.over, d.epoch, d.prev_over));
        let chat_owned = chat.to_string();

        let tab_elements: Vec<_> = tabs
            .map(|tabs| {
                tabs.tabs
                    .iter()
                    .enumerate()
                    .map(|(ix, tab)| {
                        let selected = ix == active;
                        let key = tab.key;
                        let title = Self::display_title(tab);
                        let exited = tab.exited.is_some();
                        (ix, key, title, selected, exited)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let bar_chat = chat_owned.clone();
        let drop_chat = chat_owned.clone();
        div()
            .id("terminal-tab-bar")
            .h(px(TAB_BAR_HEIGHT))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .pl(px(8.0))
            .pr(px(6.0))
            .border_b_1()
            .border_color(crate::theme::hairline(0.07))
            .on_drag_move::<TabDragPayload>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<TabDragPayload>, _, cx| {
                    let payload = event.drag(cx);
                    if payload.chat != bar_chat {
                        return;
                    }
                    let from = payload.from;
                    let rel_x = f32::from(event.event.position.x) - f32::from(event.bounds.left());
                    let over = drop_index(rel_x, TAB_WIDTH, count);
                    this.update_drag_over(from, over, cx);
                },
            ))
            .on_drop::<TabDragPayload>(cx.listener(move |this, payload: &TabDragPayload, _, cx| {
                if payload.chat != drop_chat {
                    this.drag = None;
                    cx.notify();
                    return;
                }
                let to = this.drag.as_ref().map(|d| d.over).unwrap_or(payload.from);
                let chat = drop_chat.clone();
                this.commit_reorder(&chat, payload.from, to, cx);
            }))
            .children(
                tab_elements
                    .into_iter()
                    .map(|(ix, key, title, selected, exited)| {
                        let chat_select = chat_owned.clone();
                        let chat_close = chat_owned.clone();
                        let chat_close2 = chat_owned.clone();
                        let chat_drag = chat_owned.clone();
                        let ghost_title = title.clone();
                        let (text_color, bg, glyph_alpha) = if selected {
                            (theme.text, crate::theme::ink(0.08), 0.8)
                        } else {
                            (
                                theme.text_muted.opacity(0.6),
                                gpui::transparent_black(),
                                0.6,
                            )
                        };
                        let close_btn = div()
                            .id(("terminal-tab-close", key))
                            .size(px(20.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .when(!selected, |el| el.invisible())
                            .cursor_pointer()
                            .hover(|s| s.bg(crate::theme::ink(0.09)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(&chat_close2, key, window, cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::CLOSE)
                                    .size(px(12.0))
                                    .text_color(theme.text_muted.opacity(0.8)),
                            );
                        let tab_el = div()
                            .id(("terminal-tab", key))
                            .w(px(TAB_WIDTH))
                            .h(px(28.0))
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .pl(px(8.0))
                            .pr(px(4.0))
                            .rounded(px(8.0))
                            .bg(motion::hover_blend(
                                &format!("term-tab-{key}"),
                                bg,
                                theme.element_hover,
                            ))
                            .on_hover(motion::hover_listener(format!("term-tab-{key}")))
                            .text_size(px(12.0))
                            .text_color(text_color)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_tab(&chat_select, ix, cx);
                            }))
                            .on_mouse_down(
                                MouseButton::Middle,
                                cx.listener(move |this, _, window, cx| {
                                    this.close_tab(&chat_close, key, window, cx);
                                }),
                            )
                            .on_drag(
                                TabDragPayload {
                                    chat: chat_drag,
                                    from: ix,
                                    title: ghost_title,
                                },
                                |payload, _point, _, cx| {
                                    let title = payload.title.clone();
                                    cx.stop_propagation();
                                    cx.new(|_| TabGhost { title })
                                },
                            )
                            .when(exited, |el| el.opacity(0.55))
                            .child(
                                crate::icons::icon(crate::icons::TERMINAL)
                                    .size(px(16.0))
                                    .text_color(text_color.opacity(glyph_alpha)),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(title))
                            .child(close_btn);

                        match drag {
                            Some((from, over, epoch, prev_over)) if ix != from => {
                                let target = slide_offset(ix, from, over) * TAB_WIDTH;
                                let start = slide_offset(ix, from, prev_over) * TAB_WIDTH;
                                div()
                                    .relative()
                                    .child(tab_el.with_animation(
                                        ("terminal-tab-slide", key | ((epoch as u64) << 32)),
                                        TAB_SLIDE.animation(),
                                        move |el, t| el.left(px(motion::lerp(start, target, t))),
                                    ))
                                    .into_any_element()
                            }
                            Some((from, ..)) if ix == from => div()
                                .w(px(TAB_WIDTH))
                                .h(px(28.0))
                                .flex_none()
                                .into_any_element(),
                            _ => tab_el.into_any_element(),
                        }
                    }),
            )
            .child(
                div()
                    .id("terminal-new-tab")
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .bg(motion::hover_blend(
                        "term-new-tab",
                        gpui::transparent_black(),
                        crate::theme::ink(0.05),
                    ))
                    .on_hover(motion::hover_listener("term-new-tab"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(chat) = this.selected_chat(cx) {
                            this.open_tab(chat, cx);
                        }
                    }))
                    .child(
                        crate::icons::icon(crate::icons::PLUS)
                            .size(px(16.0))
                            .text_color(theme.text_muted.opacity(0.6)),
                    ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("terminal-collapse")
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .bg(motion::hover_blend(
                        "term-collapse",
                        gpui::transparent_black(),
                        crate::theme::ink(0.05),
                    ))
                    .on_hover(motion::hover_listener("term-collapse"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleTerminal), cx);
                    })
                    .child(
                        crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                            .size(px(13.0))
                            .text_color(theme.text_muted.opacity(0.55)),
                    ),
            )
    }
}

enum StreamDisposition {
    Continue,
    Stop,
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        if self.drag.is_some() && !cx.has_active_drag() {
            self.drag = None;
        }
        let panel_bg: Option<gpui::Hsla> = (!self.embedded).then(|| terminal_panel_bg(&theme));
        let Some(chat) = self.selected_chat(cx) else {
            return div()
                .size_full()
                .when_some(panel_bg, |el, bg| el.bg(bg))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("Select a chat to open a terminal"))
                .into_any_element();
        };
        let focused = self.focus_handle.is_focused(window);

        let tab_bar: Option<gpui::AnyElement> =
            (!self.embedded).then(|| self.render_tab_bar(&chat, cx).into_any_element());
        div()
            .size_full()
            .flex()
            .flex_col()
            .when_some(panel_bg, |el, bg| el.bg(bg))
            .children(tab_bar)
            .child(
                div()
                    .id("terminal-body")
                    .flex_1()
                    .min_h_0()
                    .key_context("Terminal")
                    .track_focus(&self.focus_handle)
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _, cx| {
                        let lines = match event.delta {
                            ScrollDelta::Lines(delta) => delta.y,
                            ScrollDelta::Pixels(delta) => {
                                f32::from(delta.y) / super::view::TERM_LINE_HEIGHT
                            }
                        };
                        let step = lines.round() as i32;
                        this.scroll_active(step, cx);
                    }))
                    .child(TerminalElement::new(cx.entity(), focused)),
            )
            .into_any_element()
    }
}

impl TerminalGridHost for TerminalPanel {
    fn on_grid_metrics(&mut self, geometry: GridGeometry, cx: &mut Context<Self>) {
        TerminalPanel::on_grid_metrics(self, geometry, cx);
    }

    fn active_grid_snapshot(&self, cx: &App) -> Option<GridSnapshot> {
        TerminalPanel::active_grid_snapshot(self, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn height_clamps_between_160_and_55vh() {
        assert_eq!(clamp_terminal_height(300.0, 900.0), 300.0);
        assert_eq!(clamp_terminal_height(10.0, 900.0), 160.0);
        assert_eq!(clamp_terminal_height(4000.0, 900.0), 900.0 * 0.55);
        assert_eq!(clamp_terminal_height(200.0, 100.0), 160.0);
        assert_eq!(clamp_terminal_height(f32::NAN, 900.0), 160.0);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_ms(0), 500);
        assert_eq!(backoff_ms(1), 1000);
        assert_eq!(backoff_ms(2), 2000);
        assert_eq!(backoff_ms(3), 4000);
        assert_eq!(backoff_ms(4), 8000);
        assert_eq!(backoff_ms(10), 8000);
        assert_eq!(backoff_ms(u32::MAX), 8000);
    }

    #[test]
    fn reorder_moves_forward_and_backward() {
        let mut v = vec!["a", "b", "c", "d"];
        reorder_tabs(&mut v, 0, 2);
        assert_eq!(v, ["b", "c", "a", "d"]);
        reorder_tabs(&mut v, 3, 0);
        assert_eq!(v, ["d", "b", "c", "a"]);
        reorder_tabs(&mut v, 9, 0);
        reorder_tabs(&mut v, 1, 1);
        assert_eq!(v, ["d", "b", "c", "a"]);
    }

    #[test]
    fn drop_index_quantizes_and_clamps() {
        assert_eq!(drop_index(-10.0, 150.0, 3), 0);
        assert_eq!(drop_index(0.0, 150.0, 3), 0);
        assert_eq!(drop_index(149.0, 150.0, 3), 0);
        assert_eq!(drop_index(150.0, 150.0, 3), 1);
        assert_eq!(drop_index(700.0, 150.0, 3), 2);
        assert_eq!(drop_index(50.0, 150.0, 0), 0);
    }

    #[test]
    fn slide_offsets_shift_toward_the_gap() {
        assert_eq!(slide_offset(0, 0, 2), 0.0);
        assert_eq!(slide_offset(1, 0, 2), -1.0);
        assert_eq!(slide_offset(2, 0, 2), -1.0);
        assert_eq!(slide_offset(3, 0, 2), 0.0);
        assert_eq!(slide_offset(0, 3, 1), 0.0);
        assert_eq!(slide_offset(1, 3, 1), 1.0);
        assert_eq!(slide_offset(2, 3, 1), 1.0);
        assert_eq!(slide_offset(3, 3, 1), 0.0);
        for ix in 0..4 {
            assert_eq!(slide_offset(ix, 2, 2), 0.0);
        }
    }

    #[test]
    fn active_index_tracks_reorders() {
        assert_eq!(active_after_reorder(1, 1, 3), 3);
        assert_eq!(active_after_reorder(2, 0, 3), 1);
        assert_eq!(active_after_reorder(1, 3, 0), 2);
        assert_eq!(active_after_reorder(0, 2, 3), 0);
    }

    #[test]
    fn active_index_tracks_closes() {
        assert_eq!(active_after_close(2, 0, 3), 1);
        assert_eq!(active_after_close(1, 1, 2), 1);
        assert_eq!(active_after_close(2, 2, 2), 1);
        assert_eq!(active_after_close(0, 0, 0), 0);
    }

    #[test]
    fn exit_message_format() {
        let text = String::from_utf8(exit_message(0)).unwrap();
        assert!(text.contains("[process exited 0]"));
        let text = String::from_utf8(exit_message(137)).unwrap();
        assert!(text.contains("[process exited 137]"));
        assert!(text.starts_with("\r\n"));
        assert!(text.ends_with("\r\n"));
    }

    #[test]
    fn shell_titles() {
        assert_eq!(shell_title("/bin/zsh"), "zsh");
        assert_eq!(shell_title("/usr/local/bin/fish"), "fish");
        assert_eq!(shell_title("C:\\Windows\\System32\\cmd.exe"), "cmd.exe");
        assert_eq!(shell_title("bash"), "bash");
        assert_eq!(shell_title(""), "terminal");
    }

    #[test]
    fn stream_events_deserialize_per_contract() {
        let data: TerminalEvent =
            serde_json::from_str(r#"{"type":"data","seq":7,"data":"aGk="}"#).unwrap();
        assert_eq!(
            data,
            TerminalEvent::Data {
                seq: 7,
                data: "aGk=".into()
            }
        );
        let exit: TerminalEvent =
            serde_json::from_str(r#"{"type":"exit","seq":8,"exitCode":130}"#).unwrap();
        assert_eq!(
            exit,
            TerminalEvent::Exit {
                seq: 8,
                exit_code: 130,
                signal: None
            }
        );
        let session: TerminalSession =
            serde_json::from_str(r#"{"id":"t1","cwd":"/w","shell":"/bin/zsh"}"#).unwrap();
        assert_eq!(session.id, "t1");
        assert_eq!(session.shell, "/bin/zsh");
    }

    #[test]
    fn base64_round_trip_and_tolerance() {
        assert_eq!(decode_base64("aGk="), b"hi".to_vec());
        assert_eq!(
            decode_base64("aGk"),
            b"hi".to_vec(),
            "unpadded input tolerated"
        );
        assert_eq!(
            decode_base64("!!!"),
            Vec::<u8>::new(),
            "garbage decodes to nothing"
        );
        assert_eq!(encode_base64(b"hi"), "aGk=");
    }

    #[test]
    fn exit_message_feeds_cleanly_through_the_emulator() {
        let mut emulator = Emulator::new(40, 4);
        emulator.feed(b"$ done");
        emulator.feed(&exit_message(1));
        assert_eq!(emulator.row_text(1), "[process exited 1]");
    }
}
