use chrono::{DateTime, Utc};

use zeron_proto::{AuthState, Chat, ChatIndicator, Space, WorkspaceScope};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connecting,
    Ready,
    Failed(String),
}

pub fn attention_rank(status: ChatIndicator) -> u8 {
    match status {
        ChatIndicator::AwaitingInput => 0,
        ChatIndicator::Errored => 1,
        ChatIndicator::Working => 2,
        ChatIndicator::Completed => 3,
        ChatIndicator::Idle => 4,
    }
}

pub fn sort_active(rows: &mut Vec<(ChatIndicator, &Chat)>) {
    rows.sort_by(|(_, a), (_, b)| {
        let ka = a.last_message_at.unwrap_or(a.created_at);
        let kb = b.last_message_at.unwrap_or(b.created_at);
        kb.cmp(&ka).then_with(|| a.id.cmp(&b.id))
    });
}

pub fn sort_tabs(chats: &mut [&Chat]) {
    chats.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
}

pub fn sort_spaces(spaces: &mut [Space]) {
    spaces.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
}

pub fn sort_chats(chats: &mut [Chat]) {
    chats.sort_by(|a, b| {
        let ka = a.last_message_at.unwrap_or(a.created_at);
        let kb = b.last_message_at.unwrap_or(b.created_at);
        kb.cmp(&ka)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatePhase {
    Loading,
    Failed(String),
    SignIn,
    OrgGate,
    Ready,
}

pub fn gate_phase(
    connection: &ConnectionStatus,
    workspace_scope: Option<WorkspaceScope>,
    auth: Option<&AuthState>,
) -> GatePhase {
    match connection {
        ConnectionStatus::Connecting => GatePhase::Loading,
        ConnectionStatus::Failed(err) => GatePhase::Failed(err.clone()),
        ConnectionStatus::Ready => match workspace_scope.unwrap_or(WorkspaceScope::Synced) {
            WorkspaceScope::Local | WorkspaceScope::Development => GatePhase::Ready,
            WorkspaceScope::Synced => match auth {
                Some(AuthState::NeedsOrganization { .. }) => GatePhase::OrgGate,
                Some(AuthState::SignedIn { .. }) => GatePhase::Ready,
                Some(AuthState::SignedOut) | None => GatePhase::SignIn,
            },
        },
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use zeron_proto::UserProfile;

    fn user() -> UserProfile {
        UserProfile {
            id: "user-1".into(),
            email: "user@example.com".into(),
            name: None,
        }
    }

    #[test]
    fn workspace_scope_controls_the_auth_gate() {
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
                Some(&AuthState::NeedsOrganization { user: user() }),
            ),
            GatePhase::OrgGate
        );
    }

    #[test]
    fn development_and_local_never_use_the_workos_gate() {
        for scope in [WorkspaceScope::Local, WorkspaceScope::Development] {
            for auth in [
                AuthState::SignedOut,
                AuthState::NeedsOrganization { user: user() },
                AuthState::SignedIn {
                    user: user(),
                    org_id: Some("org-1".into()),
                },
            ] {
                assert_eq!(
                    gate_phase(&ConnectionStatus::Ready, Some(scope), Some(&auth)),
                    GatePhase::Ready
                );
            }
        }
    }

    #[test]
    fn missing_scope_falls_back_to_a_synced_gate() {
        assert_eq!(
            gate_phase(&ConnectionStatus::Ready, None, None),
            GatePhase::SignIn
        );
    }
}

pub fn parse_auth_state(value: &serde_json::Value) -> Option<AuthState> {
    if let Ok(state) = serde_json::from_value::<AuthState>(value.clone()) {
        return Some(state);
    }
    let tag = value.get("_tag").and_then(|t| t.as_str())?;
    let user = || -> Option<zeron_proto::UserProfile> {
        let u = value.get("user")?;
        Some(zeron_proto::UserProfile {
            id: u.get("id")?.as_str()?.to_string(),
            email: u.get("email")?.as_str()?.to_string(),
            name: u.get("name").and_then(|n| n.as_str()).map(str::to_string),
        })
    };
    match tag {
        "SignedOut" => Some(AuthState::SignedOut),
        "NeedsOrganization" => Some(AuthState::NeedsOrganization { user: user()? }),
        "SignedIn" => Some(AuthState::SignedIn {
            user: user()?,
            org_id: value
                .get("orgId")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatGroup<'a> {
    pub label: String,
    pub chats: Vec<&'a Chat>,
}

pub fn project_label(cwd: Option<&str>) -> String {
    let Some(cwd) = cwd.map(str::trim).filter(|c| !c.is_empty()) else {
        return "No project".to_string();
    };
    std::path::Path::new(cwd.trim_end_matches(['/', '\\']))
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| cwd.to_string())
}

pub fn group_chats<'a>(chats: impl IntoIterator<Item = &'a Chat>) -> Vec<ChatGroup<'a>> {
    let mut groups: Vec<ChatGroup<'a>> = Vec::new();
    for chat in chats {
        let label = project_label(chat.cwd.as_deref());
        match groups.iter_mut().find(|g| g.label == label) {
            Some(group) => group.chats.push(chat),
            None => groups.push(ChatGroup {
                label,
                chats: vec![chat],
            }),
        }
    }
    groups
}

pub fn format_time_ago(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let s = now.signed_duration_since(then).num_seconds().max(0);
    if s < 60 {
        return "now".to_string();
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h");
    }
    let d = h / 24;
    if d < 7 {
        return format!("{d}d");
    }
    let w = d / 7;
    if w < 5 {
        return format!("{w}w");
    }
    let mo = d / 30;
    if mo < 12 {
        return format!("{mo}mo");
    }
    format!("{}y", d / 365)
}

pub fn chat_location(chat: &Chat) -> Option<String> {
    let project = chat
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| project_label(Some(c)));
    let reference = chat
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty());
    match (project, reference) {
        (Some(p), Some(r)) => Some(format!("{p} · {r}")),
        (Some(p), None) => Some(p),
        (None, Some(r)) => Some(r.to_string()),
        (None, None) => None,
    }
}

