use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gpui::{App, Context, Entity, Task};
use gpui_tokio::Tokio;
use serde::de::DeserializeOwned;

use crate::comments::DiffComment;
use zeron_doc::{SessionMessageEntry, TranscriptDesync, TranscriptFrame};
use zeron_engine::{Engine, EngineConfig, EngineRuntime, InstanceLock};
use zeron_proto::{
    AuthState, Chat, ChatIndicator, Device, EngineInfo, HarnessId, Session, Space, WorkspaceScope,
};
use zeron_rpc::{RpcClient, RpcError, RpcReply, RpcService, connect_ws, memory_client, methods};

#[derive(Debug, Clone)]
pub struct EngineBootConfig {
    pub data_dir: PathBuf,
    pub ipc_port: u16,
    pub default_harness: HarnessId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineMode {
    InProcess,
    Remote { url: String },
}

#[async_trait]
trait EngineBackend: Send + Sync {
    fn client(&self) -> &RpcClient;
    fn mode(&self) -> EngineMode;
    async fn shutdown(&self);
}

struct InProcessEngine {
    runtime: Arc<tokio::sync::Mutex<Option<EngineRuntime>>>,
    boot_task: tokio::task::JoinHandle<()>,
    ipc_task: Option<tokio::task::JoinHandle<()>>,
    client: RpcClient,
}

#[async_trait]
impl EngineBackend for InProcessEngine {
    fn client(&self) -> &RpcClient {
        &self.client
    }
    fn mode(&self) -> EngineMode {
        EngineMode::InProcess
    }
    async fn shutdown(&self) {
        self.boot_task.abort();
        if let Some(ipc) = &self.ipc_task {
            ipc.abort();
        }
        if let Some(runtime) = self.runtime.lock().await.take() {
            runtime.shutdown().await;
        }
    }
}

#[derive(Clone)]
enum DeferredEngineState {
    Waiting,
    Ready,
    Failed(String),
}

struct DeferredEngineRpc {
    engine_info: EngineInfo,
    state: tokio::sync::watch::Receiver<DeferredEngineState>,
    service: Arc<tokio::sync::OnceCell<Arc<dyn RpcService>>>,
}

#[async_trait]
impl RpcService for DeferredEngineRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        if method == methods::ENGINE_INFO {
            return RpcReply::value(&self.engine_info);
        }
        if method == methods::ENGINE_READY {
            let mut state = self.state.clone();
            return match wait_for_deferred_engine(&mut state).await {
                Ok(()) => RpcReply::value(&serde_json::json!({ "ready": true })),
                Err(message) => Err(RpcError::Failed(message)),
            };
        }
        let mut state = self.state.clone();
        loop {
            let current = { state.borrow().clone() };
            match current {
                DeferredEngineState::Waiting => {}
                DeferredEngineState::Ready => {
                    let service = self.service.get().ok_or_else(|| {
                        RpcError::Failed(
                            "embedded engine became ready without an RPC service".into(),
                        )
                    })?;
                    return service.handle(method, params).await;
                }
                DeferredEngineState::Failed(message) => return Err(RpcError::Failed(message)),
            }
            state.changed().await.map_err(|_| RpcError::Closed)?;
        }
    }
}

async fn wait_for_deferred_engine(
    state: &mut tokio::sync::watch::Receiver<DeferredEngineState>,
) -> Result<(), String> {
    loop {
        let current = { state.borrow().clone() };
        match current {
            DeferredEngineState::Waiting => {}
            DeferredEngineState::Ready => return Ok(()),
            DeferredEngineState::Failed(message) => return Err(message),
        }
        state
            .changed()
            .await
            .map_err(|_| "embedded engine assembly ended without a result".to_string())?;
    }
}

