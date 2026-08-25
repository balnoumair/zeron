use gpui::{AnyElement, Context, ListOffset, SharedString, div, prelude::*, px};
use std::time::{Duration, Instant};

use zeron_doc::{MessagePart, MessageRole, SessionMessageEntry};

use crate::motion;
use crate::popover;
use crate::theme::Theme;
use crate::transcript::Transcript;

pub use onyx_ui::rail::{
    GlideTimeline, MAX_RAIL_TICKS, PREVIEW_PROMPT_CHARS, PREVIEW_REPLY_CHARS,
    RAIL_MIN_CONTAINER_WIDTH, RAIL_V_MARGIN, RailTick, TICK_GAP, TICK_SLOT, active_tick, bucket_of,
    rail_capacity, rail_slots, rail_visible, tick_buckets, truncate_preview,
};

fn user_text(entry: &SessionMessageEntry) -> String {
    let raw = entry
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    crate::attachments::user_message_rail_text(&raw)
}

fn first_reply_text(entries: &[SessionMessageEntry]) -> Option<String> {
    entries
        .iter()
        .find(|e| e.role == MessageRole::Assistant)
        .and_then(|entry| {
            entry.parts.iter().find_map(|part| match part {
                MessagePart::Text { text, .. } if !text.trim().is_empty() => {
                    Some(text.trim().to_string())
                }
                _ => None,
            })
        })
}

pub fn rail_ticks(
    entries: &[SessionMessageEntry],
    echoes: &[SessionMessageEntry],
) -> Vec<RailTick> {
    let mut ticks: Vec<RailTick> = Vec::new();
    for (ix, entry) in entries.iter().enumerate() {
        if entry.role != MessageRole::User {
            continue;
        }
        ticks.push(RailTick {
            message_id: entry.id.clone(),
            prompt: user_text(entry),
            reply: first_reply_text(&entries[ix + 1..]),
        });
    }
    for echo in echoes {
        if echo.role == MessageRole::User && !ticks.iter().any(|t| t.message_id == echo.id) {
            ticks.push(RailTick {
                message_id: echo.id.clone(),
                prompt: user_text(echo),
                reply: None,
            });
        }
    }
    ticks
}

fn scroll_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("ZERON_SCROLL_TRACE").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

