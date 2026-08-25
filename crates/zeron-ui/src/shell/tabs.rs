use super::*;

impl Shell {
    pub(super) fn boot_select_chat(&mut self, cx: &mut Context<Self>) {
        let first = {
            let state = self.state.read(cx);
            if !state.chats_synced || state.selected_chat.is_some() || state.auto_selected {
                return;
            }
            state
                .overview_chats(Utc::now())
                .first()
                .map(|(_, c)| c.id.clone())
        };
        if let Some(first) = first {
            self.state
                .update(cx, |s, cx| s.select_chat(Some(first), cx));
        }
    }

    pub(super) fn open_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.state
            .update(cx, |s, cx| s.select_chat(Some(chat_id), cx));
        cx.notify();
    }

    pub(super) fn open_new_session(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        let target = {
            let state = self.state.read(cx);
            self.settings
                .space_filter
                .clone()
                .filter(|id| state.space_row(id).is_some())
        };
        self.state.update(cx, |s, cx| {
            if target.is_some() {
                s.select_space(target, cx);
            }
            s.select_chat(None, cx);
        });
        cx.notify();
    }

    pub(super) fn open_new_session_in_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.state.update(cx, |s, cx| {
            s.select_space(Some(space_id), cx);
            s.select_chat(None, cx);
        });
        cx.notify();
    }

    pub(super) fn render_session_title_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let (title, harness, on_canvas): (SharedString, Option<zeron_proto::HarnessId>, bool) = {
            let state = self.state.read(cx);
            match state.selected_chat_row() {
                Some(chat) => (
                    SharedString::from(transcript::single_line(
                        &chat.title.clone().unwrap_or_else(|| "New session".into()),
                    )),
                    chat.config.as_ref().map(|c| c.harness),
                    false,
                ),
                None => (SharedString::from(""), None, true),
            }
        };

        let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
        let plus_inset = 26.0 * self.titlebar_plus_alpha();

        let content_left =
            (sidebar_now + Theme::SPACE_LG).max(self.title_bar_content_start() + plus_inset);

        let takeover = !on_canvas && self.right_pane_open(cx) && self.right_pane_expanded;
        let row_left = if takeover {
            let cluster_end = self.title_bar_content_start() - 10.0 + plus_inset - 14.0;
            (sidebar_now - 8.0).max(cluster_end)
        } else {
            content_left
        };
        let trailing: Option<gpui::AnyElement> = if on_canvas {
            None
        } else if self.right_pane_open(cx) {
            let right_now = self.eval_tween(self.right_tween, self.right_target(cx));
            let pr = self.titlebar_right_pad(Theme::SPACE_LG);
            let gap_budget = if takeover { 8.0 } else { 16.0 };
            let avail = self.viewport_width - row_left - pr - gap_budget;
            let controls = self.render_right_tab_strip(cx);
            Some(
                div()
                    .flex_none()
                    .h_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .overflow_hidden()
                    .w(px((right_now - pr).min(avail).max(0.0)))
                    .pl(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_hidden()
                            .child(controls),
                    )
                    .child(header_icon_button(
                        "expand-changes",
                        icons::EXPAND_ARROWS,
                        &theme,
                        cx.listener(|this, _, _, cx| this.toggle_right_pane_expand(cx)),
                    ))
                    .child(header_icon_button(
                        "toggle-changes",
                        icons::SIDEBAR_MINIMALISTIC,
                        &theme,
                        cx.listener(|this, _, _, cx| this.toggle_right_pane(cx)),
                    ))
                    .into_any_element(),
            )
        } else {
            Some(
                header_icon_button(
                    "toggle-changes",
                    icons::SIDEBAR_MINIMALISTIC,
                    &theme,
                    cx.listener(|this, _, _, cx| this.toggle_right_pane(cx)),
                )
                .into_any_element(),
            )
        };

        let inner = div()
            .size_full()
            .flex()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .gap(px(8.0))
            .pl(px(row_left))
            .pr(px(self.titlebar_right_pad(Theme::SPACE_LG)))
            .when(!takeover, |el| {
                el.child(
                    div()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .when_some(
                            harness.map(crate::pickers::harness_brand_icon),
                            |el, (path, tint)| {
                                el.child(
                                    icon(path)
                                        .size(px(14.0))
                                        .flex_none()
                                        .text_color(tint.unwrap_or(theme.text_muted)),
                                )
                            },
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(if on_canvas {
                                    theme.text_muted.opacity(0.7)
                                } else {
                                    theme.text.opacity(0.85)
                                })
                                .child(title),
                        ),
                )
            })
            .child(div().flex_1())
            .children(trailing);

        let bar = div().h(px(Theme::TITLEBAR_HEIGHT)).flex_none().child(inner);
        self.titlebar_drag_region("chat-titlebar", bar, cx)
            .into_any_element()
    }
}