pub fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

pub fn tool_chip_content(call: &zeron_proto::ToolCall) -> (&'static str, String) {
    let (label, detail) = tool_chip_content_raw(call);
    (label, single_line(&detail))
}

fn tool_chip_content_raw(call: &zeron_proto::ToolCall) -> (&'static str, String) {
    use zeron_proto::ToolCall;
    match call {
        ToolCall::Exec { command } => ("Run", command.clone()),
        ToolCall::ReadFile { path } => ("Read", path.clone()),
        ToolCall::WriteFile { path, .. } => ("Write", path.clone()),
        ToolCall::EditFile { path, .. } => ("Edit", path.clone()),
        ToolCall::ApplyPatch { path } => {
            ("Patch", path.clone().unwrap_or_else(|| "workspace".into()))
        }
        ToolCall::Search { pattern, path } => (
            "Search",
            match path {
                Some(path) => format!("{pattern} in {path}"),
                None => pattern.clone(),
            },
        ),
        ToolCall::Glob { pattern } => ("Glob", pattern.clone()),
        ToolCall::WebFetch { url, .. } => ("Fetch", url.clone()),
        ToolCall::WebSearch { query } => ("Web", query.clone()),
        ToolCall::Todo { items } => {
            let done = items.iter().filter(|i| i.done).count();
            ("Todo", format!("{done}/{} done", items.len()))
        }
        ToolCall::Mcp { server, tool, .. } => ("MCP", format!("{server} · {tool}")),
        ToolCall::Unknown { name, .. } => match name.strip_prefix("Agent: ") {
            Some(description) => ("Agent", description.to_owned()),
            None if name == "Agent" => ("Agent", String::new()),
            None => ("Tool", name.clone()),
        },
    }
}

pub fn tool_group_summary(tools: &[(zeron_proto::ToolCall, bool)]) -> String {
    use zeron_proto::ToolCall;
    let mut commands = 0usize;
    let mut edited: Vec<&str> = Vec::new();
    let mut reads = 0usize;
    let mut searches = 0usize;
    let mut fetches = 0usize;
    let mut todos = 0usize;
    let mut other = 0usize;
    let mut failed = 0usize;
    for (call, is_error) in tools {
        if *is_error {
            failed += 1;
        }
        match call {
            ToolCall::Exec { .. } => commands += 1,
            ToolCall::WriteFile { path, .. } | ToolCall::EditFile { path, .. } => {
                if !edited.contains(&path.as_str()) {
                    edited.push(path);
                }
            }
            ToolCall::ApplyPatch { path } => {
                let p = path.as_deref().unwrap_or("patch");
                if !edited.contains(&p) {
                    edited.push(p);
                }
            }
            ToolCall::ReadFile { .. } => reads += 1,
            ToolCall::Search { .. } | ToolCall::Glob { .. } | ToolCall::WebSearch { .. } => {
                searches += 1
            }
            ToolCall::WebFetch { .. } => fetches += 1,
            ToolCall::Todo { .. } => todos += 1,
            ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => other += 1,
        }
    }
    let mut segments: Vec<String> = Vec::new();
    if commands > 0 {
        segments.push(format!("ran {}", plural(commands, "command", "commands")));
    }
    if !edited.is_empty() {
        segments.push(format!("edited {}", plural(edited.len(), "file", "files")));
    }
    if reads > 0 {
        segments.push(format!("read {}", plural(reads, "file", "files")));
    }
    if searches > 0 {
        segments.push(format!("searched {}", plural(searches, "time", "times")));
    }
    if fetches > 0 {
        segments.push(format!("fetched {}", plural(fetches, "page", "pages")));
    }
    if todos > 0 {
        segments.push("updated todos".to_string());
    }
    if other > 0 {
        segments.push(format!("called {}", plural(other, "tool", "tools")));
    }
    if segments.is_empty() {
        segments.push(plural(tools.len(), "tool", "tools"));
    }
    if failed > 0 {
        segments.push(format!("{failed} failed"));
    }
    let mut summary = segments.join(" · ");
    if let Some(first) = summary.get(0..1) {
        let upper = first.to_uppercase();
        summary.replace_range(0..1, &upper);
    }
    summary
}

pub mod dot {
    pub const WORKING: (f32, f32, f32) = (0.718, 0.202, 349.761);
    pub const AWAITING: (f32, f32, f32) = (0.673, 0.182, 276.935);
    pub const ERRORED: (f32, f32, f32) = (0.704, 0.191, 22.216);
    pub const COMPLETED: (f32, f32, f32) = (0.765, 0.177, 163.223);
}
