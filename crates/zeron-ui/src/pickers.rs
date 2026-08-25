use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, KeyDownEvent, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};

use zeron_engine::registry::HarnessDescriptor;
use zeron_proto::{
    ChatConfig, FolderListing, HarnessId, Model, ReasoningLevel, RepoRef, SandboxLevel, Space,
};
use zeron_rpc::methods;

const MAX_REF_ROWS: usize = 300;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::motion;
use crate::popover::{self, Loadable, MenuKey};
use crate::settings::composer::ComposerDefaults;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

#[derive(Default)]
pub struct HarnessCatalogChanged;

impl gpui::Global for HarnessCatalogChanged {}

pub fn bump_harness_catalog(cx: &mut App) {
    cx.default_global::<HarnessCatalogChanged>();
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DraftConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub model_options: serde_json::Map<String, serde_json::Value>,
    pub branch: Option<String>,
    pub checkout: CheckoutKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CheckoutKind {
    #[default]
    Local,
    NewWorktree,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckoutPlan {
    CurrentCheckout { branch: Option<String> },
    ReuseWorktree { path: String, branch: String },
    NewWorktree { base: Option<String> },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedRunConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub model_options: serde_json::Map<String, serde_json::Value>,
}

impl ResolvedRunConfig {
    pub fn chat_config(&self) -> Option<ChatConfig> {
        Some(ChatConfig {
            harness: self.harness?,
            model: self.model.clone(),
            reasoning: self.reasoning,
            model_options: self.model_options.clone(),
            sandbox: SandboxLevel::WorkspaceWrite,
        })
    }
}

pub fn default_model(models: &[Model]) -> Option<&Model> {
    models.first()
}

pub fn default_reasoning(ladder: &[ReasoningLevel]) -> Option<ReasoningLevel> {
    if ladder.contains(&ReasoningLevel::High) {
        return Some(ReasoningLevel::High);
    }
    if ladder.contains(&ReasoningLevel::Medium) {
        return Some(ReasoningLevel::Medium);
    }
    ladder.first().copied()
}

pub fn clamp_reasoning(
    level: Option<ReasoningLevel>,
    ladder: &[ReasoningLevel],
) -> Option<ReasoningLevel> {
    match level {
        Some(level) if ladder.contains(&level) => Some(level),
        _ => default_reasoning(ladder),
    }
}

pub fn reasoning_label(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "Minimal",
        ReasoningLevel::Low => "Low",
        ReasoningLevel::Medium => "Medium",
        ReasoningLevel::High => "High",
        ReasoningLevel::XHigh => "X-High",
        ReasoningLevel::Max => "Max",
        ReasoningLevel::Ultra => "Ultra",
        ReasoningLevel::Ultracode => "Ultracode",
        ReasoningLevel::Ultrathink => "Ultrathink",
    }
}

pub fn traits_summary(
    model: Option<&Model>,
    reasoning: Option<ReasoningLevel>,
    selections: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(level) = reasoning {
        parts.push(reasoning_label(level).to_string());
    }
    if let Some(model) = model {
        for option in &model.options {
            let choice_id = selections
                .get(&option.id)
                .and_then(|v| v.as_str())
                .filter(|id| option.choices.iter().any(|c| c.id == *id))
                .unwrap_or(&option.default_choice);
            if let Some(choice) = option.choices.iter().find(|c| c.id == choice_id) {
                parts.push(choice.label.clone());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

pub fn traits_customized(
    model: Option<&Model>,
    reasoning: Option<ReasoningLevel>,
    ladder: &[ReasoningLevel],
    selections: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    if reasoning != default_reasoning(ladder) {
        return true;
    }
    model.is_some_and(|model| {
        model.options.iter().any(|option| {
            selections
                .get(&option.id)
                .and_then(|v| v.as_str())
                .is_some_and(|id| {
                    id != option.default_choice && option.choices.iter().any(|c| c.id == id)
                })
        })
    })
}

pub fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(at) => Some(trimmed[..at].to_string()),
        None => None,
    }
}

pub fn child_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

pub fn completion_prefix_len(name: &str, query: &str) -> Option<usize> {
    let mut len = 0;
    let mut name_chars = name.chars();
    for qc in query.chars() {
        let nc = name_chars.next()?;
        if !nc.to_lowercase().eq(qc.to_lowercase()) {
            return None;
        }
        len += nc.len_utf8();
    }
    Some(len)
}

pub fn segment_target(names: &[&str], query: &str) -> Option<usize> {
    if let Some(ix) = names.iter().position(|n| *n == query) {
        return Some(ix);
    }
    if let Some(ix) = names
        .iter()
        .position(|n| completion_prefix_len(n, query) == Some(n.len()))
    {
        return Some(ix);
    }
    let mut hits = names
        .iter()
        .enumerate()
        .filter(|(_, n)| completion_prefix_len(n, query).is_some());
    let (ix, _) = hits.next()?;
    hits.next().is_none().then_some(ix)
}

pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![("/".to_string(), "/".to_string())];
    let mut acc = String::new();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(segment);
        out.push((segment.to_string(), acc.clone()));
    }
    out
}

pub fn browser_rows(listing: &FolderListing) -> Vec<&zeron_proto::FolderEntry> {
    listing.entries.iter().filter(|e| e.is_dir).collect()
}

const NO_ACTIVE_ROW: usize = usize::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ModelRail {
    Favorites,
    #[default]
    Harness,
}

#[derive(Debug, Clone)]
struct ModelRowData {
    harness: HarnessId,
    harness_name: SharedString,
    model: Model,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Branch,
    Checkout,
    HarnessModel,
    Traits,
    Space,
    Device,
}

pub struct Pickers {
    state: Entity<AppState>,
    config: DraftConfig,
    defaults: ComposerDefaults,
    data_dir: Option<PathBuf>,
    draft_owner: Option<String>,
    space_owner: Option<String>,
    open: popover::Popup<PickerKind>,
    model_rail: ModelRail,
    harnesses: Loadable<Vec<HarnessDescriptor>>,
    models: HashMap<HarnessId, Loadable<Vec<Model>>>,
    refs: Loadable<Vec<RepoRef>>,
    refs_space: Option<String>,
    active: usize,
    model_scroll: gpui::ScrollHandle,
    search: Entity<ComposerInput>,
    search_reset_muted: bool,
    focus: FocusHandle,
    boot_focus_pending: bool,
    load_task: Option<Task<()>>,
    refs_task: Option<Task<()>>,
    switching: Option<String>,
    switch_task: Option<Task<()>>,
    switch_error: Option<String>,
    mutate_task: Option<Task<()>>,
    _search_events: Subscription,
    _state_observe: Subscription,
    _catalog_observe: Subscription,
}

impl Pickers {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| ComposerInput::new("Search…", cx));
        let search_events = cx.subscribe(&search, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Edited => {
                if !std::mem::take(&mut this.search_reset_muted) {
                    if this.open_kind() == Some(PickerKind::Branch) {
                        this.active = 0;
                    }
                    if this.open_kind() == Some(PickerKind::HarnessModel) {
                        this.active = 0;
                        this.model_scroll.set_offset(gpui::Point::default());
                    }
                }
                cx.notify();
            }
            ComposerInputEvent::Submitted => this.on_search_submit(cx),
            ComposerInputEvent::PastedImages(_)
            | ComposerInputEvent::PastedPaths(_)
            | ComposerInputEvent::CursorMoved
            | ComposerInputEvent::ViewportChanged
            | ComposerInputEvent::MentionNavigate(_)
            | ComposerInputEvent::MentionAccept
            | ComposerInputEvent::MentionDismiss => {}
        });
        let state_observe = cx.observe(&state, |this: &mut Self, state, cx| {
            let selected = state.read(cx).selected_chat.clone();
            if selected != this.draft_owner {
                this.draft_owner = selected;
                this.config.harness = None;
                this.config.model = None;
                this.config.reasoning = None;
                this.config.model_options.clear();
                this.switch_error = None;
            }
            let space = state.read(cx).selected_space.clone();
            if space != this.space_owner {
                this.space_owner = space;
                this.config.branch = None;
                this.config.checkout = CheckoutKind::default();
                this.refs = Loadable::Idle;
                this.refs_space = None;
                this.harnesses = Loadable::Idle;
                this.models.clear();
            }
            cx.notify();
        });
        let catalog_observe = cx.observe_global::<HarnessCatalogChanged>(|this: &mut Self, cx| {
            this.ensure_harnesses(true, cx);
            cx.notify();
        });
        let boot_open = match std::env::var("ZERON_OPEN_PICKER").ok().as_deref() {
            Some("model") => Some(PickerKind::HarnessModel),
            Some("traits") => Some(PickerKind::HarnessModel),
            Some("branch") => Some(PickerKind::Branch),
            Some("checkout") => Some(PickerKind::Checkout),
            Some("project") => Some(PickerKind::Space),
            Some("device") => Some(PickerKind::Device),
            _ => None,
        };
        let mut open = popover::Popup::default();
        if let Some(kind) = boot_open {
            open.open(kind);
        }
        let data_dir = state.read(cx).data_dir.clone();
        let defaults = data_dir
            .as_deref()
            .map(ComposerDefaults::load)
            .unwrap_or_default();
        {
            let device = defaults.device.clone();
            let project = defaults.project.clone();
            state.update(cx, |s, _| {
                if s.selected_device.is_none() {
                    s.selected_device = device;
                }
                if s.selected_space.is_none() {
                    s.selected_space = project;
                }
            });
        }
        let draft_owner = state.read(cx).selected_chat.clone();
        let space_owner = state.read(cx).selected_space.clone();
        Self {
            state,
            space_owner,
            config: DraftConfig::default(),
            defaults,
            data_dir,
            draft_owner,
            open,
            model_rail: ModelRail::default(),
            harnesses: Loadable::Idle,
            models: HashMap::new(),
            refs: Loadable::Idle,
            refs_space: None,
            active: 0,
            model_scroll: gpui::ScrollHandle::new(),
            search,
            search_reset_muted: false,
            focus: cx.focus_handle(),
            boot_focus_pending: boot_open.is_some(),
            load_task: None,
            refs_task: None,
            switching: None,
            switch_task: None,
            switch_error: None,
            mutate_task: None,
            _search_events: search_events,
            _state_observe: state_observe,
            _catalog_observe: catalog_observe,
        }
    }

    fn save_defaults(&self) {
        if let Some(dir) = self.data_dir.as_deref()
            && let Err(err) = self.defaults.save(dir)
        {
            tracing::warn!(error = %err, "composer-defaults save failed");
        }
    }

    pub fn draft(&self) -> &DraftConfig {
        &self.config
    }

    fn harness_locked(&self, cx: &App) -> bool {
        self.state.read(cx).selected_chat.is_some()
    }

    fn engine(&self, cx: &App) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    fn space_target(&self, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        let device = state.selected_space_row()?.device_id.clone();
        (state.local_device_id.as_deref() != Some(device.as_str())).then_some(device)
    }

    fn effective_harness(&self, cx: &App) -> Option<HarnessId> {
        if let Some(harness) = self.config.harness {
            return Some(harness);
        }
        if let Some(config) = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            return Some(config.harness);
        }
        if let Some(harness) = self.defaults.harness {
            let offered = match self.harnesses.ready() {
                Some(list) => offered_harnesses(list).iter().any(|d| d.id == harness),
                None => true,
            };
            if offered {
                return Some(harness);
            }
        }
        self.harnesses
            .ready()
            .and_then(|list| offered_harnesses(list).first().map(|d| d.id))
    }

    fn effective_model_id<'a>(&'a self, cx: &'a App) -> Option<&'a str> {
        if let Some(id) = self.config.model.as_deref() {
            return Some(id);
        }
        if let Some(chat) = self.state.read(cx).selected_chat_row() {
            return chat.config.as_ref().and_then(|c| c.model.as_deref());
        }
        let harness = self.effective_harness(cx)?;
        self.defaults.model_for(harness).map(|m| m.id.as_str())
    }

    fn effective_reasoning(&self, cx: &App) -> Option<ReasoningLevel> {
        let explicit =
            self.config
                .reasoning
                .or_else(|| match self.state.read(cx).selected_chat_row() {
                    Some(chat) => chat.config.as_ref().and_then(|c| c.reasoning),
                    None => self.defaults.reasoning,
                });
        if self.selected_model(cx).is_none() {
            return explicit;
        }
        clamp_reasoning(explicit, &self.trait_ladder(cx))
    }

    fn selected_model<'a>(&'a self, cx: &'a App) -> Option<&'a Model> {
        let harness = self.effective_harness(cx)?;
        let models = self.models.get(&harness)?.ready()?;
        match self.effective_model_id(cx) {
            Some(id) => models
                .iter()
                .find(|m| m.id == id)
                .or_else(|| default_model(models)),
            None => default_model(models),
        }
    }

    fn explicit_options(&self, cx: &App) -> serde_json::Map<String, serde_json::Value> {
        match self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            Some(config) => config.model_options.clone(),
            None => self.config.model_options.clone(),
        }
    }

    pub fn resolved_steering_mode(&self, cx: &App) -> Option<zeron_proto::SteeringMode> {
        let harness = self.effective_harness(cx)?;
        self.harnesses
            .ready()
            .and_then(|list| list.iter().find(|d| d.id == harness))
            .map(|d| d.steering_mode)
    }

    pub fn resolved(&self, cx: &App) -> ResolvedRunConfig {
        ResolvedRunConfig {
            harness: self.effective_harness(cx),
            model: self
                .selected_model(cx)
                .map(|m| m.id.clone())
                .or_else(|| self.effective_model_id(cx).map(str::to_string)),
            reasoning: self.effective_reasoning(cx),
            model_options: self.explicit_options(cx),
        }
    }

    fn open_kind(&self) -> Option<PickerKind> {
        self.open.as_open().copied()
    }

    fn mounted_kind(&self) -> Option<PickerKind> {
        self.open.get().copied()
    }

    fn animate_close(&mut self, cx: &mut Context<Self>) {
        if self.open.begin_close() {
            popover::reap_popup(cx, |pickers: &mut Self| &mut pickers.open);
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.animate_close(cx);
        cx.notify();
    }

    pub fn open_model_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_kind() != Some(PickerKind::HarnessModel) {
            self.toggle(PickerKind::HarnessModel, window, cx);
        }
    }

    fn toggle(&mut self, kind: PickerKind, window: &mut Window, cx: &mut Context<Self>) {
        let pressed_open = self.open.take_press_was_open();
        if self.open_kind() == Some(kind) || pressed_open {
            self.animate_close(cx);
            cx.notify();
            return;
        }
        self.open.open(kind);
        self.search_reset_muted = !self.search.read(cx).text().is_empty();
        self.search.update(cx, |input, cx| {
            input.set_placeholder("Search…", cx);
            if !input.text().is_empty() {
                input.set_text("", cx);
            }
        });
        if kind == PickerKind::HarnessModel {
            self.model_rail = if !self.harness_locked(cx) && !self.defaults.favorites.is_empty() {
                ModelRail::Favorites
            } else {
                ModelRail::Harness
            };
        }
        self.active = match kind {
            PickerKind::Checkout => match self.config.checkout {
                CheckoutKind::Local => 0,
                CheckoutKind::NewWorktree => 1,
            },
            PickerKind::Branch => self.selected_ref_index(cx),
            PickerKind::HarnessModel | PickerKind::Traits => self.selected_model_index(cx),
            PickerKind::Space => self.selected_space_index(cx),
            PickerKind::Device => self.selected_device_index(cx),
        };
        if kind == PickerKind::HarnessModel {
            self.model_scroll.set_offset(gpui::Point::default());
            self.model_scroll.scroll_to_item(self.active);
        }
        match kind {
            PickerKind::Branch => {
                self.switch_error = None;
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search refs…", cx);
                });
                window.focus(&handle, cx);
            }
            PickerKind::Space => {
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search projects…", cx);
                });
                window.focus(&handle, cx);
            }
            PickerKind::Device => {
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search devices…", cx);
                });
                window.focus(&handle, cx);
            }
            PickerKind::HarnessModel => {
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search models…", cx);
                });
                window.focus(&handle, cx);
            }
            _ => window.focus(&self.focus, cx),
        }
        match kind {
            PickerKind::Branch | PickerKind::Checkout => self.ensure_refs(true, cx),
            PickerKind::HarnessModel | PickerKind::Traits => {
                self.ensure_harnesses(true, cx);
                self.prefetch_models(cx);
            }
            PickerKind::Space | PickerKind::Device => {}
        }
        cx.notify();
    }

    fn ensure_harnesses(&mut self, force: bool, cx: &mut Context<Self>) {
        let reload = match self.harnesses {
            Loadable::Idle => true,
            Loadable::Loading => false,
            Loadable::Ready(_) | Loadable::Error(_) => force,
        };
        if !reload {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let target = self.space_target(cx);
        if !matches!(self.harnesses, Loadable::Ready(_)) {
            self.harnesses = Loadable::Loading;
        }
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            if let Some(target) = &target {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::LIST_HARNESSES, serde_json::Value::Object(params))
                .await;
            this.update(cx, |pickers, cx| {
                pickers.harnesses = match result {
                    Ok(value) => match serde_json::from_value::<Vec<HarnessDescriptor>>(value) {
                        Ok(list) => Loadable::Ready(list),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                pickers.prefetch_models(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn prefetch_models(&mut self, cx: &mut Context<Self>) {
        let mut targets: Vec<HarnessId> = match self.harnesses.ready() {
            Some(list) => offered_harnesses(list).iter().map(|d| d.id).collect(),
            None => Vec::new(),
        };
        if let Some(effective) = self.effective_harness(cx)
            && !targets.contains(&effective)
        {
            targets.push(effective);
        }
        for harness in targets {
            self.ensure_models(harness, cx);
        }
    }

    fn ensure_models(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if self
            .models
            .get(&harness)
            .is_some_and(|slot| !matches!(slot, Loadable::Idle))
        {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let target = self.space_target(cx);
        self.models.insert(harness, Loadable::Loading);
        cx.spawn(async move |this, cx| {
            let mut params = serde_json::json!({ "harness": harness });
            if let (Some(target), Some(object)) = (&target, params.as_object_mut()) {
                object.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            let result = engine.client().call(methods::LIST_MODELS, params).await;
            this.update(cx, |pickers, cx| {
                let loaded = match result {
                    Ok(value) => match serde_json::from_value::<Vec<Model>>(value) {
                        Ok(models) => Loadable::Ready(normalize_model_rows(harness, models)),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                if let Loadable::Ready(models) = &loaded {
                    let fresh = pickers
                        .defaults
                        .remember_labels(models.iter().map(|m| (m.id.as_str(), m.label.as_str())));
                    if fresh {
                        pickers.save_defaults();
                    }
                }
                pickers.models.insert(harness, loaded);
                if pickers.open_kind() == Some(PickerKind::HarnessModel)
                    && pickers.effective_harness(cx) == Some(harness)
                {
                    pickers.active = pickers.selected_model_index(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn ensure_refs(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(space) = self.state.read(cx).selected_space_row().cloned() else {
            return;
        };
        if !space.git_detected {
            return;
        }
        let fresh = self.refs_space.as_deref() == Some(space.id.as_str());
        if fresh && matches!(self.refs, Loadable::Loading) {
            return;
        }
        if !force && fresh && !matches!(self.refs, Loadable::Idle) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        if !(force && fresh && matches!(self.refs, Loadable::Ready(_))) {
            self.refs = Loadable::Loading;
        }
        self.refs_space = Some(space.id.clone());
        self.refs_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(space.path.clone()),
            );
            if local.as_deref() != Some(space.device_id.as_str()) {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(space.device_id.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::LIST_REFS, serde_json::Value::Object(params))
                .await;
            this.update(cx, |pickers, cx| {
                pickers.refs = match result {
                    Ok(value) => match serde_json::from_value::<Vec<RepoRef>>(value) {
                        Ok(refs) => Loadable::Ready(refs),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                if pickers.open_kind() == Some(PickerKind::Branch)
                    && pickers.search.read(cx).text().is_empty()
                {
                    pickers.active = pickers.selected_ref_index(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn pick_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        if self.state.read(cx).selected_chat_row().is_some() {
            return;
        }
        if row.worktree_path.is_some() {
            self.config.branch = Some(row.name.clone());
            self.config.checkout = CheckoutKind::Local;
        } else if self.config.checkout == CheckoutKind::NewWorktree || row.current {
            self.config.branch = Some(row.name.clone());
        } else {
            self.switch_draft_ref(row, cx);
            return;
        }
        self.animate_close(cx);
        cx.notify();
    }

    fn switch_draft_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        if self.switching.is_some() {
            return;
        }
        let Some(space) = self.state.read(cx).selected_space_row().cloned() else {
            return;
        };
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        self.switch_error = None;
        self.switching = Some(row.name.clone());
        let ref_name = row.name.clone();
        self.switch_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(space.path.clone()),
            );
            params.insert(
                "refName".into(),
                serde_json::Value::String(ref_name.clone()),
            );
            if local.as_deref() != Some(space.device_id.as_str()) {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(space.device_id.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::SWITCH_REF, serde_json::Value::Object(params))
                .await;
            this.update(cx, |pickers, cx| {
                pickers.switching = None;
                match result {
                    Ok(_) => {
                        pickers.config.branch = Some(ref_name);
                        pickers.animate_close(cx);
                        pickers.ensure_refs(true, cx);
                    }
                    Err(err) => pickers.switch_error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn pick_checkout(&mut self, kind: CheckoutKind, cx: &mut Context<Self>) {
        if kind == CheckoutKind::Local
            && self.config.checkout == CheckoutKind::NewWorktree
            && self.selected_ref_worktree().is_none()
            && self.selected_ref().is_some_and(|r| !r.current)
        {
            self.config.branch = None;
        }
        self.config.checkout = kind;
        self.animate_close(cx);
        cx.notify();
    }

    fn pick_harness(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if self.harness_locked(cx) {
            return;
        }
        if self.config.harness != Some(harness) {
            self.config.model = None;
            self.config.reasoning = None;
            self.config.model_options.clear();
        }
        self.config.harness = Some(harness);
        self.defaults.harness = Some(harness);
        self.save_defaults();
        self.model_scroll.set_offset(gpui::Point::default());
        self.ensure_models(harness, cx);
        self.active = self.selected_model_index(cx);
        cx.notify();
    }

    fn pick_model(&mut self, model_id: String, cx: &mut Context<Self>) {
        self.animate_close(cx);
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| config.model = Some(model_id));
        } else {
            self.config.model = Some(model_id.clone());
            if let Some(harness) = self.effective_harness(cx) {
                let label = self
                    .models
                    .get(&harness)
                    .and_then(|l| l.ready())
                    .and_then(|models| models.iter().find(|m| m.id == model_id))
                    .map(|m| m.label.clone())
                    .unwrap_or_else(|| model_id.clone());
                self.defaults.remember_model(harness, model_id, label);
                self.save_defaults();
            }
        }
        cx.notify();
    }

    fn pick_reasoning(&mut self, level: ReasoningLevel, cx: &mut Context<Self>) {
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| config.reasoning = Some(level));
        } else {
            self.config.reasoning = Some(level);
            self.defaults.reasoning = Some(level);
            self.save_defaults();
        }
        cx.notify();
    }

    fn pick_option(
        &mut self,
        option_id: String,
        choice_id: String,
        default: bool,
        cx: &mut Context<Self>,
    ) {
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| {
                if default {
                    config.model_options.remove(&option_id);
                } else {
                    config
                        .model_options
                        .insert(option_id, serde_json::Value::String(choice_id));
                }
            });
        } else if default {
            self.config.model_options.remove(&option_id);
        } else {
            self.config
                .model_options
                .insert(option_id, serde_json::Value::String(choice_id));
        }
        cx.notify();
    }

    fn update_chat_config(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut ChatConfig)) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let resolved = self.resolved(cx);
        let Some(mut config) = resolved.chat_config() else {
            return;
        };
        if let Some(existing) = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            config.sandbox = existing.sandbox;
        }
        change(&mut config);
        if let Some(models) = self.models.get(&config.harness).and_then(|l| l.ready()) {
            let mut ladder = config
                .model
                .as_deref()
                .and_then(|id| models.iter().find(|m| m.id == id))
                .map(|m| m.reasoning_levels.clone())
                .unwrap_or_default();
            if ladder.is_empty()
                && let Some(descriptor) = self
                    .harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == config.harness))
            {
                ladder = descriptor.reasoning_levels.clone();
            }
            if !ladder.is_empty() {
                config.reasoning = clamp_reasoning(config.reasoning, &ladder);
            }
        }
        self.state.update(cx, |state, cx| {
            state.apply_chat_config(&chat_id, config.clone());
            cx.notify();
        });
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |_, _| {
            let params = serde_json::json!({
                "op": "setChatConfig",
                "chatId": chat_id,
                "config": config,
            });
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                tracing::warn!(error = %err, "setChatConfig mutate failed");
            }
        }));
    }

    fn trait_ladder(&self, cx: &App) -> Vec<ReasoningLevel> {
        let Some(model) = self.selected_model(cx) else {
            return Vec::new();
        };
        if !model.reasoning_levels.is_empty() {
            return model.reasoning_levels.clone();
        }
        self.effective_harness(cx)
            .and_then(|h| {
                self.harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == h))
                    .map(|d| d.reasoning_levels.clone())
            })
            .unwrap_or_default()
    }

    fn rail_descriptors(&self, cx: &App) -> Vec<HarnessDescriptor> {
        let Some(list) = self.harnesses.ready() else {
            return Vec::new();
        };
        let mut descriptors = offered_harnesses(list);
        if let Some(effective) = self.effective_harness(cx)
            && !descriptors.iter().any(|d| d.id == effective)
            && let Some(descriptor) = list.iter().find(|d| d.id == effective)
        {
            descriptors.insert(0, descriptor.clone());
        }
        descriptors
    }

    fn visible_model_rows(&self, cx: &App) -> Vec<ModelRowData> {
        let effective = self.effective_harness(cx);
        let mut descriptors = self.rail_descriptors(cx);
        if self.harness_locked(cx) {
            descriptors.retain(|d| Some(d.id) == effective);
        }
        let row = |descriptor: &HarnessDescriptor, model: &Model| ModelRowData {
            harness: descriptor.id,
            harness_name: SharedString::from(descriptor.name.clone()),
            model: model.clone(),
        };
        let query = self.search.read(cx).text().trim().to_string();
        if !query.is_empty() {
            let mut ranked: Vec<(usize, usize, usize, ModelRowData)> = Vec::new();
            let mut input_ix = 0usize;
            for descriptor in &descriptors {
                let Some(models) = self.models.get(&descriptor.id).and_then(|l| l.ready()) else {
                    continue;
                };
                for model in models {
                    let by_label = popover::match_rank(&query, &model.label);
                    let by_harness = popover::match_rank(
                        &query,
                        &format!("{} {}", descriptor.name, model.label),
                    )
                    .map(|rank| rank + 2);
                    if let Some(rank) = by_label.into_iter().chain(by_harness).min() {
                        let starred = !self.defaults.is_favorite(descriptor.id, &model.id);
                        ranked.push((rank, starred as usize, input_ix, row(descriptor, model)));
                    }
                    input_ix += 1;
                }
            }
            ranked.sort_by_key(|(rank, unstarred, ix, _)| (*rank, *unstarred, *ix));
            return ranked.into_iter().map(|(_, _, _, row)| row).collect();
        }
        match self.model_rail {
            ModelRail::Favorites => {
                let mut rows = Vec::new();
                for descriptor in &descriptors {
                    let Some(models) = self.models.get(&descriptor.id).and_then(|l| l.ready())
                    else {
                        continue;
                    };
                    for model in models {
                        if self.defaults.is_favorite(descriptor.id, &model.id) {
                            rows.push(row(descriptor, model));
                        }
                    }
                }
                rows
            }
            ModelRail::Harness => {
                let Some(descriptor) = descriptors.iter().find(|d| Some(d.id) == effective) else {
                    return Vec::new();
                };
                let Some(models) = self.models.get(&descriptor.id).and_then(|l| l.ready()) else {
                    return Vec::new();
                };
                let (starred, rest): (Vec<&Model>, Vec<&Model>) = models
                    .iter()
                    .partition(|m| self.defaults.is_favorite(descriptor.id, &m.id));
                starred
                    .into_iter()
                    .chain(rest)
                    .map(|model| row(descriptor, model))
                    .collect()
            }
        }
    }

    fn selected_model_index(&self, cx: &App) -> usize {
        let selected = self.selected_model(cx).map(|m| m.id.clone());
        let effective = self.effective_harness(cx);
        self.visible_model_rows(cx)
            .iter()
            .position(|row| {
                Some(row.harness) == effective && selected.as_deref() == Some(row.model.id.as_str())
            })
            .unwrap_or(0)
    }

    fn model_rows_len(&self, cx: &App) -> usize {
        self.visible_model_rows(cx).len()
    }

    fn activate_model_row(&mut self, cx: &mut Context<Self>) {
        self.activate_model_index(self.active, cx);
    }

    fn activate_model_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.visible_model_rows(cx).into_iter().nth(ix) else {
            return;
        };
        if self.effective_harness(cx) != Some(row.harness) {
            if self.harness_locked(cx) {
                return;
            }
            self.pick_harness(row.harness, cx);
        }
        self.pick_model(row.model.id, cx);
    }

    fn toggle_model_favorite(&mut self, harness: HarnessId, model: &str, cx: &mut Context<Self>) {
        self.defaults.toggle_favorite(harness, model);
        self.save_defaults();
        self.active = self.selected_model_index(cx);
        cx.notify();
    }

    fn filtered_ref_rows(&self, cx: &App) -> Vec<RepoRef> {
        let Some(refs) = self.refs.ready() else {
            return Vec::new();
        };
        let names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
        let query = self.search.read(cx).text().to_string();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| refs[ix].clone())
            .collect()
    }

    fn selected_ref_index(&self, cx: &App) -> usize {
        let rows = self.filtered_ref_rows(cx);
        let selected = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.branch.clone())
            .or_else(|| self.config.branch.clone());
        let index = match selected {
            Some(name) => rows.iter().position(|r| r.name == name).unwrap_or(0),
            None => rows.iter().position(|r| r.current).unwrap_or(0),
        };
        index.min(MAX_REF_ROWS.saturating_sub(1))
    }

    fn selected_ref(&self) -> Option<&RepoRef> {
        let refs = self.refs.ready()?;
        match self.config.branch.as_deref() {
            Some(name) => refs.iter().find(|r| r.name == name),
            None => refs.iter().find(|r| r.current),
        }
    }

    fn effective_ref_name(&self) -> Option<String> {
        self.config
            .branch
            .clone()
            .or_else(|| self.selected_ref().map(|r| r.name.clone()))
    }

    fn selected_ref_worktree(&self) -> Option<String> {
        self.selected_ref().and_then(|r| r.worktree_path.clone())
    }

    pub fn checkout_plan(&self) -> CheckoutPlan {
        match self.config.checkout {
            CheckoutKind::NewWorktree => CheckoutPlan::NewWorktree {
                base: self.effective_ref_name(),
            },
            CheckoutKind::Local => match self.selected_ref_worktree() {
                Some(path) => CheckoutPlan::ReuseWorktree {
                    path,
                    branch: self.effective_ref_name().unwrap_or_default(),
                },
                None => CheckoutPlan::CurrentCheckout {
                    branch: self.effective_ref_name(),
                },
            },
        }
    }

    fn checkout_label(&self) -> &'static str {
        match self.config.checkout {
            CheckoutKind::NewWorktree => "New worktree",
            CheckoutKind::Local => {
                if self.selected_ref_worktree().is_some() {
                    "Current worktree"
                } else {
                    "Current checkout"
                }
            }
        }
    }

    fn ref_label(&self) -> SharedString {
        match (self.config.checkout, self.effective_ref_name()) {
            (_, None) => SharedString::from("Select ref"),
            (CheckoutKind::NewWorktree, Some(name)) => SharedString::from(format!("From {name}")),
            (CheckoutKind::Local, Some(name)) => SharedString::from(name),
        }
    }

    fn scoped_space_rows(&self, cx: &App) -> Vec<Space> {
        let state = self.state.read(cx);
        let device = state.effective_device_id();
        state
            .spaces_sorted()
            .into_iter()
            .filter(|s| match device.as_deref() {
                Some(d) => s.device_id == d,
                None => true,
            })
            .cloned()
            .collect()
    }

    fn filtered_space_rows(&self, cx: &App) -> Vec<Space> {
        let query = self.search.read(cx).text().to_string();
        let spaces = self.scoped_space_rows(cx);
        let names: Vec<String> = spaces
            .iter()
            .map(|s| s.display_name().to_string())
            .collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| spaces[ix].clone())
            .collect()
    }

    fn selected_space_index(&self, cx: &App) -> usize {
        let selected = self
            .state
            .read(cx)
            .selected_space_row()
            .map(|s| s.id.clone());
        selected
            .as_deref()
            .and_then(|id| self.scoped_space_rows(cx).iter().position(|s| s.id == id))
            .unwrap_or(NO_ACTIVE_ROW)
    }

    fn pick_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.state
            .update(cx, |s, cx| s.select_space(Some(space_id), cx));
        self.remember_target(cx);
        self.close(cx);
    }

    fn pick_device(&mut self, device_id: String, cx: &mut Context<Self>) {
        self.state
            .update(cx, |s, cx| s.select_device(device_id, cx));
        self.remember_target(cx);
        self.close(cx);
    }

    fn remember_target(&mut self, cx: &App) {
        {
            let state = self.state.read(cx);
            self.defaults.device = state
                .selected_device
                .clone()
                .or_else(|| state.local_device_id.clone());
            self.defaults.project = state.selected_space.clone();
            self.defaults.no_project = state.no_project;
        }
        if let Some(dir) = &self.data_dir {
            if let Err(err) = self.defaults.save(dir) {
                tracing::warn!(error = %err, "composer-defaults save failed");
            }
        }
    }

    fn device_rows(&self, cx: &App) -> Vec<zeron_proto::Device> {
        let state = self.state.read(cx);
        let local = state.local_device_id.clone();
        let mut devices: Vec<zeron_proto::Device> = state.devices.clone();
        devices.sort_by_key(|d| {
            (
                local.as_deref() != Some(d.id.as_str()),
                d.name.to_lowercase(),
                d.id.clone(),
            )
        });
        devices
    }

    fn filtered_device_rows(&self, cx: &App) -> Vec<zeron_proto::Device> {
        let query = self.search.read(cx).text().to_string();
        let rows = self.device_rows(cx);
        let names: Vec<String> = rows.iter().map(|d| d.name.clone()).collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| rows[ix].clone())
            .collect()
    }

    fn selected_device_index(&self, cx: &App) -> usize {
        let effective = self.state.read(cx).effective_device_id();
        self.device_rows(cx)
            .iter()
            .position(|d| Some(d.id.as_str()) == effective.as_deref())
            .unwrap_or(0)
    }

    fn render_device_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = chrono::Utc::now();
        let rows = self.filtered_device_rows(cx);
        let (effective, local, online): (Option<String>, Option<String>, Vec<bool>) = {
            let state = self.state.read(cx);
            (
                state.effective_device_id(),
                state.local_device_id.clone(),
                rows.iter()
                    .map(|d| state.device_online(&d.id, now))
                    .collect(),
            )
        };
        let active = self.active;
        let body: AnyElement =
            if rows.is_empty() {
                div()
                    .p(px(Theme::SPACE_SM))
                    .text_size(px(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("No devices match."))
                    .into_any_element()
            } else {
                div()
                    .id("device-list")
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .max_h(px(224.0))
                    .overflow_y_scroll()
                    .children(rows.into_iter().zip(online).enumerate().map(
                        |(ix, (device, online))| {
                            let is_local = local.as_deref() == Some(device.id.as_str());
                            let label: SharedString = device.name.clone().into();
                            let is_selected = effective.as_deref() == Some(device.id.as_str());
                            let pick_id = device.id.clone();
                            popover::menu_row_nav(
                                &theme,
                                is_selected,
                                ix == active,
                                format!("device-row-{ix}"),
                            )
                            .id(("device-row", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pick_device(pick_id.clone(), cx);
                            }))
                            .child(div().flex_1().min_w_0().truncate().child(label))
                            .when(is_local, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(10.0))
                                        .text_color(theme.text_muted.opacity(0.45))
                                        .child(SharedString::from("You")),
                                )
                            })
                            .when(!online, |el| {
                                el.child(
                                    crate::icons::icon(crate::icons::WIFI_OFF)
                                        .size(px(12.0))
                                        .flex_none()
                                        .text_color(theme.warning.opacity(0.8)),
                                )
                            })
                        },
                    ))
                    .into_any_element()
            };
        div()
            .flex()
            .flex_col()
            .child(self.search_box(&theme))
            .child(body)
            .into_any_element()
    }

    fn render_space_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let rows = self.filtered_space_rows(cx);
        let selected = self
            .state
            .read(cx)
            .selected_space_row()
            .map(|s| s.id.clone());
        let active = self.active;
        let body: AnyElement = if rows.is_empty() {
            let empty: &str = if self.search.read(cx).text().is_empty() {
                "No projects on this device."
            } else {
                "No projects match."
            };
            div()
                .p(px(Theme::SPACE_SM))
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from(empty.to_string()))
                .into_any_element()
        } else {
            div()
                .id("space-list")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .max_h(px(224.0))
                .overflow_y_scroll()
                .children(rows.into_iter().enumerate().map(|(ix, space)| {
                    let label: SharedString = space.display_name().to_string().into();
                    let is_selected = selected.as_deref() == Some(space.id.as_str());
                    let pick_id = space.id.clone();
                    popover::menu_row_nav(
                        &theme,
                        is_selected,
                        ix == active,
                        format!("space-row-{ix}"),
                    )
                    .id(("space-row", ix))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pick_space(pick_id.clone(), cx);
                    }))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                }))
                .into_any_element()
        };
        let new_project = popover::menu_row_nav(&theme, false, false, "project-new".to_string())
            .id("project-new")
            .on_click(cx.listener(|this, _, window, cx| {
                this.close(cx);
                window.dispatch_action(Box::new(crate::shell::AddSpacePalette), cx);
            }))
            .child(
                crate::icons::icon(crate::icons::PLUS)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from("New project…")),
            );
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(self.search_box(&theme))
            .child(body)
            .child(
                div()
                    .my(px(2.0))
                    .mx(px(-4.0))
                    .h(px(1.0))
                    .flex_none()
                    .bg(theme.border.opacity(0.6)),
            )
            .child(new_project)
            .into_any_element()
    }

    fn on_search_submit(&mut self, cx: &mut Context<Self>) {
        if self.open_kind() == Some(PickerKind::Branch)
            && let Some(row) = self.filtered_ref_rows(cx).into_iter().nth(self.active)
        {
            self.pick_ref(row, cx);
        }
        if self.open_kind() == Some(PickerKind::Space)
            && let Some(space) = self.filtered_space_rows(cx).into_iter().nth(self.active)
        {
            self.pick_space(space.id, cx);
        }
        if self.open_kind() == Some(PickerKind::Device)
            && let Some(device) = self.filtered_device_rows(cx).into_iter().nth(self.active)
        {
            self.pick_device(device.id, cx);
        }
        if self.open_kind() == Some(PickerKind::HarnessModel) {
            self.activate_model_row(cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
        if !self.open.is_open() {
            return;
        }
        if self.open_kind() == Some(PickerKind::HarnessModel)
            && event.keystroke.modifiers.platform
            && let Ok(n) = event.keystroke.key.parse::<usize>()
            && (1..=9).contains(&n)
        {
            self.activate_model_index(n - 1, cx);
            cx.notify();
            return;
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        let search_focused = self.search.read(cx).focus_handle(cx).is_focused(window);
        match key {
            MenuKey::Escape => {
                self.animate_close(cx);
                cx.notify();
            }
            MenuKey::Up | MenuKey::Down => {
                let delta = if key == MenuKey::Up { -1 } else { 1 };
                let count = match self.open_kind() {
                    Some(PickerKind::Branch) => self.filtered_ref_rows(cx).len().min(MAX_REF_ROWS),
                    Some(PickerKind::Checkout) => 2,
                    Some(PickerKind::HarnessModel) => self.model_rows_len(cx),
                    Some(PickerKind::Traits) => 0,
                    Some(PickerKind::Space) => self.filtered_space_rows(cx).len(),
                    Some(PickerKind::Device) => self.filtered_device_rows(cx).len(),
                    None => 0,
                };
                let current = (self.active != NO_ACTIVE_ROW).then_some(self.active);
                self.active = popover::menu_step(current, count, delta).unwrap_or(0);
                if self.open_kind() == Some(PickerKind::HarnessModel)
                    && self.active < self.model_rows_len(cx)
                {
                    self.model_scroll.scroll_to_item(self.active);
                }
                cx.notify();
            }
            MenuKey::Enter if !search_focused => {
                if self.open_kind() == Some(PickerKind::HarnessModel) {
                    self.activate_model_row(cx);
                } else if self.open_kind() == Some(PickerKind::Checkout) {
                    let kind = if self.active == 0 {
                        CheckoutKind::Local
                    } else {
                        CheckoutKind::NewWorktree
                    };
                    self.pick_checkout(kind, cx);
                } else {
                    self.on_search_submit(cx);
                }
            }
            _ => {}
        }
    }

    fn trigger_chip(
        &self,
        kind: PickerKind,
        label: SharedString,
        set: bool,
        chip_icon: Option<(&'static str, Option<gpui::Hsla>)>,
        suffix: Option<(SharedString, Option<gpui::Hsla>)>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id: &'static str = match kind {
            PickerKind::Branch => "picker-branch",
            PickerKind::Checkout => "picker-checkout",
            PickerKind::HarnessModel => "picker-model",
            PickerKind::Traits => "picker-traits",
            PickerKind::Space => "picker-space",
            PickerKind::Device => "picker-device",
        };
        let open = self.open_kind() == Some(kind);
        div()
            .id(id)
            .h(px(32.0))
            .max_w(px(208.0))
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .rounded(px(8.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(motion::hover_blend(
                id,
                if set {
                    theme.text.opacity(0.9)
                } else {
                    theme.text_muted
                },
                theme.text,
            ))
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.open.note_trigger_press_matching(|open| *open == kind)
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .when_some(chip_icon, |el, (path, tint)| {
                el.child(
                    crate::icons::icon(path)
                        .size(px(16.0))
                        .text_color(tint.unwrap_or(theme.text_muted)),
                )
            })
            .child(div().min_w_0().truncate().child(label))
            .when_some(suffix, |el, (suffix, tint)| {
                el.child(
                    div()
                        .flex_none()
                        .text_color(tint.unwrap_or(theme.text_muted.opacity(0.7)))
                        .child(suffix),
                )
            })
    }

    fn footer_chip(
        &self,
        kind: PickerKind,
        id: &'static str,
        icon_path: &'static str,
        label: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let open = self.open_kind() == Some(kind);
        div()
            .id(id)
            .h(px(20.0))
            .max_w(px(280.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(motion::hover_blend(
                id,
                theme.text_muted.opacity(0.7),
                theme.text.opacity(0.8),
            ))
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.open.note_trigger_press_matching(|open| *open == kind)
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(div().min_w_0().truncate().child(label))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.5)),
            )
    }

    fn footer_label(icon_path: &'static str, label: SharedString, theme: &Theme) -> gpui::Div {
        div()
            .h(px(20.0))
            .max_w(px(160.0))
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.6))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.6)),
            )
            .child(div().min_w_0().truncate().child(label))
    }

    pub fn render_target_selectors(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            Some(PickerKind::Space) => {
                let content = self.render_space_popover(cx);
                Some((PickerKind::Space, self.popover_frame(280.0, content, cx)))
            }
            Some(PickerKind::Device) => {
                let content = self.render_device_popover(cx);
                Some((PickerKind::Device, self.popover_frame(224.0, content, cx)))
            }
            _ => None,
        };
        let (device_label, project_label, offline) = {
            let state = self.state.read(cx);
            let device_id = state.effective_device_id();
            let device_label: SharedString = device_id
                .as_deref()
                .and_then(|id| state.device_name(id))
                .map(str::to_string)
                .unwrap_or_else(|| "This device".to_string())
                .into();
            let offline = device_id
                .as_deref()
                .is_some_and(|id| !state.device_online(id, chrono::Utc::now()));
            let project_label: SharedString = state
                .selected_space_row()
                .map(|s| s.display_name().to_string())
                .unwrap_or_else(|| "No project".to_string())
                .into();
            (device_label, project_label, offline)
        };
        let device_chip = self
            .footer_chip(
                PickerKind::Device,
                "picker-device",
                crate::icons::MONITOR,
                device_label,
                &theme,
                cx,
            )
            .when(offline, |el| el.text_color(theme.warning.opacity(0.8)));
        let project_chip = self.footer_chip(
            PickerKind::Space,
            "picker-project",
            crate::icons::FOLDER,
            project_label,
            &theme,
            cx,
        );
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .child(attach_overlay_below(
                device_chip,
                &mut overlay,
                PickerKind::Device,
                "device-popover",
                closing,
            ))
            .child(attach_overlay_below(
                project_chip,
                &mut overlay,
                PickerKind::Space,
                "project-popover",
                closing,
            ))
            .into_any_element()
    }

    pub fn render_footer(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let (space, session) = {
            let state = self.state.read(cx);
            let space = state.selected_space_row().cloned();
            let session = state
                .selected_chat
                .as_ref()
                .and_then(|_| state.selected_chat_row().cloned());
            (space, session)
        };
        let row = || {
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .px(px(10.0))
                .mb(px(-8.0))
        };

        if let Some(chat) = &session {
            let Some(space) = space.as_ref().filter(|s| s.git_detected) else {
                return None;
            };
            let is_worktree = chat.cwd.as_deref().is_some_and(|cwd| cwd != space.path);
            let (icon_path, label) = if is_worktree {
                (crate::icons::FOLDER_WITH_FILES, "Worktree")
            } else {
                (crate::icons::FOLDER, "Local checkout")
            };
            let left = div()
                .flex()
                .flex_row()
                .items_center()
                .min_w_0()
                .child(Self::footer_label(
                    icon_path,
                    SharedString::from(label),
                    &theme,
                ));
            let right = div()
                .flex()
                .flex_row()
                .items_center()
                .min_w_0()
                .child(Self::footer_label(
                    crate::icons::GIT_BRANCH,
                    chat.branch
                        .clone()
                        .map(SharedString::from)
                        .unwrap_or_else(|| SharedString::from("No ref")),
                    &theme,
                ));
            return Some(row().child(left).child(right).into_any_element());
        }

        let git = space.as_ref().is_some_and(|s| s.git_detected);
        if !git {
            return None;
        }
        self.ensure_refs(false, cx);
        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            Some(PickerKind::Branch) => {
                let content = self.render_branch_popover(cx);
                Some((PickerKind::Branch, self.popover_frame(320.0, content, cx)))
            }
            Some(PickerKind::Checkout) => {
                let content = self.render_checkout_popover(cx);
                Some((PickerKind::Checkout, self.popover_frame(224.0, content, cx)))
            }
            _ => None,
        };

        let ref_label = self.ref_label();
        let ref_chip = self.footer_chip(
            PickerKind::Branch,
            "picker-branch",
            crate::icons::GIT_BRANCH,
            ref_label,
            &theme,
            cx,
        );
        let kind_icon = match (self.config.checkout, self.selected_ref_worktree().is_some()) {
            (CheckoutKind::Local, false) => crate::icons::FOLDER,
            _ => crate::icons::FOLDER_WITH_FILES,
        };
        let kind_chip = self.footer_chip(
            PickerKind::Checkout,
            "picker-checkout",
            kind_icon,
            SharedString::from(self.checkout_label()),
            &theme,
            cx,
        );
        let left = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .child(attach_overlay(
                kind_chip,
                &mut overlay,
                PickerKind::Checkout,
                "checkout-popover",
                closing,
            ));
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .child(attach_overlay_end(
                ref_chip,
                &mut overlay,
                PickerKind::Branch,
                "branch-popover",
                closing,
            ));
        Some(row().child(left).child(right).into_any_element())
    }

    fn popover_frame(&self, width: f32, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        popover::popover_card(&theme)
            .w(px(width))
            .max_h(px(640.0))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    fn popover_frame_flush(
        &self,
        width: f32,
        content: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        popover::popover_card_flush(&theme)
            .w(px(width))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close(cx)))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    fn search_box(&self, theme: &Theme) -> AnyElement {
        popover::search_input_frame(theme, self.search.clone().into_any_element())
            .into_any_element()
    }

    fn retry_row(
        &self,
        id: &'static str,
        message: &str,
        kind: PickerKind,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        popover::error_row(theme, message)
            .child(
                div()
                    .id(id)
                    .px(px(Theme::SPACE_SM))
                    .py(px(3.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .border_1()
                    .border_color(theme.border)
                    .text_color(theme.text)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| match kind {
                        PickerKind::Branch | PickerKind::Checkout => this.ensure_refs(true, cx),
                        PickerKind::HarnessModel | PickerKind::Traits => {
                            this.harnesses = Loadable::Idle;
                            this.models.clear();
                            this.ensure_harnesses(false, cx);
                        }
                        PickerKind::Space | PickerKind::Device => {}
                    }))
                    .child(SharedString::from("Retry")),
            )
            .into_any_element()
    }

    fn render_branch_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        if self.state.read(cx).selected_space_row().is_none() {
            return div()
                .p(px(Theme::SPACE_SM))
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("No project selected"))
                .into_any_element();
        }
        let rows = self.filtered_ref_rows(cx);
        let total = rows.len();
        let shown = total.min(MAX_REF_ROWS);
        let session_branch = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.branch.clone());
        let switching = self.switching.clone();
        let body: AnyElement =
            match &self.refs {
                Loadable::Loading | Loadable::Idle => {
                    popover::skeleton_rows("branch-skeleton", &theme, 4, cx.entity_id(), cx)
                }
                Loadable::Error(message) => {
                    let message = message.clone();
                    self.retry_row("branch-retry", &message, PickerKind::Branch, &theme, cx)
                }
                Loadable::Ready(_) if rows.is_empty() => div()
                    .p(px(Theme::SPACE_SM))
                    .text_size(px(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("No refs found."))
                    .into_any_element(),
                Loadable::Ready(_) => {
                    let active = self.active;
                    let selected = session_branch.or_else(|| self.config.branch.clone());
                    div()
                        .id("branch-list")
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .max_h(px(224.0))
                        .overflow_y_scroll()
                        .children(rows.into_iter().take(MAX_REF_ROWS).enumerate().map(
                            |(ix, row)| {
                                let label: SharedString = row.name.clone().into();
                                let is_selected = selected.as_deref() == Some(row.name.as_str());
                                let tag: Option<&'static str> = if row.current {
                                    Some("current")
                                } else if row.worktree_path.is_some() {
                                    Some("worktree")
                                } else {
                                    None
                                };
                                let is_switching = switching.as_deref() == Some(row.name.as_str());
                                popover::menu_row_nav(
                                    &theme,
                                    is_selected,
                                    ix == active,
                                    format!("branch-row-{ix}"),
                                )
                                .id(("branch-row", ix))
                                .when(switching.is_some(), |el| el.opacity(0.55))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_ref(row.clone(), cx);
                                }))
                                .child(div().flex_1().min_w_0().truncate().child(label))
                                .when(is_switching, |el| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_size(px(10.0))
                                            .text_color(theme.text_muted.opacity(0.6))
                                            .child(SharedString::from("switching…")),
                                    )
                                })
                                .when_some(tag, |el, tag| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_size(px(10.0))
                                            .text_color(theme.text_muted.opacity(0.45))
                                            .child(SharedString::from(tag)),
                                    )
                                })
                            },
                        ))
                        .into_any_element()
                }
            };
        let mut popover = div()
            .flex()
            .flex_col()
            .child(self.search_box(&theme))
            .child(body);
        if let Some(error) = &self.switch_error {
            popover = popover.child(
                popover::menu_section().child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.danger.opacity(0.9))
                        .child(SharedString::from(error.clone())),
                ),
            );
        }
        if total > shown {
            popover = popover.child(
                popover::menu_section().child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!(
                            "Showing {shown} of {total} refs"
                        ))),
                ),
            );
        }
        popover.into_any_element()
    }

    fn render_checkout_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let has_worktree = self.selected_ref_worktree().is_some();
        let local_label: &'static str = if has_worktree {
            "Current worktree"
        } else {
            "Current checkout"
        };
        let local_icon = if has_worktree {
            crate::icons::FOLDER_WITH_FILES
        } else {
            crate::icons::FOLDER
        };
        let options: [(CheckoutKind, &'static str, &'static str); 2] = [
            (CheckoutKind::Local, local_label, local_icon),
            (
                CheckoutKind::NewWorktree,
                "New worktree",
                crate::icons::FOLDER_WITH_FILES,
            ),
        ];
        let active = self.active;
        let current = self.config.checkout;
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(
                options
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (kind, label, icon_path))| {
                        let is_selected = current == kind;
                        popover::menu_row_nav(
                            &theme,
                            is_selected,
                            ix == active,
                            format!("checkout-row-{ix}"),
                        )
                        .id(("checkout-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick_checkout(kind, cx);
                        }))
                        .child(
                            crate::icons::icon(icon_path)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(label)),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_harness_model_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        const HEIGHT: f32 = 346.0;

        let theme = Theme::of(cx).clone();

        match &self.harnesses {
            Loadable::Loading | Loadable::Idle => {
                return div()
                    .h(px(HEIGHT))
                    .p(px(8.0))
                    .child(popover::skeleton_rows(
                        "harness-skeleton",
                        &theme,
                        4,
                        cx.entity_id(),
                        cx,
                    ))
                    .into_any_element();
            }
            Loadable::Error(message) => {
                let message = message.clone();
                return div()
                    .h(px(HEIGHT))
                    .p(px(8.0))
                    .child(self.retry_row(
                        "harness-retry",
                        &message,
                        PickerKind::HarnessModel,
                        &theme,
                        cx,
                    ))
                    .into_any_element();
            }
            Loadable::Ready(_) => {}
        }

        let locked = self.harness_locked(cx);
        let effective = self.effective_harness(cx);
        let model_scroll = self.model_scroll.clone();
        let query = self.search.read(cx).text().trim().to_string();
        let searching = !query.is_empty();
        let favorites_view = !searching && self.model_rail == ModelRail::Favorites;
        let descriptors = self.rail_descriptors(cx);
        let rows = self.visible_model_rows(cx);
        let active = self.active;
        let selected_id = self.selected_model(cx).map(|m| m.id.clone());

        let rail: Option<AnyElement> = (!searching).then(|| {
            let mut column = div()
                .w(px(44.0))
                .flex_none()
                .p(px(4.0))
                .flex()
                .flex_col()
                .gap(px(4.0));
            column = column.child(
                div()
                    .id("model-rail-favorites")
                    .relative()
                    .w(px(36.0))
                    .h(px(36.0))
                    .rounded(px(8.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .when(!favorites_view, |el| {
                        el.hover(|s| s.bg(crate::theme::ink(0.06)))
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.model_rail = ModelRail::Favorites;
                        this.active = this.selected_model_index(cx);
                        this.model_scroll.set_offset(gpui::Point::default());
                        this.model_scroll.scroll_to_item(this.active);
                        cx.notify();
                    }))
                    .child(
                        crate::icons::icon(crate::icons::STAR_BOLD)
                            .size(px(17.0))
                            .text_color(if favorites_view {
                                theme.text
                            } else {
                                theme.text_muted.opacity(0.75)
                            }),
                    )
                    .when(favorites_view, |el| {
                        el.child(rail_indicator(picker_purple(&theme)))
                    }),
            );
            column = column.child(
                div()
                    .h(px(1.0))
                    .mx(px(-4.0))
                    .my(px(1.0))
                    .bg(crate::theme::hairline(0.08)),
            );
            for (ix, descriptor) in descriptors.iter().enumerate() {
                let harness = descriptor.id;
                let is_viewed = !favorites_view && effective == Some(harness);
                let is_disabled = locked && effective != Some(harness);
                let (icon_path, tint) = harness_brand_icon(harness);
                column = column.child(
                    div()
                        .id(("harness-tab", ix))
                        .relative()
                        .w(px(36.0))
                        .h(px(36.0))
                        .rounded(px(8.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(is_disabled, |el| el.opacity(0.35))
                        .when(!is_disabled, |el| el.cursor_pointer())
                        .when(!is_disabled && !is_viewed, |el| {
                            el.hover(|s| s.bg(crate::theme::ink(0.06)))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.model_rail = ModelRail::Harness;
                            this.pick_harness(harness, cx);
                            cx.notify();
                        }))
                        .child(crate::icons::icon(icon_path).size(px(18.0)).text_color(
                            tint.unwrap_or(if is_viewed {
                                theme.text
                            } else {
                                theme.text_muted
                            }),
                        ))
                        .when(is_viewed, |el| {
                            el.child(rail_indicator(picker_purple(&theme)))
                        }),
                );
            }
            column.into_any_element()
        });

        let search_row = div()
            .flex_none()
            .h(px(46.0))
            .px(px(10.0))
            .border_b_1()
            .border_color(crate::theme::hairline(0.08))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                crate::icons::icon(crate::icons::MAGNIFER)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.0))
                    .child(self.search.clone()),
            );

        let effective_models = effective.and_then(|h| self.models.get(&h));
        let list_children: Vec<AnyElement> = if !rows.is_empty() {
            rows.iter()
                .enumerate()
                .map(|(ix, row)| {
                    let is_selected = Some(row.harness) == effective
                        && selected_id.as_deref() == Some(row.model.id.as_str());
                    let is_active = ix == active;
                    let is_fav = self.defaults.is_favorite(row.harness, &row.model.id);
                    let (icon_path, tint) = harness_brand_icon(row.harness);
                    let label: SharedString = row.model.label.clone().into();
                    let harness_name = row.harness_name.clone();
                    let harness = row.harness;
                    let star_model = row.model.id.clone();
                    let mut el = div()
                        .id(("model-row", ix))
                        .px(px(8.0))
                        .py(px(6.0))
                        .rounded(px(8.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.0))
                        .cursor_pointer();
                    if is_selected {
                        el = el
                            .bg(crate::theme::card_selected_bg())
                            .shadow(crate::theme::card_selected_shadows());
                    } else if is_active {
                        el = el.bg(crate::theme::ink(0.05));
                    }
                    el = el.on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered && this.active != ix {
                            this.active = ix;
                            cx.notify();
                        }
                    }));
                    el = el
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.activate_model_index(ix, cx);
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(
                                    div()
                                        .w_full()
                                        .truncate()
                                        .text_size(px(12.5))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text)
                                        .child(label),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(6.0))
                                        .child(
                                            crate::icons::icon(icon_path)
                                                .size(px(11.0))
                                                .flex_none()
                                                .text_color(
                                                    tint.unwrap_or(theme.text_muted.opacity(0.7)),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .min_w_0()
                                                .truncate()
                                                .text_size(px(11.0))
                                                .text_color(theme.text_muted.opacity(0.7))
                                                .child(harness_name),
                                        ),
                                ),
                        );
                    if ix < 9 {
                        el = el.child(popover::kbd_hint(&theme, &format!("⌘{}", ix + 1)));
                    }
                    el = el.child(
                        div()
                            .id(("model-star", ix))
                            .flex_none()
                            .w(px(22.0))
                            .h(px(22.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|s| s.bg(crate::theme::ink(0.08)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_model_favorite(harness, &star_model, cx);
                            }))
                            .child(
                                crate::icons::icon(if is_fav {
                                    crate::icons::STAR_BOLD
                                } else {
                                    crate::icons::STAR
                                })
                                .size(px(13.0))
                                .text_color(if is_fav {
                                    theme.warning
                                } else {
                                    theme.text_muted.opacity(0.45)
                                }),
                            ),
                    );
                    el.into_any_element()
                })
                .collect()
        } else if searching {
            vec![empty_list_note(&theme, "No models found")]
        } else if favorites_view {
            vec![empty_list_note(
                &theme,
                "No starred models yet — hit a row's star",
            )]
        } else {
            match effective_models {
                Some(Loadable::Error(message)) => {
                    let message = message.clone();
                    vec![self.retry_row(
                        "model-retry",
                        &message,
                        PickerKind::HarnessModel,
                        &theme,
                        cx,
                    )]
                }
                _ => vec![popover::skeleton_rows(
                    "model-skeleton",
                    &theme,
                    4,
                    cx.entity_id(),
                    cx,
                )],
            }
        };

        let pane = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(crate::theme::ink(0.02))
            .when(rail.is_some(), |el| {
                el.border_l_1().border_color(crate::theme::hairline(0.07))
            })
            .child(search_row)
            .child(
                div().flex_1().min_h_0().py(px(6.0)).child(
                    div()
                        .id("model-menu-scroll")
                        .size_full()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .px(px(6.0))
                        .overflow_y_scroll()
                        .track_scroll(&model_scroll)
                        .children(list_children),
                ),
            );

        div()
            .h(px(HEIGHT))
            .flex()
            .flex_row()
            .items_stretch()
            .children(rail)
            .child(pane)
            .into_any_element()
    }

    fn render_traits_sections(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(model) = self.selected_model(cx).cloned() else {
            return popover::skeleton_rows("traits-skeleton", &theme, 3, cx.entity_id(), cx);
        };
        let levels = self.trait_ladder(cx);
        let current = self.effective_reasoning(cx);

        let mut sections: Vec<AnyElement> = Vec::new();
        if !levels.is_empty() {
            let default_level = default_reasoning(&levels);
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(popover::menu_heading(&theme, "Reasoning"))
                    .children(levels.into_iter().enumerate().map(|(ix, level)| {
                        let is_active = current == Some(level);
                        let is_default = default_level == Some(level);
                        let mut row =
                            popover::menu_row(&theme, is_active, format!("trait-reasoning-{ix}"))
                                .id(("reasoning-row", ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_reasoning(level, cx);
                                }))
                                .child(SharedString::from(reasoning_label(level)));
                        row = row.child(div().flex_1());
                        if is_default {
                            row = row.child(default_badge(&theme));
                        }
                        row
                    }))
                    .into_any_element(),
            );
        }

        let selections = self.explicit_options(cx);
        for (opt_ix, option) in model.options.iter().enumerate() {
            if !sections.is_empty() {
                sections.push(popover::menu_separator().into_any_element());
            }
            let selected_choice = selections
                .get(&option.id)
                .and_then(|v| v.as_str())
                .unwrap_or(&option.default_choice)
                .to_string();
            let option_id = option.id.clone();
            let default_choice = option.default_choice.clone();
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(popover::menu_heading(&theme, &option.label))
                    .children(
                        option
                            .choices
                            .iter()
                            .enumerate()
                            .map(|(choice_ix, choice)| {
                                let is_active = selected_choice == choice.id;
                                let choice_id = choice.id.clone();
                                let option_id = option_id.clone();
                                let is_default = choice.id == default_choice;
                                let mut row = popover::menu_row(
                                    &theme,
                                    is_active,
                                    format!("trait-choice-{opt_ix}-{choice_ix}"),
                                )
                                .id(("trait-choice", opt_ix * 32 + choice_ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_option(
                                        option_id.clone(),
                                        choice_id.clone(),
                                        is_default,
                                        cx,
                                    );
                                }))
                                .child(SharedString::from(choice.label.clone()));
                                row = row.child(div().flex_1());
                                if is_default {
                                    row = row.child(default_badge(&theme));
                                }
                                row
                            }),
                    )
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_col()
            .pb(px(2.0))
            .children(sections)
            .into_any_element()
    }
}

