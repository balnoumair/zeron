use gpui::{AnyElement, SharedString, div, prelude::*, px};

use crate::theme::{Theme, ink};

pub fn page_column() -> gpui::Div {
    div()
        .w_full()
        .max_w(px(768.0))
        .mx_auto()
        .px(px(24.0))
        .pt(px(32.0))
        .pb(px(64.0))
        .flex()
        .flex_col()
}

pub fn page_header(theme: &Theme, title: &str, count: Option<usize>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap(px(10.0))
        .child(
            div()
                .text_size(px(16.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(SharedString::from(title.to_string())),
        )
        .when_some(count, |el, count| {
            el.child(
                div()
                    .text_size(px(13.0))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(SharedString::from(format!("{count}"))),
            )
        })
}

pub fn page_subtitle(theme: &Theme, copy: impl Into<SharedString>) -> gpui::Div {
    div()
        .mt(px(4.0))
        .text_size(px(13.0))
        .text_color(theme.text_muted)
        .child(copy.into())
}

pub fn field_label(theme: &Theme, label: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(13.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text)
        .child(label.into())
}

pub fn option_card_row() -> gpui::Div {
    div().flex().flex_row().items_start().gap(px(16.0)).w_full()
}

pub const OPTION_CARD_HEIGHT: f32 = 148.0;
pub const OPTION_CARD_RADIUS: f32 = 10.0;
const RING_GAP: f32 = 2.0;
const RING_WIDTH: f32 = 2.0;

pub fn option_card(
    theme: &Theme,
    label: impl Into<SharedString>,
    selected: bool,
    preview: AnyElement,
) -> gpui::Div {
    let frame = div()
        .h(px(OPTION_CARD_HEIGHT))
        .w_full()
        .rounded(px(OPTION_CARD_RADIUS))
        .overflow_hidden()
        .border_1()
        .border_color(theme.border)
        .child(preview);

    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(8.0))
        .cursor_pointer()
        .child(
            div()
                .w_full()
                .rounded(px(OPTION_CARD_RADIUS + RING_GAP + RING_WIDTH))
                .p(px(RING_GAP))
                .border_2()
                .border_color(if selected {
                    theme.accent
                } else {
                    gpui::transparent_black()
                })
                .child(frame),
        )
        .child(
            div()
                .text_size(px(13.0))
                .text_color(if selected {
                    theme.text
                } else {
                    theme.text_muted
                })
                .child(label.into()),
        )
}

pub fn section_card(theme: &Theme) -> gpui::Div {
    div()
        .mt(px(24.0))
        .rounded(px(12.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.card_glass_bg())
        .overflow_hidden()
        .flex()
        .flex_col()
}

pub fn card_row(theme: &Theme, first: bool) -> gpui::Div {
    div()
        .px(px(20.0))
        .py(px(14.0))
        .when(!first, |el| el.border_t_1().border_color(theme.border))
        .hover(|s| s.bg(ink(0.015)))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(14.0))
}

pub fn row_tile(theme: &Theme, icon_path: &'static str) -> gpui::Div {
    div()
        .flex_none()
        .size(px(36.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(theme.border)
        .bg(ink(0.03))
        .flex()
        .items_center()
        .justify_center()
        .child(
            crate::icons::icon(icon_path)
                .size(px(16.0))
                .text_color(theme.text_muted),
        )
}

pub fn row_title(theme: &Theme, title: impl Into<SharedString>) -> gpui::Div {
    div()
        .min_w_0()
        .truncate()
        .text_size(px(13.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text)
        .child(title.into())
}

pub fn meta_line(theme: &Theme, fragments: Vec<AnyElement>) -> gpui::Div {
    let mut line = div()
        .mt(px(4.0))
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_x(px(8.0))
        .gap_y(px(2.0))
        .text_size(px(11.5))
        .text_color(theme.text_muted.opacity(0.65));
    let mut first = true;
    for fragment in fragments {
        if !first {
            line = line.child(
                div()
                    .text_color(theme.text_muted.opacity(0.3))
                    .child(SharedString::from("·")),
            );
        }
        line = line.child(fragment);
        first = false;
    }
    line
}

pub fn badge(theme: &Theme, label: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex_none()
        .px(px(8.0))
        .py(px(2.0))
        .rounded_full()
        .border_1()
        .border_color(theme.border)
        .text_size(px(10.5))
        .text_color(theme.text_muted)
        .child(label.into())
}

pub fn badge_active(theme: &Theme, label: impl Into<SharedString>) -> gpui::Div {
    let emerald = theme.success;
    let emerald_text = theme.success_muted;
    div()
        .flex_none()
        .px(px(8.0))
        .py(px(2.0))
        .rounded_full()
        .bg(emerald.opacity(0.12))
        .text_size(px(10.5))
        .text_color(emerald_text.opacity(0.9))
        .child(label.into())
}

pub fn toggle_switch(theme: &Theme, on: bool) -> gpui::Div {
    div()
        .flex_none()
        .w(px(32.0))
        .h(px(18.0))
        .rounded_full()
        .bg(if on { theme.text } else { ink(0.15) })
        .relative()
        .child(
            div()
                .absolute()
                .top(px(2.0))
                .left(px(if on { 16.0 } else { 2.0 }))
                .size(px(14.0))
                .rounded_full()
                .bg(if on { theme.on_solid } else { ink(0.7) }),
        )
}

pub fn ghost_action(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .rounded(px(8.0))
        .px(px(10.0))
        .py(px(6.0))
        .text_size(px(12.0))
        .text_color(theme.text_muted)
        .cursor_pointer()
}

pub fn ghost_hover(theme: &Theme, s: gpui::StyleRefinement) -> gpui::StyleRefinement {
    s.bg(ink(0.06)).text_color(theme.text)
}

pub fn error_strip(theme: &Theme, message: impl Into<SharedString>) -> gpui::Div {
    let red = theme.danger;
    let red_text = theme.danger_muted;
    div()
        .mt(px(16.0))
        .px(px(16.0))
        .py(px(12.0))
        .rounded(px(12.0))
        .border_1()
        .border_color(red.opacity(0.2))
        .bg(red.opacity(0.06))
        .text_size(px(12.5))
        .text_color(red_text.opacity(0.9))
        .flex()
        .flex_row()
        .items_start()
        .gap(px(8.0))
        .child(
            div().flex_none().mt(px(2.0)).child(
                crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                    .size(px(16.0))
                    .text_color(red_text.opacity(0.9)),
            ),
        )
        .child(div().min_w_0().child(message.into()))
}

pub fn warning_strip(theme: &Theme, message: impl Into<SharedString>) -> gpui::Div {
    let amber = theme.warning;
    let amber_text = theme.warning_muted;
    div()
        .mt(px(8.0))
        .px(px(16.0))
        .py(px(10.0))
        .rounded(px(12.0))
        .border_1()
        .border_color(amber.opacity(0.2))
        .bg(amber.opacity(0.06))
        .text_size(px(12.0))
        .text_color(amber_text.opacity(0.9))
        .flex()
        .flex_row()
        .items_start()
        .gap(px(8.0))
        .child(
            div().flex_none().mt(px(2.0)).child(
                crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                    .size(px(14.0))
                    .text_color(amber_text.opacity(0.9)),
            ),
        )
        .child(div().min_w_0().child(message.into()))
}