struct RemoteEngine {
    client: Arc<RpcClient>,
    url: String,
    lifecycle_task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[async_trait]
impl EngineBackend for RemoteEngine {
    fn client(&self) -> &RpcClient {
        &self.client
    }
    fn mode(&self) -> EngineMode {
        EngineMode::Remote {
            url: self.url.clone(),
        }
    }
    async fn shutdown(&self) {
        if let Some(task) = self.lifecycle_task.lock().await.take() {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<dyn EngineBackend>,
    engine_info: EngineInfo,
    deferred_state: Option<tokio::sync::watch::Receiver<DeferredEngineState>>,
}

impl EngineHandle {
    pub async fn bootstrap(config: EngineBootConfig) -> anyhow::Result<EngineHandle> {
        static BOOTSTRAP_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _gate = BOOTSTRAP_GATE.lock().await;

        if let Some(handle) = Self::attach_to_daemon(config.ipc_port).await {
            return Ok(handle);
        }

        tracing::info!(data_dir = %config.data_dir.display(), "no daemon on port; embedding engine");
        let engine_config = EngineConfig {
            data_dir: config.data_dir,
            ipc_port: config.ipc_port,
            default_harness: config.default_harness,
        };

        std::fs::create_dir_all(&engine_config.data_dir)?;
        let lock_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let lock = loop {
            match InstanceLock::acquire(&engine_config.data_dir) {
                Ok(lock) => break lock,
                Err(err) => {
                    if std::time::Instant::now() >= lock_deadline {
                        return Err(err.into());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    if let Some(handle) = Self::attach_to_daemon(engine_config.ipc_port).await {
                        return Ok(handle);
                    }
                }
            }
        };

        let engine_info = Engine::engine_info(&engine_config)?;
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(DeferredEngineState::Waiting);
        let assembled_service = Arc::new(tokio::sync::OnceCell::new());
        let service: Arc<dyn RpcService> = Arc::new(DeferredEngineRpc {
            engine_info: engine_info.clone(),
            state: state_rx.clone(),
            service: assembled_service.clone(),
        });
        let client = memory_client(service.clone());

        let ipc_task = match zeron_engine::serve_ipc(engine_config.ipc_port, service).await {
            Ok(task) => Some(task),
            Err(err) => {
                tracing::warn!(
                    port = engine_config.ipc_port,
                    error = %err,
                    "IPC port unavailable; other viewports cannot attach to this window"
                );
                None
            }
        };
        let runtime = Arc::new(tokio::sync::Mutex::new(None));
        let runtime_for_boot = runtime.clone();
        let service_for_boot = assembled_service.clone();
        let boot_task = tokio::spawn(async move {
            match Engine::assemble_runtime_with_lock(&engine_config, lock).await {
                Ok(engine_runtime) => {
                    let service: Arc<dyn RpcService> = engine_runtime.core().rpc_service();
                    *runtime_for_boot.lock().await = Some(engine_runtime);
                    if service_for_boot.set(service).is_err() {
                        state_tx.send_replace(DeferredEngineState::Failed(
                            "embedded engine RPC service was assembled more than once".into(),
                        ));
                        return;
                    }
                    state_tx.send_replace(DeferredEngineState::Ready);
                }
                Err(err) => {
                    tracing::error!(error = %err, "embedded engine assembly failed");
                    state_tx.send_replace(DeferredEngineState::Failed(format!("{err:#}")));
                }
            }
        });
        let handle = EngineHandle {
            inner: Arc::new(InProcessEngine {
                runtime,
                boot_task,
                ipc_task,
                client,
            }),
            engine_info,
            deferred_state: Some(state_rx.clone()),
        };
        if let Err(message) = wait_for_deferred_engine(&mut state_rx).await {
            handle.shutdown().await;
            return Err(anyhow::anyhow!(message));
        }
        Ok(handle)
    }

    async fn attach_to_daemon(ipc_port: u16) -> Option<EngineHandle> {
        let url = format!("ws://127.0.0.1:{ipc_port}");
        let probe = tokio::time::timeout(
            std::time::Duration::from_millis(750),
            tokio::net::TcpStream::connect(("127.0.0.1", ipc_port)),
        )
        .await;
        if !matches!(probe, Ok(Ok(_))) {
            return None;
        }
        tracing::info!(%url, "engine daemon detected; connecting");
        match connect_ws(&url).await {
            Ok(client) => match query_engine_info(&client).await {
                Ok(engine_info) => {
                    let client = Arc::new(client);
                    let (state_tx, state_rx) =
                        tokio::sync::watch::channel(DeferredEngineState::Waiting);
                    let lifecycle_client = client.clone();
                    let lifecycle_task = tokio::spawn(async move {
                        let state = match lifecycle_client
                            .call(methods::ENGINE_READY, serde_json::json!({}))
                            .await
                        {
                            Ok(_) => DeferredEngineState::Ready,
                            Err(RpcError::Failed(message))
                                if message
                                    == format!("unknown method: {}", methods::ENGINE_READY) =>
                            {
                                DeferredEngineState::Ready
                            }
                            Err(err) => DeferredEngineState::Failed(err.to_string()),
                        };
                        state_tx.send_replace(state);
                    });
                    Some(EngineHandle {
                        inner: Arc::new(RemoteEngine {
                            client,
                            url,
                            lifecycle_task: tokio::sync::Mutex::new(Some(lifecycle_task)),
                        }),
                        engine_info,
                        deferred_state: Some(state_rx),
                    })
                }
                Err(err) => {
                    tracing::warn!(
                        %url,
                        error = %err,
                        "listener did not provide engine identity; embedding instead"
                    );
                    None
                }
            },
            Err(err) => {
                tracing::warn!(%url, error = %err, "not an engine; embedding instead");
                None
            }
        }
    }

    pub fn client(&self) -> &RpcClient {
        self.inner.client()
    }

    pub fn mode(&self) -> EngineMode {
        self.inner.mode()
    }

    pub fn engine_info(&self) -> &EngineInfo {
        &self.engine_info
    }

    fn deferred_state(&self) -> Option<tokio::sync::watch::Receiver<DeferredEngineState>> {
        self.deferred_state.clone()
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

async fn query_engine_info(client: &RpcClient) -> Result<EngineInfo, RpcError> {
    match client
        .call_as(methods::ENGINE_INFO, serde_json::json!({}))
        .await
    {
        Ok(info) => Ok(info),
        Err(err)
            if matches!(&err, RpcError::UnknownMethod(method) if method == methods::ENGINE_INFO)
                || matches!(&err, RpcError::Failed(message) if message == &format!("unknown method: {}", methods::ENGINE_INFO)) =>
        {
            let device: serde_json::Value = client
                .call_as(methods::LOCAL_DEVICE, serde_json::json!({}))
                .await?;
            let device_id = device
                .get("deviceId")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| RpcError::BadParams("legacy LocalDevice lacks deviceId".into()))?;
            Ok(EngineInfo {
                device_id: device_id.into(),
                workspace_scope: WorkspaceScope::Synced,
            })
        }
        Err(err) => Err(err),
    }
}

pub use crate::view::{
    ChatGroup, ConnectionStatus, GatePhase, attention_rank, chat_location, format_time_ago,
    gate_phase, group_chats, parse_auth_state, project_label, sort_active, sort_chats, sort_spaces,
    sort_tabs,
};
pub use zeron_proto::{Indicator, SESSION_STALE_MS, display_status, effective_indicator};

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgRow {
    pub organization_id: String,
    pub name: String,
}

pub fn parse_orgs(value: &serde_json::Value) -> Vec<OrgRow> {
    let list = value.get("orgs").unwrap_or(value);
    serde_json::from_value(list.clone()).unwrap_or_default()
}

pub fn org_name_valid(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty() && trimmed.chars().count() <= 64
}

pub fn sort_memberships(mut orgs: Vec<OrgRow>) -> Vec<OrgRow> {
    orgs.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    orgs.dedup_by(|a, b| a.organization_id == b.organization_id);
    orgs
}

#[derive(Debug, Clone)]
struct PendingSend {
    message_id: String,
    started: DateTime<Utc>,
}

pub const PENDING_SEND_TTL_MS: i64 = 30_000;

pub struct AppState {
    pub connection: ConnectionStatus,
    pub workspace_scope: Option<WorkspaceScope>,
    pub auth: Option<AuthState>,
    pub devices: Vec<Device>,
    pub spaces: Vec<Space>,
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,
    pub selected_space: Option<String>,
    pub no_project: bool,
    pub selected_device: Option<String>,
    pub selected_chat: Option<String>,
    pub auto_selected: bool,
    pub chats_synced: bool,
    pub spaces_synced: bool,
    pub transcript: Vec<SessionMessageEntry>,
    echoes: HashMap<String, Vec<SessionMessageEntry>>,
    pending_sends: HashMap<String, PendingSend>,
    diff_comments: HashMap<String, Vec<DiffComment>>,
    pub local_device_id: Option<String>,
    pub data_dir: Option<PathBuf>,
    engine: Option<EngineHandle>,
    watch_tasks: Vec<Task<()>>,
    transcript_task: Option<Task<()>>,
    sub_transcripts: HashMap<String, Vec<SessionMessageEntry>>,
    sub_watch_tasks: HashMap<String, Task<()>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            connection: ConnectionStatus::Connecting,
            workspace_scope: None,
            auth: None,
            devices: Vec::new(),
            spaces: Vec::new(),
            chats: Vec::new(),
            sessions: Vec::new(),
            selected_space: None,
            no_project: false,
            selected_device: None,
            selected_chat: None,
            transcript: Vec::new(),
            echoes: HashMap::new(),
            pending_sends: HashMap::new(),
            diff_comments: HashMap::new(),
            local_device_id: None,
            data_dir: None,
            engine: None,
            watch_tasks: Vec::new(),
            transcript_task: None,
            sub_transcripts: HashMap::new(),
            sub_watch_tasks: HashMap::new(),
            auto_selected: false,
            chats_synced: false,
            spaces_synced: false,
        }
    }

    pub fn composer_key(&self) -> String {
        self.selected_chat.clone().unwrap_or_default()
    }

    pub fn diff_comments(&self, key: &str) -> &[DiffComment] {
        self.diff_comments
            .get(key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn add_diff_comment(&mut self, key: &str, comment: DiffComment) {
        self.diff_comments
            .entry(key.to_string())
            .or_default()
            .push(comment);
    }

    pub fn remove_diff_comment(&mut self, key: &str, id: &str) {
        if let Some(list) = self.diff_comments.get_mut(key) {
            list.retain(|c| c.id != id);
            if list.is_empty() {
                self.diff_comments.remove(key);
            }
        }
    }

    pub fn take_diff_comments(&mut self, key: &str) -> Vec<DiffComment> {
        self.diff_comments.remove(key).unwrap_or_default()
    }

    pub fn purge_diff_comments(&mut self, key: &str) {
        self.diff_comments.remove(key);
    }

    pub fn apply_chats(&mut self, mut chats: Vec<Chat>) {
        sort_chats(&mut chats);
        self.chats = chats;
        self.chats_synced = true;
        if let Some(selected) = &self.selected_chat
            && !self.chats.iter().any(|c| &c.id == selected)
        {
            self.selected_chat = None;
            self.transcript.clear();
            self.transcript_task = None;
        }
    }

    pub fn apply_sessions(&mut self, sessions: Vec<Session>) {
        self.sessions = sessions;
    }

    pub fn apply_spaces(&mut self, mut spaces: Vec<Space>) {
        sort_spaces(&mut spaces);
        self.spaces = spaces;
        self.spaces_synced = true;
        if let Some(selected) = &self.selected_space
            && !self.spaces.iter().any(|s| &s.id == selected)
        {
            self.selected_space = self.first_space_on_picked_device();
        }
        if self.selected_space.is_none() && !self.no_project {
            self.selected_space = self.first_space_on_picked_device();
        }
    }

    pub fn apply_chat_config(&mut self, chat_id: &str, config: zeron_proto::ChatConfig) {
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.config = Some(config);
        }
    }

    pub fn apply_devices(&mut self, mut devices: Vec<Device>) {
        if self.workspace_scope == Some(WorkspaceScope::Local)
            && let Some(local_id) = self.local_device_id.as_deref()
            && let Some(device) = devices.iter_mut().find(|device| device.id == local_id)
            && device.name == "unknown-device"
        {
            device.name = "Local".to_string();
        }
        self.devices = devices;
    }

    pub fn apply_auth(&mut self, auth: AuthState) {
        self.auth = Some(auth);
    }

    pub fn apply_auth_value(&mut self, value: serde_json::Value) {
        if let Some(auth) = parse_auth_state(&value) {
            self.apply_auth(auth);
        }
    }

    pub fn auth_user(&self) -> Option<&zeron_proto::UserProfile> {
        match self.auth.as_ref()? {
            AuthState::SignedIn { user, .. } | AuthState::NeedsOrganization { user } => Some(user),
            AuthState::SignedOut => None,
        }
    }

    fn first_space_on_picked_device(&self) -> Option<String> {
        let device = self
            .selected_device
            .as_deref()
            .or(self.local_device_id.as_deref());
        let sorted = self.spaces_sorted();
        device
            .and_then(|d| sorted.iter().find(|s| s.device_id == d).copied())
            .or_else(|| sorted.first().copied())
            .map(|s| s.id.clone())
    }

    pub fn apply_transcript(&mut self, entries: Vec<SessionMessageEntry>) {
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            echoes.retain(|echo| !entries.iter().any(|e| e.id == echo.id));
        }
        self.transcript = entries;
        self.ack_pending_send_from_transcript();
    }

    pub fn apply_transcript_frame(
        &mut self,
        frame: TranscriptFrame,
    ) -> Result<(), TranscriptDesync> {
        zeron_doc::apply_transcript_frame(&mut self.transcript, frame)?;
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            let transcript = &self.transcript;
            echoes.retain(|echo| !transcript.iter().any(|e| e.id == echo.id));
        }
        self.ack_pending_send_from_transcript();
        Ok(())
    }

    pub fn sub_transcript(&self, doc_id: &str) -> &[SessionMessageEntry] {
        self.sub_transcripts
            .get(doc_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn watch_subagent_doc(&mut self, doc_id: String, cx: &mut Context<Self>) {
        if self.sub_watch_tasks.contains_key(&doc_id) {
            return;
        }
        let Some(handle) = self.engine.clone() else {
            return;
        };
        self.sub_transcripts.entry(doc_id.clone()).or_default();
        let task = spawn_subagent_watch(cx, handle, doc_id.clone());
        self.sub_watch_tasks.insert(doc_id, task);
    }

    pub fn unwatch_subagent_doc(&mut self, doc_id: &str) {
        self.sub_watch_tasks.remove(doc_id);
        self.sub_transcripts.remove(doc_id);
    }

    pub fn set_subagent_snapshot(&mut self, doc_id: String, entries: Vec<SessionMessageEntry>) {
        self.sub_watch_tasks.remove(&doc_id);
        self.sub_transcripts.insert(doc_id, entries);
    }

    pub fn push_echo(&mut self, chat_id: &str, entry: SessionMessageEntry) {
        let echoes = self.echoes.entry(chat_id.to_string()).or_default();
        if !echoes.iter().any(|e| e.id == entry.id) {
            echoes.push(entry);
        }
    }

    pub fn remove_echo(&mut self, chat_id: &str, message_id: &str) {
        if let Some(echoes) = self.echoes.get_mut(chat_id) {
            echoes.retain(|e| e.id != message_id);
        }
    }

    pub fn begin_pending_send(&mut self, chat_id: &str, message_id: &str, now: DateTime<Utc>) {
        self.pending_sends.insert(
            chat_id.to_string(),
            PendingSend {
                message_id: message_id.to_string(),
                started: now,
            },
        );
    }

    pub fn end_pending_send(&mut self, chat_id: &str, message_id: &str) {
        if self
            .pending_sends
            .get(chat_id)
            .is_some_and(|p| p.message_id == message_id)
        {
            self.pending_sends.remove(chat_id);
        }
    }

    pub fn send_queued_unacked(&self, chat_id: &str, now: DateTime<Utc>) -> bool {
        self.pending_sends.get(chat_id).is_some_and(|p| {
            now.signed_duration_since(p.started).num_milliseconds() > PENDING_SEND_TTL_MS
        })
    }
    pub fn send_pending(&self, chat_id: &str, now: DateTime<Utc>) -> bool {
        self.pending_sends.get(chat_id).is_some_and(|p| {
            now.signed_duration_since(p.started).num_milliseconds() <= PENDING_SEND_TTL_MS
        })
    }

    pub fn pending_send_started(&self, chat_id: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.pending_sends
            .get(chat_id)
            .filter(|p| {
                now.signed_duration_since(p.started).num_milliseconds() <= PENDING_SEND_TTL_MS
            })
            .map(|p| p.started)
    }

    fn ack_pending_send_from_transcript(&mut self) {
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(pending) = self.pending_sends.get(chat_id)
            && self.transcript.iter().any(|e| e.id == pending.message_id)
        {
            self.pending_sends.remove(chat_id);
        }
    }

    pub fn pending_echoes(&self) -> &[SessionMessageEntry] {
        self.selected_chat
            .as_deref()
            .and_then(|id| self.echoes.get(id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn visible_chats(&self) -> impl Iterator<Item = &Chat> {
        self.chats.iter().filter(|c| !c.archived)
    }

    pub fn selected_space_row(&self) -> Option<&Space> {
        if self.no_project {
            return None;
        }
        let id = self.selected_space.as_deref()?;
        self.spaces.iter().find(|s| s.id == id)
    }

    pub fn effective_device_id(&self) -> Option<String> {
        if let Some(space) = self.selected_space_row() {
            return Some(space.device_id.clone());
        }
        self.selected_device
            .clone()
            .or_else(|| self.local_device_id.clone())
    }

    pub fn select_device(&mut self, device_id: String, cx: &mut Context<Self>) {
        let project_moves = self
            .selected_space_row()
            .is_some_and(|s| s.device_id != device_id);
        if project_moves {
            let first = self
                .spaces_sorted()
                .iter()
                .find(|s| s.device_id == device_id)
                .map(|s| s.id.clone());
            self.no_project = first.is_none();
            if first.is_some() {
                self.selected_space = first;
            }
        }
        self.selected_device = Some(device_id);
        cx.notify();
    }

    pub fn space_row(&self, space_id: &str) -> Option<&Space> {
        self.spaces.iter().find(|s| s.id == space_id)
    }

    pub fn spaces_sorted(&self) -> Vec<&Space> {
        let mut spaces: Vec<&Space> = self.spaces.iter().collect();
        spaces.sort_by_key(|s| (s.display_name().to_lowercase(), s.id.clone()));
        spaces
    }

    pub fn space_for_chat(&self, chat: &Chat) -> Option<&Space> {
        self.space_row(chat.space_id.as_deref()?)
    }

    pub fn chats_in_space(&self, space_id: &str) -> Vec<&Chat> {
        let mut chats: Vec<&Chat> = self
            .visible_chats()
            .filter(|c| c.space_id.as_deref() == Some(space_id))
            .collect();
        sort_tabs(&mut chats);
        chats
    }

    pub fn device_name(&self, device_id: &str) -> Option<&str> {
        self.devices
            .iter()
            .find(|d| d.id == device_id)
            .map(|d| d.name.as_str())
    }

    pub fn device_online(&self, device_id: &str, now: DateTime<Utc>) -> bool {
        if self.local_device_id.as_deref() == Some(device_id) {
            return true;
        }
        match self.devices.iter().find(|d| d.id == device_id) {
            Some(d) => crate::settings::devices::device_online(d.last_seen_at, now),
            None => true,
        }
    }

    pub fn space_device_tag(&self, space: &Space, now: DateTime<Utc>) -> (String, bool) {
        let offline = !self.device_online(&space.device_id, now);
        let device = self
            .device_name(&space.device_id)
            .unwrap_or("Unknown device");
        (format!("@ {device}"), offline)
    }

    pub fn selected_space_git(&self) -> bool {
        self.selected_space_row().is_some_and(|s| s.git_detected)
    }

    pub fn display_status_for(&self, chat: &Chat, now: DateTime<Utc>) -> ChatIndicator {
        if self.send_pending(&chat.id, now) {
            return ChatIndicator::Working;
        }
        display_status(chat, self.session_for(&chat.id), now)
    }

    pub fn overview_chats(&self, now: DateTime<Utc>) -> Vec<(ChatIndicator, &Chat)> {
        let mut rows: Vec<(ChatIndicator, &Chat)> = self
            .visible_chats()
            .filter(|c| match c.space_id.as_deref() {
                None => true,
                Some(id) => self.space_row(id).is_some(),
            })
            .map(|c| (self.display_status_for(c, now), c))
            .collect();
        sort_active(&mut rows);
        rows
    }

    pub fn session_for(&self, chat_id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.chat_id == chat_id)
    }

    pub fn indicator_for(&self, chat_id: &str, now: DateTime<Utc>) -> Indicator {
        if self.send_pending(chat_id, now) {
            return Indicator::Working;
        }
        effective_indicator(self.session_for(chat_id), now)
    }

    pub fn selected_chat_row(&self) -> Option<&Chat> {
        let id = self.selected_chat.as_deref()?;
        self.chats.iter().find(|c| c.id == id)
    }

    pub fn gate(&self) -> GatePhase {
        gate_phase(
            &self.connection,
            self.workspace_scope.or(Some(WorkspaceScope::Local)),
            self.auth.as_ref(),
        )
    }

    pub fn engine(&self) -> Option<&EngineHandle> {
        self.engine.as_ref()
    }

    pub fn prepare_runtime_replacement(&mut self, cx: &mut Context<Self>) {
        self.engine = None;
        self.watch_tasks.clear();
        self.transcript_task = None;
        self.connection = ConnectionStatus::Connecting;
        self.workspace_scope = None;
        self.auth = None;
        self.devices.clear();
        self.spaces.clear();
        self.chats.clear();
        self.sessions.clear();
        self.selected_space = None;
        self.no_project = false;
        self.selected_device = None;
        self.selected_chat = None;
        self.auto_selected = false;
        self.chats_synced = false;
        self.spaces_synced = false;
        self.transcript.clear();
        self.echoes.clear();
        self.pending_sends.clear();
        self.local_device_id = None;
        cx.notify();
    }

    pub fn bootstrap(state: Entity<AppState>, config: EngineBootConfig, cx: &mut App) {
        let data_dir = config.data_dir.clone();
        state.update(cx, |s, cx| {
            s.connection = ConnectionStatus::Connecting;
            s.workspace_scope = None;
            s.auth = None;
            s.data_dir = Some(data_dir);
            cx.notify();
        });
        let boot = Tokio::spawn(cx, EngineHandle::bootstrap(config));
        cx.spawn(async move |cx| {
            let outcome = match boot.await {
                Ok(Ok(handle)) => Ok(handle),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            state.update(cx, |s, cx| match outcome {
                Ok(handle) => s.attach_engine(handle, cx),
                Err(message) => {
                    tracing::error!(%message, "engine bootstrap failed");
                    s.connection = ConnectionStatus::Failed(message);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn attach_engine(&mut self, handle: EngineHandle, cx: &mut Context<Self>) {
        let engine_info = handle.engine_info();
        self.workspace_scope = Some(engine_info.workspace_scope);
        self.local_device_id = Some(engine_info.device_id.clone());
        self.engine = Some(handle.clone());
        let mut watch_tasks = Vec::with_capacity(8);
        if let Some(task) = spawn_deferred_engine_watch(cx, handle.clone()) {
            watch_tasks.push(task);
        }
        watch_tasks.extend([
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_SESSIONS,
                AppState::apply_sessions,
            ),
            spawn_chats_watch(cx, handle.clone()),
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_DEVICES,
                AppState::apply_devices,
            ),
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_SPACES,
                AppState::apply_spaces,
            ),
        ]);
        self.watch_tasks = watch_tasks;
        self.connection = ConnectionStatus::Ready;
        if let Some(chat_id) = self.selected_chat.clone() {
            self.transcript_task = Some(spawn_transcript_watch(cx, handle, chat_id));
        }
        cx.notify();
    }

    pub fn select_chat(&mut self, chat_id: Option<String>, cx: &mut Context<Self>) {
        if self.selected_chat == chat_id {
            if let Some(id) = chat_id {
                self.mark_chat_seen(&id, cx);
            }
            return;
        }
        self.selected_chat = chat_id.clone();
        self.auto_selected = true;
        self.transcript.clear();
        self.transcript_task = None;
        if let Some(id) = chat_id.as_deref() {
            if let Some(chat) = self.chats.iter().find(|c| c.id == id) {
                match chat.space_id.clone() {
                    Some(space_id) => {
                        self.selected_space = Some(space_id);
                        self.no_project = false;
                    }
                    None => {
                        self.no_project = true;
                        self.selected_device = Some(chat.device_id.clone());
                    }
                }
            }
            self.mark_chat_seen(id, cx);
        }
        if let (Some(chat_id), Some(handle)) = (chat_id, self.engine.clone()) {
            self.transcript_task = Some(spawn_transcript_watch(cx, handle, chat_id));
        }
        cx.notify();
    }

    pub fn select_space(&mut self, space_id: Option<String>, cx: &mut Context<Self>) {
        match &space_id {
            Some(id) => {
                self.no_project = false;
                if let Some(device) = self.space_row(id).map(|s| s.device_id.clone()) {
                    self.selected_device = Some(device);
                }
            }
            None => self.no_project = true,
        }
        if self.selected_space == space_id && space_id.is_some() {
            cx.notify();
            return;
        }
        if space_id.is_some() {
            self.selected_space = space_id;
        }
        cx.notify();
    }

    pub fn probe_sync(&mut self, cx: &mut Context<Self>) {
        let _ = cx;
    }

    pub fn mark_chat_seen(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) else {
            return;
        };
        if !chat.unseen() {
            return;
        }
        chat.last_seen_at = Some(Utc::now());
        cx.notify();
        let Some(handle) = self.engine.clone() else {
            return;
        };
        let chat_id = chat_id.to_string();
        cx.spawn(async move |_, _| {
            let params = serde_json::json!({ "op": "markChatSeen", "chatId": chat_id });
            if let Err(err) = handle.client().call(methods::MUTATE, params).await {
                tracing::warn!(chat = %chat_id, error = %err, "markChatSeen failed");
            }
        })
        .detach();
    }
}

fn spawn_deferred_engine_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
) -> Option<Task<()>> {
    let mut deferred = handle.deferred_state()?;
    Some(cx.spawn(async move |this, cx| {
        let Err(failure) = wait_for_deferred_engine(&mut deferred).await else {
            return;
        };
        tracing::error!(error = %failure, "engine assembly failed after attachment");
        handle.shutdown().await;
        this.update(cx, |state, cx| {
            state.connection = ConnectionStatus::Failed(failure);
            cx.notify();
        })
        .ok();
    }))
}

fn spawn_chats_watch(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        loop {
            let mut rx = match handle
                .client()
                .subscribe(methods::WATCH_CHATS, serde_json::json!({}))
                .await
            {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::debug!(error = %err, "chats watch unavailable; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue;
                }
            };
            while let Some(value) = rx.recv().await {
                let parsed: Vec<Chat> = match serde_json::from_value(value) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        tracing::warn!(error = %err, "dropping malformed chats frame");
                        continue;
                    }
                };
                let alive = this.update(cx, |state, cx| {
                    state.apply_chats(parsed);
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            tracing::debug!("chats stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

fn spawn_watch<T: DeserializeOwned + 'static>(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    method: &'static str,
    apply: fn(&mut AppState, T),
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        loop {
            let mut rx = match handle
                .client()
                .subscribe(method, serde_json::json!({}))
                .await
            {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::debug!(method, error = %err, "watch unavailable; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue;
                }
            };
            while let Some(value) = rx.recv().await {
                let parsed: T = match serde_json::from_value(value) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        tracing::warn!(method, error = %err, "dropping malformed watch frame");
                        continue;
                    }
                };
                let alive = this.update(cx, |state, cx| {
                    apply(state, parsed);
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            tracing::debug!(method, "watch stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

fn spawn_transcript_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    chat_id: String,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        'resubscribe: loop {
            let params = serde_json::json!({ "chatId": chat_id });
            let mut rx = match handle
                .client()
                .subscribe(methods::WATCH_DOC_MESSAGES, params)
                .await
            {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::warn!(%chat_id, error = %err, "transcript watch failed; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue 'resubscribe;
                }
            };
            while let Some(value) = rx.recv().await {
                let frame: TranscriptFrame = match serde_json::from_value(value) {
                    Ok(frame) => frame,
                    Err(err) => {
                        tracing::warn!(error = %err, "malformed transcript frame; resubscribing");
                        cx.background_executor().timer(RETRY_DELAY).await;
                        continue 'resubscribe;
                    }
                };
                let mut desync = false;
                let alive = this.update(cx, |state, cx| {
                    if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                        if let Err(err) = state.apply_transcript_frame(frame) {
                            tracing::warn!(%chat_id, error = %err, "resubscribing transcript");
                            desync = true;
                        }
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
                if desync {
                    continue 'resubscribe;
                }
            }
            tracing::debug!(%chat_id, "transcript stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

fn spawn_subagent_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    doc_id: String,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        'resubscribe: loop {
            let params = serde_json::json!({ "chatId": doc_id });
            let mut rx = match handle
                .client()
                .subscribe(methods::WATCH_DOC_MESSAGES, params)
                .await
            {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::warn!(%doc_id, error = %err, "subagent watch failed; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue 'resubscribe;
                }
            };
            while let Some(value) = rx.recv().await {
                let frame: TranscriptFrame = match serde_json::from_value(value) {
                    Ok(frame) => frame,
                    Err(err) => {
                        tracing::warn!(error = %err, "malformed subagent frame; resubscribing");
                        cx.background_executor().timer(RETRY_DELAY).await;
                        continue 'resubscribe;
                    }
                };
                let mut desync = false;
                let alive = this.update(cx, |state, cx| {
                    if let Some(rows) = state.sub_transcripts.get_mut(&doc_id) {
                        if let Err(err) = zeron_doc::apply_transcript_frame(rows, frame) {
                            tracing::warn!(%doc_id, error = %err, "resubscribing subagent watch");
                            desync = true;
                        }
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
                if desync {
                    continue 'resubscribe;
                }
            }
            tracing::debug!(%doc_id, "subagent stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use zeron_engine::{EngineCore, default_registry};
    use zeron_proto::{SessionStatus, UserProfile};

    async fn free_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    struct LegacyIdentityRpc;

    #[async_trait]
    impl RpcService for LegacyIdentityRpc {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                methods::LOCAL_DEVICE => {
                    RpcReply::value(&serde_json::json!({ "deviceId": "legacy-device" }))
                }
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    struct DeferredIdentityRpc {
        engine_info: EngineInfo,
        state: tokio::sync::watch::Receiver<DeferredEngineState>,
    }

    #[async_trait]
    impl RpcService for DeferredIdentityRpc {
        async fn handle(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                methods::ENGINE_INFO => RpcReply::value(&self.engine_info),
                methods::ENGINE_READY => {
                    let mut state = self.state.clone();
                    wait_for_deferred_engine(&mut state)
                        .await
                        .map_err(RpcError::Failed)?;
                    RpcReply::value(&serde_json::json!({ "ready": true }))
                }
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    #[tokio::test]
    async fn legacy_daemon_identity_falls_back_to_synced_scope() {
        let client = memory_client(Arc::new(LegacyIdentityRpc));

        let info = query_engine_info(&client).await.unwrap();

        assert_eq!(info.device_id, "legacy-device");
        assert_eq!(info.workspace_scope, WorkspaceScope::Synced);
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(info.workspace_scope),
                Some(&AuthState::SignedOut),
            ),
            GatePhase::SignIn
        );
    }

    #[tokio::test]
    async fn remote_viewport_treats_legacy_daemon_as_ready() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(zeron_rpc::serve_ws_listener(
            listener,
            Arc::new(LegacyIdentityRpc),
        ));
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            default_harness: HarnessId::Mock,
        })
        .await
        .expect("legacy daemon remains attachable");

        let mut deferred = handle
            .deferred_state()
            .expect("remote viewport tracks readiness");
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_deferred_engine(&mut deferred),
        )
        .await
        .expect("legacy readiness fallback completes")
        .expect("unknown EngineReady means the old daemon is assembled");

        handle.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn bootstrap_embeds_engine_when_port_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);
        assert!(matches!(
            handle
                .deferred_state()
                .expect("embedded lifecycle")
                .borrow()
                .clone(),
            DeferredEngineState::Ready
        ));
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn bootstrap_reports_local_assembly_failure_before_returning_a_handle() {
        let dir = tempfile::tempdir().unwrap();
        zeron_engine::EngineProfile::local(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("profiles")).unwrap();
        std::fs::write(dir.path().join("profiles/local"), b"not a directory").unwrap();
        let port = free_port().await;

        let error = match EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            default_harness: HarnessId::Mock,
        })
        .await
        {
            Ok(handle) => {
                handle.shutdown().await;
                panic!("a corrupt local store must fail bootstrap")
            }
            Err(error) => error,
        };

        assert!(!format!("{error:#}").is_empty());
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err(),
            "failed bootstrap must release the IPC listener"
        );
    }

    #[tokio::test]
    async fn deferred_engine_failure_remains_observable_after_early_attach() {
        let (state_tx, mut state_rx) = tokio::sync::watch::channel(DeferredEngineState::Waiting);
        state_tx.send_replace(DeferredEngineState::Failed("store failed".into()));

        assert_eq!(
            wait_for_deferred_engine(&mut state_rx).await,
            Err("store failed".into())
        );
    }

    #[tokio::test]
    async fn remote_viewport_observes_deferred_engine_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (state_tx, state_rx) = tokio::sync::watch::channel(DeferredEngineState::Waiting);
        let server = tokio::spawn(zeron_rpc::serve_ws_listener(
            listener,
            Arc::new(DeferredIdentityRpc {
                engine_info: EngineInfo {
                    device_id: "owner-device".into(),
                    workspace_scope: WorkspaceScope::Local,
                },
                state: state_rx,
            }),
        ));

        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            default_harness: HarnessId::Mock,
        })
        .await
        .expect("second viewport attaches over IPC");
        assert!(matches!(handle.mode(), EngineMode::Remote { .. }));

        let mut deferred = handle
            .deferred_state()
            .expect("remote viewport tracks engine readiness");
        state_tx.send_replace(DeferredEngineState::Failed("store failed".into()));
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                wait_for_deferred_engine(&mut deferred),
            )
            .await
            .expect("remote readiness probe completes"),
            Err("store failed".into())
        );

        handle.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn an_embedded_engine_serves_the_ipc_port_for_other_viewports() {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port().await;
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);

        let attached = connect_ws(&format!("ws://127.0.0.1:{port}"))
            .await
            .expect("a second viewport must be able to attach");
        let harnesses = attached
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));

        handle.shutdown().await;
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err(),
            "the port must be released on shutdown"
        );
    }

    #[tokio::test]
    async fn concurrent_bootstraps_elect_one_embedded_engine() {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port().await;
        let config = EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            default_harness: HarnessId::Mock,
        };
        let (a, b) = tokio::join!(
            EngineHandle::bootstrap(config.clone()),
            EngineHandle::bootstrap(config.clone()),
        );
        let a = a.expect("first viewport boots");
        let b = b.expect("second viewport boots");

        let modes = [a.mode(), b.mode()];
        assert_eq!(
            modes
                .iter()
                .filter(|mode| **mode == EngineMode::InProcess)
                .count(),
            1,
            "exactly one viewport embeds: {modes:?}"
        );
        assert_eq!(
            modes
                .iter()
                .filter(|mode| matches!(mode, EngineMode::Remote { .. }))
                .count(),
            1,
            "the other attaches over IPC: {modes:?}"
        );

        for handle in [&a, &b] {
            let mut deferred = handle.deferred_state().expect("lifecycle tracked");
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                wait_for_deferred_engine(&mut deferred),
            )
            .await
            .expect("readiness resolves")
            .expect("both viewports reach Ready");
        }

        b.shutdown().await;
        a.shutdown().await;
    }

    #[tokio::test]
    async fn a_stranger_on_the_ipc_port_does_not_wedge_the_window() {
        let squatter = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = squatter.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            default_harness: HarnessId::Mock,
        })
        .await
        .expect("a taken port must not fail the boot");
        assert_eq!(handle.mode(), EngineMode::InProcess);
        assert!(
            handle
                .client()
                .call(methods::LIST_HARNESSES, serde_json::json!({}))
                .await
                .is_ok(),
            "the window still works over its own transport"
        );
        handle.shutdown().await;
        drop(squatter);
    }

    #[tokio::test]
    async fn production_bootstrap_opens_local_data_without_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();

        assert_eq!(handle.engine_info().workspace_scope, WorkspaceScope::Local);
        let info: EngineInfo = handle
            .client()
            .call_as(methods::ENGINE_INFO, serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(info, *handle.engine_info());

        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .expect("local data RPC is immediately available");
        assert!(harnesses.as_array().is_some_and(|items| !items.is_empty()));
        assert!(
            !dir.path().join("orgs/dev-org/dev-user").exists(),
            "production boot must not create dev-user data"
        );
        assert!(dir.path().join("profiles/local").is_dir());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn local_boot_ignores_legacy_cloud_session() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("session.json"),
            r#"{"refreshToken":"saved","user":{"id":"user_1","email":"u@example.com"}}"#,
        )
        .unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();

        assert!(matches!(
            handle
                .deferred_state()
                .expect("embedded lifecycle")
                .borrow()
                .clone(),
            DeferredEngineState::Ready
        ));

        let info: EngineInfo = handle
            .client()
            .call_as(methods::ENGINE_INFO, serde_json::json!({}))
            .await
            .expect("local EngineInfo is available after assembly");
        assert_eq!(info.workspace_scope, WorkspaceScope::Local);
        assert!(
            handle
                .client()
                .call(methods::LIST_HARNESSES, serde_json::json!({}))
                .await
                .is_ok()
        );
        assert!(!dir.path().join("orgs").exists());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn bootstrap_connects_when_daemon_is_listening() {
        let daemon_dir = tempfile::tempdir().unwrap();
        let core = EngineCore::assemble(
            daemon_dir.path(),
            Arc::new(default_registry()),
            HarnessId::Mock,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(zeron_rpc::serve_ws_listener(listener, core.rpc_service()));

        let ui_dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: ui_dir.path().to_path_buf(),
            ipc_port: port,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(
            handle.mode(),
            EngineMode::Remote {
                url: format!("ws://127.0.0.1:{port}")
            }
        );
        assert_eq!(handle.engine_info().workspace_scope, WorkspaceScope::Local);
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
        assert!(matches!(
            handle
                .client()
                .call(methods::STOP_ENGINE, serde_json::json!({}))
                .await,
            Err(RpcError::Failed(message))
                if message == format!("unknown method: {}", methods::STOP_ENGINE)
        ));
    }

    fn chat(id: &str, created_min: i64, last_msg_min: Option<i64>) -> Chat {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: None,
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: last_msg_min.map(|m| base + TimeDelta::minutes(m)),
            created_at: base + TimeDelta::minutes(created_min),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
        }
    }

    fn space(id: &str, device_id: &str, path: &str, created_min: i64) -> Space {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Space {
            id: id.into(),
            device_id: device_id.into(),
            path: path.into(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: base + TimeDelta::minutes(created_min),
        }
    }

    fn session(
        chat_id: &str,
        status: SessionStatus,
        updated_secs_ago: i64,
        now: DateTime<Utc>,
    ) -> Session {
        Session {
            chat_id: chat_id.into(),
            device_id: "dev".into(),
            status,
            started_at: None,
            updated_at: now - TimeDelta::seconds(updated_secs_ago),
        }
    }

    fn user_entry(id: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: zeron_doc::MessageRole::User,
            parts: Vec::new(),
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn device(id: &str, name: &str) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            platform: "macos".into(),
            last_seen_at: None,
            created_at: None,
            version: None,
        }
    }

    #[test]
    fn local_workspace_hides_the_unknown_device_sentinel() {
        let mut state = AppState::new();
        state.workspace_scope = Some(WorkspaceScope::Local);
        state.local_device_id = Some("local".into());

        state.apply_devices(vec![
            device("local", "unknown-device"),
            device("remote", "unknown-device"),
        ]);

        assert_eq!(state.device_name("local"), Some("Local"));
        assert_eq!(state.device_name("remote"), Some("unknown-device"));

        state.apply_devices(vec![device("local", "José's MacBook Pro")]);
        assert_eq!(state.device_name("local"), Some("José's MacBook Pro"));
    }

    #[test]
    fn queued_unacked_takes_over_after_the_ttl() {
        let now = Utc::now();
        let mut s = AppState::new();
        assert!(!s.send_queued_unacked("c", now), "no send, no queued line");
        s.begin_pending_send("c", "m1", now);
        assert!(s.send_pending("c", now));
        assert!(!s.send_queued_unacked("c", now));
        let later = now + TimeDelta::milliseconds(PENDING_SEND_TTL_MS + 1);
        assert!(!s.send_pending("c", later));
        assert!(s.send_queued_unacked("c", later));
        s.end_pending_send("c", "m1");
        assert!(!s.send_queued_unacked("c", later));
    }

    #[test]
    fn send_pending_overlays_working_until_ttl() {
        let now = Utc::now();
        let s_chat = chat("c", 0, Some(10));
        let mut s = AppState::new();
        assert_eq!(s.display_status_for(&s_chat, now), ChatIndicator::Completed);
        assert_eq!(s.indicator_for("c", now), Indicator::None);
        s.begin_pending_send("c", "m1", now);
        assert_eq!(s.display_status_for(&s_chat, now), ChatIndicator::Working);
        assert_eq!(s.indicator_for("c", now), Indicator::Working);
        let later = now + TimeDelta::milliseconds(PENDING_SEND_TTL_MS + 1);
        assert_eq!(
            s.display_status_for(&s_chat, later),
            ChatIndicator::Completed
        );
        assert_eq!(s.indicator_for("c", later), Indicator::None);
    }

    #[test]
    fn send_pending_acked_when_the_host_writes_the_message_back() {
        let now = Utc::now();
        let mut s = AppState::new();
        s.selected_chat = Some("c".into());
        s.begin_pending_send("c", "m1", now);
        s.apply_transcript(vec![user_entry("other")]);
        assert!(s.send_pending("c", now));
        s.apply_transcript(vec![user_entry("other"), user_entry("m1")]);
        assert!(!s.send_pending("c", now));
    }

    #[test]
    fn send_failure_cleanup_only_ends_its_own_overlay() {
        let now = Utc::now();
        let mut s = AppState::new();
        s.begin_pending_send("c", "m1", now);
        s.begin_pending_send("c", "m2", now);
        s.end_pending_send("c", "m1");
        assert!(s.send_pending("c", now), "m2's overlay must survive");
        s.end_pending_send("c", "m2");
        assert!(!s.send_pending("c", now));
    }

    #[test]
    fn chats_sort_by_last_message_desc_with_created_fallback() {
        let mut chats = vec![
            chat("a", 0, Some(10)),
            chat("b", 5, None),
            chat("c", 1, Some(30)),
            chat("d", 40, None),
        ];
        sort_chats(&mut chats);
        let order: Vec<&str> = chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["d", "c", "a", "b"]);
    }

    #[test]
    fn chat_sort_ties_are_deterministic() {
        let mut chats = vec![chat("z", 0, Some(10)), chat("a", 0, Some(10))];
        sort_chats(&mut chats);
        assert_eq!(chats[0].id, "a");
    }

    #[test]
    fn working_indicator_staleness() {
        let now = Utc::now();
        let fresh = session("c", SessionStatus::Working, 10, now);
        assert_eq!(effective_indicator(Some(&fresh), now), Indicator::Working);
        let stale = session("c", SessionStatus::Working, 46, now);
        assert_eq!(effective_indicator(Some(&stale), now), Indicator::None);
        let edge = session("c", SessionStatus::Working, 45, now);
        assert_eq!(effective_indicator(Some(&edge), now), Indicator::Working);
        let skewed = session("c", SessionStatus::Working, -30, now);
        assert_eq!(effective_indicator(Some(&skewed), now), Indicator::Working);
    }

    #[test]
    fn indicator_kinds() {
        let now = Utc::now();
        assert_eq!(effective_indicator(None, now), Indicator::None);
        let idle = session("c", SessionStatus::Idle, 0, now);
        assert_eq!(effective_indicator(Some(&idle), now), Indicator::None);
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(effective_indicator(Some(&errored), now), Indicator::Errored);
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            effective_indicator(Some(&awaiting), now),
            Indicator::AwaitingInput
        );
        let awaiting_stale = session("c", SessionStatus::AwaitingInput, 300, now);
        assert_eq!(
            effective_indicator(Some(&awaiting_stale), now),
            Indicator::None
        );
    }