fn default_badge(theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .text_size(px(10.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text_muted.opacity(0.6))
        .child(SharedString::from("Default"))
}

fn rail_indicator(tint: gpui::Hsla) -> gpui::Div {
    div()
        .absolute()
        .right(px(-4.0))
        .top(px(8.0))
        .w(px(3.0))
        .h(px(20.0))
        .rounded_tl(px(3.0))
        .rounded_bl(px(3.0))
        .bg(tint)
}

fn picker_purple(theme: &Theme) -> gpui::Hsla {
    match theme.appearance {
        crate::theme::Appearance::Dark => crate::theme::oklch(0.702, 0.183, 293.541),
        crate::theme::Appearance::Light => crate::theme::oklch(0.541, 0.281, 293.009),
    }
}

fn empty_list_note(theme: &Theme, copy: &str) -> AnyElement {
    div()
        .px(px(8.0))
        .py(px(24.0))
        .text_size(px(12.0))
        .text_color(theme.text_muted.opacity(0.6))
        .text_center()
        .child(SharedString::from(copy.to_string()))
        .into_any_element()
}

pub(crate) fn normalize_model_rows(harness: HarnessId, models: Vec<Model>) -> Vec<Model> {
    fn strip_1m(id: &str) -> Option<&str> {
        id.strip_suffix("[1m]").or_else(|| id.strip_suffix("-1m"))
    }
    fn norm(id: &str) -> String {
        id.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase()
    }
    let catalog = match harness {
        HarnessId::ClaudeCode => zeron_harness::claude::catalog::static_models(),
        _ => Vec::new(),
    };
    let curated_label = |id: &str| -> Option<String> {
        let id_norm = norm(id);
        if let Some(row) = catalog.iter().find(|m| norm(&m.id) == id_norm) {
            return Some(row.label.clone());
        }
        (!id_norm.is_empty() && id_norm.chars().all(|c| c.is_ascii_alphabetic()))
            .then(|| catalog.iter().find(|m| norm(&m.id).contains(&id_norm)))
            .flatten()
            .map(|m| m.label.clone())
    };
    let ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
    let has_real = ids.iter().any(|id| !id.eq_ignore_ascii_case("default"));
    let models: Vec<Model> = models
        .into_iter()
        .filter_map(|mut model| {
            if has_real
                && model.id.eq_ignore_ascii_case("default")
                && !model.label.trim().eq_ignore_ascii_case("auto")
                && harness != HarnessId::Cursor
            {
                return None;
            }
            if let Some(base) = strip_1m(&model.id.clone()) {
                if ids.iter().any(|other| other == base) {
                    return None;
                }
                model.id = base.to_string();
                if let Some(at) = model.label.rfind(" (")
                    && model.label.ends_with(')')
                {
                    model.label.truncate(at);
                    while model.label.ends_with(' ') {
                        model.label.pop();
                    }
                }
                if !model.options.iter().any(|o| o.id == "contextWindow") {
                    model.options.push(zeron_proto::ModelOption {
                        id: "contextWindow".into(),
                        label: "Context Window".into(),
                        choices: vec![
                            zeron_proto::ModelOptionChoice {
                                id: "200k".into(),
                                label: "200K".into(),
                            },
                            zeron_proto::ModelOptionChoice {
                                id: "1m".into(),
                                label: "1M".into(),
                            },
                        ],
                        default_choice: "1m".into(),
                    });
                }
            }
            if let Some(label) = curated_label(&model.id) {
                model.label = label;
            }
            Some(model)
        })
        .collect();
    if harness == HarnessId::Cursor {
        pin_cursor_auto(models)
    } else {
        models
    }
}

fn is_cursor_auto_row(model: &Model) -> bool {
    model.label.trim().eq_ignore_ascii_case("auto")
        || matches!(
            model
                .id
                .split_once('[')
                .map(|(b, _)| b)
                .unwrap_or(model.id.as_str()),
            "default" | "auto-smart" | "auto"
        )
}

fn pin_cursor_auto(mut models: Vec<Model>) -> Vec<Model> {
    if let Some(ix) = models.iter().position(is_cursor_auto_row) {
        if ix != 0 {
            let auto = models.remove(ix);
            models.insert(0, auto);
        }
        return models;
    }
    models.insert(
        0,
        Model {
            id: "default".into(),
            label: "Auto".into(),
            description: Some("Cursor picks the model per request".into()),
            reasoning_levels: Vec::new(),
            options: Vec::new(),
        },
    );
    models
}

pub(crate) fn harness_brand_icon(harness: HarnessId) -> (&'static str, Option<gpui::Hsla>) {
    match harness {
        HarnessId::ClaudeCode | HarnessId::Mock => (
            crate::icons::CLAUDE_MARK,
            Some(crate::icons::claude_brand()),
        ),
        HarnessId::Codex => (crate::icons::OPENAI_MARK, None),
        HarnessId::Cursor => (crate::icons::CURSOR_MARK, None),
        HarnessId::Grok => (crate::icons::GROK_MARK, None),
        HarnessId::Hermes => (crate::icons::HERMES_MARK, None),
        HarnessId::Pi => (crate::icons::PI_MARK, None),
        HarnessId::Opencode => (crate::icons::OPENCODE_MARK, None),
    }
}

fn mock_harness_enabled() -> bool {
    std::env::var("ZERON_HARNESS")
        .ok()
        .as_deref()
        .map(str::trim)
        == Some("mock")
}

pub fn visible_harnesses(list: &[HarnessDescriptor]) -> Vec<HarnessDescriptor> {
    visible_harnesses_impl(list, mock_harness_enabled())
}

fn visible_harnesses_impl(list: &[HarnessDescriptor], allow_mock: bool) -> Vec<HarnessDescriptor> {
    if allow_mock {
        return list.to_vec();
    }
    let real: Vec<HarnessDescriptor> = list
        .iter()
        .filter(|d| d.id != HarnessId::Mock)
        .cloned()
        .collect();
    if real.is_empty() { list.to_vec() } else { real }
}

pub fn offered_harnesses(list: &[HarnessDescriptor]) -> Vec<HarnessDescriptor> {
    offered_harnesses_impl(list, mock_harness_enabled())
}

fn offered_harnesses_impl(list: &[HarnessDescriptor], allow_mock: bool) -> Vec<HarnessDescriptor> {
    let visible = visible_harnesses_impl(list, allow_mock);
    let offered: Vec<HarnessDescriptor> = visible
        .iter()
        .filter(|d| {
            zeron_engine::registry::descriptor_enabled(d) || (allow_mock && d.id == HarnessId::Mock)
        })
        .cloned()
        .collect();
    if offered.is_empty() { visible } else { offered }
}

fn attach_overlay(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: Option<std::time::Instant>,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip.child(popover::anchored_menu_above(id, element, closing));
    }
    chip
}