impl Transcript {
    pub fn scroll_to_row(&mut self, target: usize, cx: &mut Context<Self>) {
        if motion::reduced_motion(cx) {
            self.list_state().scroll_to(ListOffset {
                item_ix: target,
                offset_in_item: px(0.0),
            });
            cx.notify();
            return;
        }
        self.set_scroll_task(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let total = motion::SCROLL_GLIDE.total().mul_f32(motion::speed_scale());
            let mut timeline = GlideTimeline::new();
            let mut height_ema: Option<f32> = None;
            let trace = scroll_trace_enabled();
            let frames = (total.as_millis() / 16) as usize + 90;
            for _ in 0..frames {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let raw = (started.elapsed().as_secs_f32() / total.as_secs_f32()).min(1.0);
                let eased = motion::SCROLL_GLIDE.curve.eval(raw);
                let frac = timeline.step(eased);
                let done = this.update(cx, |t, cx| {
                    let list = t.list_state().clone();
                    if raw >= 1.0 {
                        list.scroll_to(ListOffset {
                            item_ix: target,
                            offset_in_item: px(0.0),
                        });
                        cx.notify();
                        return true;
                    }
                    let viewport = f32::from(list.viewport_bounds().size.height);
                    if t.is_glued() && viewport > 0.0 {
                        list.scroll_by(px(-(viewport + 0.5)));
                    }
                    let top = list.logical_scroll_top();
                    let top_height = list
                        .bounds_for_item(top.item_ix)
                        .map(|b| f32::from(b.size.height).max(1.0));
                    if viewport > 0.0 {
                        let bottom = f32::from(list.viewport_bounds().bottom());
                        let mut ix = top.item_ix;
                        let mut count = 0.0f32;
                        while let Some(b) = list.bounds_for_item(ix) {
                            if f32::from(b.top()) >= bottom {
                                break;
                            }
                            count += 1.0;
                            ix += 1;
                        }
                        if count > 0.0 {
                            let mean = viewport / count;
                            let ema = height_ema.get_or_insert(mean);
                            *ema += 0.5 * (mean - *ema);
                        }
                    }
                    if height_ema.is_none() {
                        height_ema = top_height;
                    }
                    let here = top.item_ix as f32
                        + top_height
                            .map(|h| (f32::from(top.offset_in_item) / h).clamp(0.0, 1.0))
                            .unwrap_or(0.0);
                    if trace {
                        tracing::warn!(
                            ms = started.elapsed().as_millis() as u64,
                            eased,
                            here,
                            dist = t.distance_from_bottom(),
                            "scroll-glide"
                        );
                    }

                    if target < top.item_ix {
                        let next = here - frac * (here - target as f32);
                        let step_px = (here - next) * height_ema.unwrap_or(0.0);
                        if step_px > 0.0 && step_px <= crate::transcript::OVERDRAW_PX * 0.8 {
                            list.scroll_by(px(-step_px));
                            cx.notify();
                            return false;
                        }
                        let ix = (next.floor().max(0.0) as usize).min(top.item_ix);
                        let within = next - ix as f32;
                        let offset = if ix == top.item_ix {
                            top_height
                                .map(|h| (within * h).min(f32::from(top.offset_in_item)))
                                .unwrap_or(0.0)
                        } else {
                            within * height_ema.unwrap_or(0.0)
                        };
                        list.scroll_to(ListOffset {
                            item_ix: ix,
                            offset_in_item: px(offset),
                        });
                        cx.notify();
                        return false;
                    }
                    match list.bounds_for_item(target) {
                        Some(bounds) => {
                            let delta = f32::from(bounds.top() - list.viewport_bounds().top());
                            list.scroll_by(px(frac * delta));
                        }
                        None => {
                            let next = here + frac * (target as f32 - here);
                            let ix = (next.floor().max(0.0) as usize).min(target);
                            let within = next - ix as f32;
                            list.scroll_to(ListOffset {
                                item_ix: ix,
                                offset_in_item: px(within * height_ema.unwrap_or(0.0)),
                            });
                        }
                    }
                    cx.notify();
                    false
                });
                match done {
                    Ok(true) | Err(_) => return,
                    Ok(false) => {}
                }
            }
            this.update(cx, |t, cx| {
                t.list_state().scroll_to(ListOffset {
                    item_ix: target,
                    offset_in_item: px(0.0),
                });
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn render_rail(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if !self.rail_enabled() {
            return gpui::Empty.into_any_element();
        }
        let (entries, echoes) = {
            let state = self.state_entity().read(cx);
            (state.transcript.clone(), state.pending_echoes().to_vec())
        };
        let ticks = rail_ticks(&entries, &echoes);
        let pairs: Vec<(RailTick, usize)> = ticks
            .into_iter()
            .filter_map(|tick| {
                let row = self
                    .rows()
                    .iter()
                    .position(|r| r.id.as_ref() == tick.message_id.as_str())?;
                Some((tick, row))
            })
            .collect();
        if pairs.len() < 2 {
            return gpui::Empty.into_any_element();
        }
        let tick_rows: Vec<usize> = pairs.iter().map(|(_, row)| *row).collect();
        let mut top_row = self.list_state().logical_scroll_top().item_ix;
        let read_top = f32::from(self.list_state().viewport_bounds().top())
            + crate::transcript::OWN_SEND_TOP_INSET_PX
            + 0.5;
        while let Some(bounds) = self.list_state().bounds_for_item(top_row + 1) {
            if f32::from(bounds.top()) <= read_top {
                top_row += 1;
            } else {
                break;
            }
        }
        let active = active_tick(&tick_rows, top_row);
        let hover = self.rail_hover();
        let theme = Theme::of(cx).clone();

        let viewport_h = f32::from(self.list_state().viewport_bounds().size.height);
        let capacity = rail_slots(if viewport_h > 0.0 { viewport_h } else { 600.0 });
        let buckets = tick_buckets(pairs.len(), capacity);
        let active_bucket = active.and_then(|ix| bucket_of(&buckets, ix));

        div()
            .absolute()
            .left(px(16.0))
            .top_0()
            .bottom_0()
            .w(px(26.0))
            .flex()
            .flex_col()
            .items_start()
            .justify_center()
            .gap(px(TICK_GAP))
            .children(buckets.into_iter().enumerate().map(|(ix, (start, end))| {
                let rep = active.filter(|&a| a >= start && a < end).unwrap_or(start);
                let (tick, row) = &pairs[rep];
                let (tick, row) = (tick.clone(), *row);
                let bucket_len = end - start;
                let is_active = active_bucket == Some(ix);
                let is_hovered = hover == Some(ix);
                let bar_width = if is_hovered { 20.0 } else { 12.0 };
                let bar_color = if is_active || is_hovered {
                    theme.text.opacity(0.8)
                } else {
                    crate::theme::ink(0.16)
                };
                let prompt = truncate_preview(&tick.prompt, PREVIEW_PROMPT_CHARS);
                let reply = tick
                    .reply
                    .as_deref()
                    .map(|r| truncate_preview(r, PREVIEW_REPLY_CHARS));
                let card: Option<AnyElement> = is_hovered.then(|| {
                    let card = popover::popover_card(&theme)
                        .w(px(280.0))
                        .p(px(Theme::SPACE_SM))
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme.text)
                                .child(SharedString::from(prompt.clone())),
                        )
                        .when_some(reply.clone(), |el, reply| {
                            el.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(reply)),
                            )
                        })
                        .when(bucket_len > 1, |el| {
                            el.child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(theme.text_muted.opacity(0.7))
                                    .child(SharedString::from(format!("{bucket_len} prompts"))),
                            )
                        });
                    crate::frost::frosted(12.0, crate::frost::MENU_BLUR, card).into_any_element()
                });
                div()
                    .id(("rail-tick", ix))
                    .relative()
                    .h(px(TICK_SLOT))
                    .w_full()
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        this.set_rail_hover(if *hovered { Some(ix) } else { None });
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.scroll_to_row(row, cx);
                    }))
                    .child(
                        div()
                            .h(px(2.0))
                            .w(px(bar_width))
                            .rounded(px(1.0))
                            .bg(bar_color),
                    )
                    .when_some(card, |el, card| {
                        el.child(gpui::deferred(
                            gpui::anchored()
                                .anchor(gpui::Anchor::LeftCenter)
                                .snap_to_window_with_margin(px(8.0))
                                .child(div().pl(px(26.0)).child(card)),
                        ))
                    })
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_doc::MessageStatus;