    #[test]
    fn display_status_derivation() {
        let now = Utc::now();
        let mut c = chat("c", 0, Some(10));
        let working = session("c", SessionStatus::Working, 5, now);
        assert_eq!(
            display_status(&c, Some(&working), now),
            ChatIndicator::Working
        );
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            display_status(&c, Some(&awaiting), now),
            ChatIndicator::AwaitingInput
        );
        assert_eq!(display_status(&c, None, now), ChatIndicator::Completed);
        let idle = session("c", SessionStatus::Idle, 5, now);
        assert_eq!(
            display_status(&c, Some(&idle), now),
            ChatIndicator::Completed
        );
        let stale = session("c", SessionStatus::Working, 300, now);
        assert_eq!(
            display_status(&c, Some(&stale), now),
            ChatIndicator::Completed
        );
        c.last_seen_at = c.last_message_at.map(|t| t + TimeDelta::minutes(1));
        assert_eq!(display_status(&c, Some(&idle), now), ChatIndicator::Idle);
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(display_status(&c, Some(&errored), now), ChatIndicator::Idle);
        c.last_seen_at = None;
        assert_eq!(
            display_status(&c, Some(&errored), now),
            ChatIndicator::Errored
        );
        let fresh = chat("f", 0, None);
        assert_eq!(display_status(&fresh, None, now), ChatIndicator::Idle);
    }