fn attach_overlay_below(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: Option<std::time::Instant>,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip.child(popover::anchored_menu_below(id, element, closing));
    }
    chip
}

fn attach_overlay_end(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: Option<std::time::Instant>,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip
            .relative()
            .child(popover::anchored_menu_above_end(id, element, closing));
    }
    chip
}

impl Render for Pickers {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        if self.boot_focus_pending {
            match self.open_kind() {
                Some(PickerKind::Branch) => {
                    self.search.update(cx, |input, cx| {
                        input.set_placeholder("Search refs…", cx);
                    });
                    let handle = self.search.read(cx).focus_handle(cx);
                    if handle.is_focused(window) {
                        self.boot_focus_pending = false;
                    } else {
                        window.focus(&handle, cx);
                    }
                }
                Some(_) => {
                    if self.focus.is_focused(window) {
                        self.boot_focus_pending = false;
                    } else {
                        window.focus(&self.focus, cx);
                    }
                }
                None => self.boot_focus_pending = false,
            }
        }

        self.ensure_harnesses(false, cx);
        self.prefetch_models(cx);
        if matches!(
            self.open_kind(),
            Some(PickerKind::Branch) | Some(PickerKind::Checkout)
        ) && matches!(self.refs, Loadable::Idle)
        {
            self.ensure_refs(false, cx);
        }
        let model_label: SharedString = {
            let loaded = self.selected_model(cx).map(|m| m.label.clone());
            let label = loaded.or_else(|| {
                let remembered = self
                    .effective_harness(cx)
                    .and_then(|h| self.defaults.model_for(h));
                match self.effective_model_id(cx) {
                    Some(id) => Some(
                        remembered
                            .filter(|m| m.id == id)
                            .map(|m| m.label.clone())
                            .or_else(|| self.defaults.label_for(id).map(str::to_string))
                            .unwrap_or_else(|| id.to_string()),
                    ),
                    None => remembered.map(|m| m.label.clone()),
                }
            });
            label.map(SharedString::from).unwrap_or_default()
        };
        let harness_icon: (&'static str, Option<gpui::Hsla>) = self
            .effective_harness(cx)
            .map(harness_brand_icon)
            .unwrap_or((
                crate::icons::CLAUDE_MARK,
                Some(crate::icons::claude_brand()),
            ));
        let explicit_options = self.explicit_options(cx);
        let traits_set = traits_summary(
            self.selected_model(cx),
            self.effective_reasoning(cx),
            &explicit_options,
        );
        let traits_active = traits_customized(
            self.selected_model(cx),
            self.effective_reasoning(cx),
            &self.trait_ladder(cx),
            &explicit_options,
        );
        let traits_label: SharedString = traits_set
            .clone()
            .map(SharedString::from)
            .unwrap_or_else(|| SharedString::from("Traits"));

        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            Some(PickerKind::Branch)
            | Some(PickerKind::Checkout)
            | Some(PickerKind::Space)
            | Some(PickerKind::Device) => None,
            Some(PickerKind::HarnessModel) => {
                let content = self.render_harness_model_popover(cx);
                Some((
                    PickerKind::HarnessModel,
                    self.popover_frame_flush(360.0, content, cx),
                ))
            }
            Some(PickerKind::Traits) => {
                let content = div()
                    .p(px(4.0))
                    .child(self.render_traits_sections(cx))
                    .into_any_element();
                Some((
                    PickerKind::Traits,
                    self.popover_frame_flush(240.0, content, cx),
                ))
            }
            None => None,
        };