    fn entry(id: &str, role: MessageRole, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.into(),
            }],
            created_at: 0,
            device_id: "d".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    #[test]
    fn capacity_counts_slots_that_fit() {
        assert_eq!(rail_capacity(880.0), 64);
        assert_eq!(rail_capacity(0.0), 1);
        assert!(rail_capacity(200.0) >= 10);
        assert_eq!(rail_slots(880.0), MAX_RAIL_TICKS);
        assert_eq!(rail_slots(2000.0), MAX_RAIL_TICKS);
        assert!(rail_slots(100.0) < MAX_RAIL_TICKS);
    }

    #[test]
    fn buckets_are_identity_under_capacity() {
        let b = tick_buckets(5, 64);
        assert_eq!(b.len(), 5);
        assert!(
            b.iter()
                .enumerate()
                .all(|(k, &(s, e))| s == k && e == k + 1)
        );
    }

    #[test]
    fn buckets_partition_evenly_over_capacity() {
        let n = 100;
        let b = tick_buckets(n, 8);
        assert_eq!(b.len(), 8);
        assert_eq!(b[0].0, 0);
        assert_eq!(b.last().unwrap().1, n);
        for w in b.windows(2) {
            assert_eq!(w[0].1, w[1].0, "contiguous");
        }
        for &(s, e) in &b {
            assert!((e - s) == 12 || (e - s) == 13, "even split, got {}", e - s);
        }
    }

    #[test]
    fn bucket_of_maps_ticks_to_their_bucket() {
        let b = tick_buckets(10, 3);
        assert_eq!(bucket_of(&b, 0), Some(0));
        assert_eq!(bucket_of(&b, 3), Some(1));
        assert_eq!(bucket_of(&b, 9), Some(2));
        assert_eq!(bucket_of(&b, 10), None);
        assert!(tick_buckets(0, 8).is_empty());
        assert_eq!(tick_buckets(3, 0), vec![(0, 3)]);
    }

    #[test]
    fn ticks_map_user_prompts_with_reply_openings() {
        let entries = vec![
            entry("u1", MessageRole::User, "first question"),
            entry("a1", MessageRole::Assistant, "first answer"),
            entry("u2", MessageRole::User, "second question"),
            entry("a2", MessageRole::Assistant, "second answer"),
        ];
        let ticks = rail_ticks(&entries, &[]);
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[0].message_id, "u1");
        assert_eq!(ticks[0].prompt, "first question");
        assert_eq!(ticks[0].reply.as_deref(), Some("first answer"));
        assert_eq!(ticks[1].reply.as_deref(), Some("second answer"));
    }

    #[test]
    fn ticks_include_echoes_deduped() {
        let entries = vec![entry("u1", MessageRole::User, "sent")];
        let echoes = vec![
            entry("u1", MessageRole::User, "sent"),
            entry("u2", MessageRole::User, "pending"),
        ];
        let ticks = rail_ticks(&entries, &echoes);
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[1].message_id, "u2");
        assert_eq!(ticks[1].reply, None);
    }

    #[test]
    fn tick_without_reply_yet() {
        let entries = vec![
            entry("u1", MessageRole::User, "q"),
            entry("a1", MessageRole::Assistant, "reply to first"),
            entry("u2", MessageRole::User, "latest"),
        ];
        let ticks = rail_ticks(&entries, &[]);
        assert_eq!(ticks[1].reply, None);
        assert!(rail_ticks(&[], &[]).is_empty());
    }

    #[test]
    fn active_tick_tracks_viewport_top() {
        let tick_rows = [0, 5, 9];
        assert_eq!(active_tick(&tick_rows, 0), Some(0));
        assert_eq!(active_tick(&tick_rows, 4), Some(0));
        assert_eq!(active_tick(&tick_rows, 5), Some(1));
        assert_eq!(active_tick(&tick_rows, 8), Some(1));
        assert_eq!(active_tick(&tick_rows, 100), Some(2));
        assert_eq!(active_tick(&[3, 7], 1), Some(0));
        assert_eq!(active_tick(&[], 4), None);
    }

    #[test]
    fn rail_width_gate() {
        assert!(rail_visible(768.0));
        assert!(rail_visible(1200.0));
        assert!(!rail_visible(767.9));
        assert!(!rail_visible(0.0));
    }

    #[test]
    fn glide_timeline_matches_absolute_eased_interpolation() {
        let curve = motion::SCROLL_GLIDE.curve;
        let mut timeline = GlideTimeline::new();
        let (start, target) = (1000.0f32, 0.0f32);
        let mut pos = start;
        for i in 1..=60 {
            let t = i as f32 / 60.0;
            let eased = curve.eval(t);
            let frac = timeline.step(eased);
            pos -= frac * (pos - target);
            let absolute = start + eased * (target - start);
            assert!(
                (pos - absolute).abs() < 0.05,
                "frame {i}: pos {pos} != absolute {absolute}"
            );
        }
        assert_eq!(pos, target);
    }

    #[test]
    fn glide_timeline_survives_remaining_distance_reestimate() {
        let curve = motion::SCROLL_GLIDE.curve;
        let mut timeline = GlideTimeline::new();
        let mut pos = 500.0f32;
        let mut prev_frac = 0.0f32;
        for i in 1..=60 {
            let t = i as f32 / 60.0;
            let frac = timeline.step(curve.eval(t));
            if i == 30 {
                pos *= 2.0;
            }
            pos -= frac * pos;
            assert!((0.0..=1.0).contains(&frac));
            if i > 1 && i < 55 {
                assert!(frac >= prev_frac - 0.05, "frame {i}: frac regressed");
            }
            prev_frac = frac;
        }
        assert_eq!(pos, 0.0);
    }

    #[test]
    fn glide_timeline_step_clamps() {
        let mut timeline = GlideTimeline::new();
        assert_eq!(timeline.step(0.4), 0.4);
        assert_eq!(timeline.step(0.3), 0.0);
        assert_eq!(timeline.step(1.0), 1.0);
        assert_eq!(timeline.step(1.0), 1.0);
    }

    #[test]
    fn glide_first_frame_is_gentle() {
        let spec = motion::SCROLL_GLIDE;
        assert_eq!(spec.duration_ms, 500);
        let first = spec.curve.eval(16.0 / 500.0);
        assert!(first < 0.02, "first frame covered {first} of the distance");
        let mid = spec.curve.eval(0.5);
        assert!((mid - 0.5).abs() < 0.01);
    }

    #[test]
    fn preview_truncation() {
        assert_eq!(truncate_preview("short", 10), "short");
        assert_eq!(truncate_preview("  padded  ", 10), "padded");
        let long = "x".repeat(50);
        let cut = truncate_preview(&long, 10);
        assert!(cut.chars().count() <= 10);
        assert!(cut.ends_with('…'));
        let uni = "héllo wörld attaché case overflowing";
        let cut = truncate_preview(uni, 12);
        assert!(cut.ends_with('…'));
    }
}