    #[test]
    fn active_list_sorts_by_recency_only_status_never_moves_rows() {
        let a = chat("a", 0, Some(10));
        let b = chat("b", 0, Some(20));
        let c = chat("c", 0, Some(5));
        let d = chat("d", 0, Some(1));
        let mut rows = vec![
            (ChatIndicator::Completed, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut rows);
        let order: Vec<&str> = rows.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a", "c", "d"], "recency desc, status ignored");

        let mut seen = vec![
            (ChatIndicator::Idle, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut seen);
        let order_after: Vec<&str> = seen.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, order_after);
    }

    #[test]
    fn tabs_order_by_creation_not_activity() {
        let a = chat("a", 5, Some(100));
        let b = chat("b", 1, Some(2));
        let mut tabs = vec![&a, &b];
        sort_tabs(&mut tabs);
        let order: Vec<&str> = tabs.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a"]);
    }

    #[test]
    fn apply_spaces_sorts_and_heals_selection() {
        let mut state = AppState::new();
        state.apply_spaces(vec![
            space("s2", "dev", "/b", 2),
            space("s1", "dev", "/a", 1),
        ]);
        let ids: Vec<&str> = state.spaces.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["s1", "s2"]);
        assert_eq!(state.selected_space.as_deref(), Some("s1"));
        state.selected_space = Some("s2".into());
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        assert_eq!(state.selected_space.as_deref(), Some("s1"));
        state.apply_spaces(vec![]);
        assert_eq!(state.selected_space, None);
    }