        let left = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .gap(px(4.0));
        let model_chip = self.trigger_chip(
            PickerKind::HarnessModel,
            model_label,
            true,
            Some(harness_icon),
            None,
            &theme,
            cx,
        );
        let has_traits = !self.trait_ladder(cx).is_empty()
            || self
                .selected_model(cx)
                .is_some_and(|m| !m.options.is_empty());
        let traits_chip = has_traits.then(|| {
            self.trigger_chip(
                PickerKind::Traits,
                traits_label,
                traits_active,
                None,
                None,
                &theme,
                cx,
            )
        });
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .gap(px(4.0))
            .child(attach_overlay_end(
                model_chip,
                &mut overlay,
                PickerKind::HarnessModel,
                "model-popover",
                closing,
            ))
            .children(traits_chip.map(|chip| {
                attach_overlay_end(
                    chip,
                    &mut overlay,
                    PickerKind::Traits,
                    "traits-popover",
                    closing,
                )
            }));
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(Theme::SPACE_SM))
            .child(left)
            .child(right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::{FolderEntry, Model, ModelOption, ModelOptionChoice};

    fn bare_model(id: &str, label: &str) -> Model {
        Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: Vec::new(),
            options: Vec::new(),
        }
    }

    #[test]
    fn normalize_drops_default_alias_and_folds_orphan_1m_rows() {
        let models = normalize_model_rows(
            HarnessId::Codex,
            vec![
                bare_model("default", "Default (recommended)"),
                bare_model("titan[1m]", "Titan (1M context)"),
                bare_model("gpt-x-9[1m]", "GPT X-9"),
                bare_model("nano", "Nano"),
            ],
        );
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["titan", "gpt-x-9", "nano"]
        );
        assert_eq!(models[0].label, "Titan");
        assert_eq!(models[1].label, "GPT X-9");
        assert!(
            models[0]
                .options
                .iter()
                .any(|o| o.id == "contextWindow" && o.default_choice == "1m")
        );
        assert!(models[2].options.is_empty());

        let only_default =
            normalize_model_rows(HarnessId::Codex, vec![bare_model("default", "Default")]);
        assert_eq!(only_default.len(), 1);

        let paired = normalize_model_rows(
            HarnessId::Codex,
            vec![
                bare_model("titan-5", "Titan 5"),
                bare_model("titan-5[1m]", "Titan 5 (1M)"),
            ],
        );
        assert_eq!(paired.len(), 1);
        assert_eq!(paired[0].id, "titan-5");

        let clean = vec![bare_model("titan-5", "Titan 5")];
        assert_eq!(normalize_model_rows(HarnessId::Codex, clean.clone()), clean);

        let cursor = normalize_model_rows(
            HarnessId::Cursor,
            vec![
                bare_model("default", "Auto"),
                bare_model("composer-2.5", "Composer 2.5"),
            ],
        );
        assert_eq!(
            cursor.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["default", "composer-2.5"]
        );
        assert_eq!(cursor[0].label, "Auto");

        let injected = normalize_model_rows(
            HarnessId::Cursor,
            vec![bare_model("composer-2.5", "Composer 2.5")],
        );
        assert_eq!(
            injected.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["default", "composer-2.5"]
        );
        assert_eq!(injected[0].label, "Auto");
    }

    #[test]
    fn normalize_gives_claude_rows_their_versioned_catalog_labels() {
        let models = normalize_model_rows(
            HarnessId::ClaudeCode,
            vec![
                bare_model("default", "Default (recommended)"),
                bare_model("opus[1m]", "Opus (1M context)"),
                bare_model("claude-fable-5[1m]", "Fable"),
                bare_model("sonnet", "Sonnet"),
                bare_model("haiku", "Haiku"),
                bare_model("claude-nova-1", "Nova 1"),
            ],
        );
        assert_eq!(
            models.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(),
            vec!["Opus 5", "Fable 5", "Sonnet 5", "Haiku 4.5", "Nova 1"]
        );
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["opus", "claude-fable-5", "sonnet", "haiku", "claude-nova-1"]
        );
    }

    #[test]
    fn traits_summary_formats_non_defaults() {
        let model = Model {
            id: "opus".into(),
            label: "Opus".into(),
            description: None,
            reasoning_levels: vec![ReasoningLevel::Medium, ReasoningLevel::High],
            options: vec![
                ModelOption {
                    id: "context".into(),
                    label: "Context window".into(),
                    choices: vec![
                        ModelOptionChoice {
                            id: "standard".into(),
                            label: "Standard".into(),
                        },
                        ModelOptionChoice {
                            id: "1m".into(),
                            label: "1M".into(),
                        },
                    ],
                    default_choice: "standard".into(),
                },
                ModelOption {
                    id: "speed".into(),
                    label: "Speed".into(),
                    choices: vec![
                        ModelOptionChoice {
                            id: "normal".into(),
                            label: "Normal".into(),
                        },
                        ModelOptionChoice {
                            id: "fast".into(),
                            label: "Fast".into(),
                        },
                    ],
                    default_choice: "normal".into(),
                },
            ],
        };
        let mut selections = serde_json::Map::new();
        selections.insert("context".into(), serde_json::Value::String("1m".into()));
        selections.insert("speed".into(), serde_json::Value::String("fast".into()));
        assert_eq!(
            traits_summary(Some(&model), Some(ReasoningLevel::High), &selections),
            Some("High · 1M · Fast".to_string())
        );
        assert_eq!(
            traits_summary(Some(&model), None, &serde_json::Map::new()),
            Some("Standard · Normal".to_string())
        );
        let mut stale = serde_json::Map::new();
        stale.insert(
            "speed".into(),
            serde_json::Value::String("ludicrous".into()),
        );
        assert_eq!(
            traits_summary(Some(&model), None, &stale),
            Some("Standard · Normal".to_string())
        );
        assert_eq!(
            traits_summary(
                None,
                Some(ReasoningLevel::Ultrathink),
                &serde_json::Map::new()
            ),
            Some("Ultrathink".to_string())
        );
        assert_eq!(traits_summary(None, None, &serde_json::Map::new()), None);

        let ladder = model.reasoning_levels.clone();
        assert!(traits_customized(
            Some(&model),
            Some(ReasoningLevel::High),
            &ladder,
            &selections
        ));
        assert!(!traits_customized(
            Some(&model),
            default_reasoning(&ladder),
            &ladder,
            &serde_json::Map::new()
        ));
        let mut defaults = serde_json::Map::new();
        defaults.insert("speed".into(), serde_json::Value::String("normal".into()));
        assert!(!traits_customized(
            Some(&model),
            default_reasoning(&ladder),
            &ladder,
            &defaults
        ));
        assert!(!traits_customized(
            Some(&model),
            default_reasoning(&ladder),
            &ladder,
            &stale
        ));
        assert!(traits_customized(
            Some(&model),
            Some(ReasoningLevel::Medium),
            &ladder,
            &serde_json::Map::new()
        ));
    }

    #[test]
    fn folder_paths_and_breadcrumbs() {
        assert_eq!(parent_path("/home/w/dev"), Some("/home/w".to_string()));
        assert_eq!(parent_path("/home"), Some("/".to_string()));
        assert_eq!(parent_path("/home/"), Some("/".to_string()));
        assert_eq!(parent_path("/"), None);
        assert_eq!(parent_path(""), None);
        assert_eq!(child_path("/home", "w"), "/home/w");
        assert_eq!(child_path("/", "home"), "/home");
        let crumbs = breadcrumbs("/home/w/dev");
        let labels: Vec<&str> = crumbs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["/", "home", "w", "dev"]);
        assert_eq!(crumbs[2].1, "/home/w");
        assert_eq!(breadcrumbs("/").len(), 1);
    }

    #[test]
    fn completion_prefix_lengths() {
        assert_eq!(completion_prefix_len("Documents", "doc"), Some(3));
        assert_eq!(&"Documents"[3..], "uments");
        assert_eq!(completion_prefix_len("zeron", "zeron"), Some(5));
        assert_eq!(completion_prefix_len("zeron", ""), Some(0));
        assert_eq!(completion_prefix_len("zeron", "dev"), None);
        assert_eq!(completion_prefix_len("dev", "devel"), None);
        assert_eq!(completion_prefix_len("héllo", "hé"), Some(3));
        assert_eq!(&"héllo"[3..], "llo");
    }

    #[test]
    fn segment_target_resolution() {
        let names = ["github", "GitHub", "worktree"];
        assert_eq!(segment_target(&names, "GitHub"), Some(1));
        assert_eq!(segment_target(&names, "github"), Some(0));
        assert_eq!(segment_target(&names, "WORKTREE"), Some(2));
        assert_eq!(segment_target(&names, "work"), Some(2));
        assert_eq!(segment_target(&names, "g"), None);
        assert_eq!(segment_target(&names, "x"), None);
    }

    #[test]
    fn browser_navigation_reducer() {
        let listing = FolderListing {
            path: "/home/w".into(),
            entries: vec![
                FolderEntry {
                    name: "notes.txt".into(),
                    is_dir: false,
                    is_repo: false,
                },
                FolderEntry {
                    name: "dev".into(),
                    is_dir: true,
                    is_repo: false,
                },
                FolderEntry {
                    name: "zeron".into(),
                    is_dir: true,
                    is_repo: true,
                },
            ],
            truncated: false,
        };
        assert_eq!(browser_rows(&listing).len(), 2);
        assert_eq!(browser_rows(&listing)[1].name, "zeron");
    }

    #[test]
    fn resolved_chat_config_requires_harness() {
        let mut resolved = ResolvedRunConfig::default();
        assert!(resolved.chat_config().is_none());
        resolved.harness = Some(HarnessId::ClaudeCode);
        resolved.model = Some("opus".into());
        resolved.reasoning = Some(ReasoningLevel::High);
        let config = resolved.chat_config().expect("harness set");
        assert_eq!(config.harness, HarnessId::ClaudeCode);
        assert_eq!(config.model.as_deref(), Some("opus"));
        assert_eq!(config.sandbox, SandboxLevel::WorkspaceWrite);
    }

    #[test]
    fn default_model_is_first_catalog_row() {
        let models = vec![
            Model {
                id: "flagship".into(),
                label: "Flagship".into(),
                description: None,
                reasoning_levels: vec![],
                options: vec![],
            },
            Model {
                id: "fast".into(),
                label: "Fast".into(),
                description: None,
                reasoning_levels: vec![],
                options: vec![],
            },
        ];
        assert_eq!(default_model(&models).map(|m| &*m.id), Some("flagship"));
        assert!(default_model(&[]).is_none());
    }

    #[test]
    fn default_reasoning_prefers_high_then_medium() {
        use ReasoningLevel::*;
        assert_eq!(
            default_reasoning(&[Low, Medium, High, XHigh, Max, Ultracode, Ultrathink]),
            Some(High)
        );
        assert_eq!(default_reasoning(&[Low, Medium, High, Max]), Some(High));
        assert_eq!(default_reasoning(&[Minimal, Low, Medium]), Some(Medium));
        assert_eq!(default_reasoning(&[Minimal, Low]), Some(Minimal));
        assert_eq!(default_reasoning(&[]), None);
    }

    #[test]
    fn clamp_reasoning_keeps_offered_levels_and_heals_foreign_ones() {
        use ReasoningLevel::*;
        let ladder = [Low, Medium, High, Max];
        assert_eq!(clamp_reasoning(Some(Max), &ladder), Some(Max));
        assert_eq!(clamp_reasoning(Some(XHigh), &ladder), Some(High));
        assert_eq!(clamp_reasoning(None, &ladder), Some(High));
        assert_eq!(clamp_reasoning(Some(High), &[]), None);
    }

    #[test]
    fn mock_harness_hidden_unless_alone() {
        let descriptor = |id: HarnessId, name: &str| HarnessDescriptor {
            id,
            name: name.into(),
            supports_steering: true,
            steering_mode: zeron_proto::SteeringMode::StepBoundary,
            reasoning_levels: vec![],
            installed: true,
            enabled: None,
        };
        let mixed = vec![
            descriptor(HarnessId::Mock, "Mock"),
            descriptor(HarnessId::ClaudeCode, "Claude Code"),
        ];
        let visible = visible_harnesses_impl(&mixed, false);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, HarnessId::ClaudeCode);
        let only_mock = vec![descriptor(HarnessId::Mock, "Mock")];
        assert_eq!(visible_harnesses_impl(&only_mock, false).len(), 1);
        assert_eq!(visible_harnesses_impl(&mixed, true).len(), 2);
        assert_eq!(visible_harnesses_impl(&mixed, true)[0].id, HarnessId::Mock);
    }

    #[test]
    fn offered_harnesses_follow_the_catalog_enabled_flags() {
        let descriptor = |id: HarnessId, name: &str, enabled: Option<bool>| HarnessDescriptor {
            id,
            name: name.into(),
            supports_steering: true,
            steering_mode: zeron_proto::SteeringMode::StepBoundary,
            reasoning_levels: vec![],
            installed: true,
            enabled,
        };
        let catalog = |claude: Option<bool>, codex: Option<bool>, grok: Option<bool>| {
            vec![
                descriptor(HarnessId::Mock, "Mock", Some(false)),
                descriptor(HarnessId::ClaudeCode, "Claude Code", claude),
                descriptor(HarnessId::Codex, "Codex", codex),
                descriptor(HarnessId::Grok, "Grok", grok),
            ]
        };
        let offered = offered_harnesses_impl(&catalog(None, None, None), false);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::ClaudeCode, HarnessId::Codex]
        );
        let offered = offered_harnesses_impl(&catalog(Some(true), Some(false), Some(true)), false);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::ClaudeCode, HarnessId::Grok]
        );
        let offered = offered_harnesses_impl(&catalog(Some(true), Some(false), None), true);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::Mock, HarnessId::ClaudeCode]
        );
        let offered =
            offered_harnesses_impl(&catalog(Some(false), Some(false), Some(false)), false);
        assert_eq!(offered.len(), 3);
    }
}