    #[test]
    fn chats_in_space_filters_and_orders() {
        let mut state = AppState::new();
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        let mut in_space_new = chat("new", 5, None);
        in_space_new.space_id = Some("s1".into());
        let mut in_space_old = chat("old", 1, Some(50));
        in_space_old.space_id = Some("s1".into());
        let mut other = chat("other", 2, None);
        other.space_id = Some("s2".into());
        let mut archived = chat("gone", 0, None);
        archived.space_id = Some("s1".into());
        archived.archived = true;
        let dangling = chat("dangling", 3, None);
        state.apply_chats(vec![in_space_new, in_space_old, other, archived, dangling]);
        let ids: Vec<&str> = state
            .chats_in_space("s1")
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, ["old", "new"]);
        let now = Utc::now();
        let overview: Vec<&str> = state
            .overview_chats(now)
            .iter()
            .map(|(_, c)| c.id.as_str())
            .collect();
        assert_eq!(overview, ["old", "new", "dangling"]);
    }

    #[test]
    fn apply_chats_drops_vanished_selection() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        state.selected_chat = Some("a".into());
        state.transcript = vec![];
        state.apply_chats(vec![chat("b", 1, None)]);
        assert_eq!(state.selected_chat, None);
        state.selected_chat = Some("b".into());
        state.apply_chats(vec![chat("b", 1, None), chat("c", 2, None)]);
        assert_eq!(state.selected_chat.as_deref(), Some("b"));
    }

    #[test]
    fn apply_chat_config_stamps_the_row() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        let config = zeron_proto::ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: Some("claude-fable-5".into()),
            reasoning: Some(zeron_proto::ReasoningLevel::XHigh),
            model_options: serde_json::Map::new(),
            sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
        };
        state.apply_chat_config("a", config.clone());
        assert_eq!(
            state.chats.iter().find(|c| c.id == "a").unwrap().config,
            Some(config)
        );
        assert!(
            state
                .chats
                .iter()
                .find(|c| c.id == "b")
                .unwrap()
                .config
                .is_none()
        );
        state.apply_chat_config(
            "missing",
            zeron_proto::ChatConfig {
                harness: HarnessId::ClaudeCode,
                model: None,
                reasoning: None,
                model_options: serde_json::Map::new(),
                sandbox: zeron_proto::SandboxLevel::WorkspaceWrite,
            },
        );
    }

    #[test]
    fn visible_chats_filters_archived() {
        let mut state = AppState::new();
        let mut archived = chat("a", 0, Some(99));
        archived.archived = true;
        state.apply_chats(vec![archived, chat("b", 1, None)]);
        let visible: Vec<&str> = state.visible_chats().map(|c| c.id.as_str()).collect();
        assert_eq!(visible, ["b"]);
    }

    #[test]
    fn echoes_show_until_doc_frame_confirms() {
        let mut state = AppState::new();
        state.selected_chat = Some("c1".into());
        let echo = SessionMessageEntry {
            id: "m1".into(),
            role: zeron_doc::MessageRole::User,
            parts: vec![],
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        };
        state.push_echo("c1", echo.clone());
        state.push_echo("c1", echo.clone());
        assert_eq!(state.pending_echoes().len(), 1);
        state.apply_transcript(vec![]);
        assert_eq!(state.pending_echoes().len(), 1);
        state.apply_transcript(vec![SessionMessageEntry {
            id: "m1".into(),
            ..echo.clone()
        }]);
        assert!(state.pending_echoes().is_empty());
        state.push_echo(
            "c1",
            SessionMessageEntry {
                id: "m2".into(),
                ..echo.clone()
            },
        );
        state.remove_echo("c1", "m2");
        assert!(state.pending_echoes().is_empty());
        state.push_echo(
            "other",
            SessionMessageEntry {
                id: "m3".into(),
                ..echo
            },
        );
        assert!(state.pending_echoes().is_empty());
    }

    #[test]
    fn gate_phases() {
        let user = UserProfile {
            id: "u".into(),
            email: "w@example.com".into(),
            name: None,
        };
        assert_eq!(
            gate_phase(&ConnectionStatus::Connecting, None, None),
            GatePhase::Loading
        );
        assert_eq!(
            gate_phase(&ConnectionStatus::Failed("boom".into()), None, None),
            GatePhase::Failed("boom".into())
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(WorkspaceScope::Local),
                Some(&AuthState::SignedOut),
            ),
            GatePhase::Ready
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(WorkspaceScope::Synced),
                Some(&AuthState::SignedOut),
            ),
            GatePhase::SignIn
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(WorkspaceScope::Synced),
                Some(&AuthState::SignedIn {
                    user: user.clone(),
                    org_id: Some("org-1".into()),
                }),
            ),
            GatePhase::Ready
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(WorkspaceScope::Synced),
                Some(&AuthState::NeedsOrganization { user })
            ),
            GatePhase::OrgGate
        );
    }

    #[test]
    fn auth_changes_do_not_change_a_local_runtime_scope_or_watches() {
        let mut state = AppState::new();
        state.workspace_scope = Some(WorkspaceScope::Local);
        state.watch_tasks.push(Task::ready(()));

        state.apply_auth(AuthState::NeedsOrganization {
            user: UserProfile {
                id: "u".into(),
                email: "w@example.com".into(),
                name: None,
            },
        });
        assert_eq!(state.workspace_scope, Some(WorkspaceScope::Local));
        assert_eq!(state.watch_tasks.len(), 1);

        state.apply_auth(AuthState::SignedIn {
            user: UserProfile {
                id: "u".into(),
                email: "w@example.com".into(),
                name: None,
            },
            org_id: Some("org-1".into()),
        });
        assert_eq!(state.workspace_scope, Some(WorkspaceScope::Local));
        assert_eq!(state.watch_tasks.len(), 1);
    }

    #[test]
    fn auth_frames_parse_both_wire_shapes() {
        let proto = serde_json::json!({ "state": "signedOut" });
        assert_eq!(parse_auth_state(&proto), Some(AuthState::SignedOut));
        let engine = serde_json::json!({
            "_tag": "SignedIn",
            "user": { "id": "u1", "email": "w@example.com" },
            "orgId": "org-1",
        });
        let Some(AuthState::SignedIn { user, org_id }) = parse_auth_state(&engine) else {
            panic!("expected SignedIn");
        };
        assert_eq!(user.email, "w@example.com");
        assert_eq!(org_id.as_deref(), Some("org-1"));
        let needs = serde_json::json!({
            "_tag": "NeedsOrganization",
            "user": { "id": "u1", "email": "w@example.com", "name": "W" },
        });
        assert!(matches!(
            parse_auth_state(&needs),
            Some(AuthState::NeedsOrganization { .. })
        ));
        assert_eq!(
            parse_auth_state(&serde_json::json!({ "_tag": "Wat" })),
            None
        );
        assert_eq!(parse_auth_state(&serde_json::json!(42)), None);
    }

    fn chat_with_cwd(id: &str, created_min: i64, cwd: Option<&str>) -> Chat {
        let mut c = chat(id, created_min, None);
        c.cwd = cwd.map(str::to_string);
        c
    }

    #[test]
    fn project_labels_from_cwd() {
        assert_eq!(project_label(Some("/home/w/dev/zeron")), "zeron");
        assert_eq!(project_label(Some("/home/w/dev/zeron/")), "zeron");
        assert_eq!(project_label(None), "No project");
        assert_eq!(project_label(Some("   ")), "No project");
        assert_eq!(project_label(Some("/")), "/");
    }

    #[test]
    fn grouped_sidebar_preserves_recency_order() {
        let chats = [
            chat_with_cwd("a", 9, Some("/dev/zeron")),
            chat_with_cwd("b", 8, Some("/dev/zed")),
            chat_with_cwd("c", 7, Some("/dev/zeron")),
            chat_with_cwd("d", 6, None),
        ];
        let groups = group_chats(chats.iter());
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["zeron", "zed", "No project"]);
        let zeron_ids: Vec<&str> = groups[0].chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(zeron_ids, ["a", "c"]);
        assert!(group_chats(std::iter::empty()).is_empty());
    }

    #[test]
    fn relative_times_match_zeron_format() {
        let now = Utc::now();
        let ago = |secs: i64| now - chrono::Duration::seconds(secs);
        assert_eq!(format_time_ago(ago(0), now), "now");
        assert_eq!(format_time_ago(ago(59), now), "now");
        assert_eq!(format_time_ago(ago(60), now), "1m");
        assert_eq!(format_time_ago(ago(59 * 60), now), "59m");
        assert_eq!(format_time_ago(ago(60 * 60), now), "1h");
        assert_eq!(format_time_ago(ago(23 * 3600 + 3599), now), "23h");
        assert_eq!(format_time_ago(ago(24 * 3600), now), "1d");
        assert_eq!(format_time_ago(ago(6 * 86400), now), "6d");
        assert_eq!(format_time_ago(ago(7 * 86400), now), "1w");
        assert_eq!(format_time_ago(ago(30 * 86400), now), "4w");
        assert_eq!(format_time_ago(ago(35 * 86400), now), "1mo");
        assert_eq!(format_time_ago(ago(400 * 86400), now), "1y");
        assert_eq!(
            format_time_ago(now + chrono::Duration::hours(2), now),
            "now"
        );
    }

    #[test]
    fn chat_location_joins_project_and_branch() {
        let mut c = chat_with_cwd("x", 1, Some("/home/w/dev/soccertcg"));
        c.branch = Some("zeron/rebalance".into());
        assert_eq!(
            chat_location(&c).as_deref(),
            Some("soccertcg · zeron/rebalance")
        );
        c.branch = None;
        assert_eq!(chat_location(&c).as_deref(), Some("soccertcg"));
        c.cwd = None;
        c.branch = Some("main".into());
        assert_eq!(chat_location(&c).as_deref(), Some("main"));
        c.branch = Some("   ".into());
        assert_eq!(chat_location(&c), None);
        c.branch = None;
        assert_eq!(chat_location(&c), None);
    }

    #[test]
    fn org_gate_reducers() {
        assert!(org_name_valid("Acme"));
        assert!(org_name_valid("  padded  "));
        assert!(!org_name_valid(""));
        assert!(!org_name_valid("   "));
        assert!(!org_name_valid(&"x".repeat(65)));

        let rows = parse_orgs(&serde_json::json!({ "orgs": [
            { "id": "m2", "organizationId": "o2", "name": "beta" },
            { "id": "m1", "organizationId": "o1", "name": "Alpha" },
            { "id": "m3", "organizationId": "o1", "name": "Alpha" },
        ]}));
        assert_eq!(rows.len(), 3);
        let sorted = sort_memberships(rows);
        let names: Vec<&str> = sorted.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(
            names,
            ["Alpha", "beta"],
            "case-insensitive sort + dedupe by org id"
        );
        assert_eq!(
            parse_orgs(&serde_json::json!([{ "id": "m", "organizationId": "o", "name": "n" }]))
                .len(),
            1
        );
        assert!(parse_orgs(&serde_json::json!("nope")).is_empty());
    }
}
