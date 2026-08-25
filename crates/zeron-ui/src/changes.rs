use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, ListAlignment, ListState,
    SharedString, Subscription, Task, Window, div, font, list, prelude::*, px,
};

use zeron_proto::{Chat, CheckoutDiff, GitHistoryCommit};
use zeron_rpc::methods;

use crate::comments::{self, CommentSide, DiffComment};
use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::history::{GitHistory, GitHistoryCount, GitHistoryEvent, GitHistoryFetchButton};
use crate::markdown::render;
use crate::motion::{self, AnimationExt as _, CHEVRON, COLLAPSE};
use crate::popover::{self, Popup};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;
use onyx_syntax::LanguageId as Lang;

pub const FILE_HEADER_HEIGHT: f32 = 36.0;
pub const HUNK_HEADER_HEIGHT: f32 = 28.0;
pub const DIFF_LINE_HEIGHT: f32 = 21.0;
pub const NOTICE_HEIGHT: f32 = 24.0;
pub const BODY_BOTTOM_PAD: f32 = 8.0;
pub const GUTTER_WIDTH: f32 = 36.0;
pub const MARKER_WIDTH: f32 = 28.0;
pub const ACCENT_BAR_WIDTH: f32 = 3.0;
const DIFF_TEXT_SIZE: f32 = 12.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Del,
    Meta,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceSide {
    Old,
    New,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceLineRef {
    pub side: SourceSide,
    pub line_number: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffHighlights {
    pub old: Option<Arc<onyx_syntax::HighlightedDocument>>,
    pub new: Option<Arc<onyx_syntax::HighlightedDocument>>,
}

impl DiffHighlights {
    pub fn source_ref(&self, line: &DiffLine) -> Option<SourceLineRef> {
        match line.kind {
            LineKind::Del => line.old_no.map(|line_number| SourceLineRef {
                side: SourceSide::Old,
                line_number,
            }),
            LineKind::Add => line.new_no.map(|line_number| SourceLineRef {
                side: SourceSide::New,
                line_number,
            }),
            LineKind::Context => line
                .new_no
                .filter(|_| self.new.is_some())
                .map(|line_number| SourceLineRef {
                    side: SourceSide::New,
                    line_number,
                })
                .or_else(|| {
                    line.old_no.map(|line_number| SourceLineRef {
                        side: SourceSide::Old,
                        line_number,
                    })
                }),
            LineKind::Meta => None,
        }
    }

    pub fn spans(&self, line: &DiffLine) -> &[onyx_syntax::HighlightSpan] {
        let Some(source_ref) = self.source_ref(line) else {
            return &[];
        };
        let document = match source_ref.side {
            SourceSide::Old => self.old.as_deref(),
            SourceSide::New => self.new.as_deref(),
        };
        document
            .and_then(|document| document.lines.get(source_ref.line_number as usize - 1))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub binary: bool,
    pub notices: Vec<String>,
    pub hunks: Vec<Hunk>,
    pub additions: u32,
    pub deletions: u32,
    pub max_line: u32,
}

impl FileDiff {
    fn new(path: String, old_path: Option<String>) -> Self {
        Self {
            path,
            old_path,
            status: FileStatus::Modified,
            binary: false,
            notices: Vec::new(),
            hunks: Vec::new(),
            additions: 0,
            deletions: 0,
            max_line: 0,
        }
    }
}

pub fn gutter_width(file: &FileDiff) -> f32 {
    let digits = file.max_line.max(1).ilog10() + 1;
    (digits as f32 * 6.6 + 8.0 + 6.0).max(GUTTER_WIDTH)
}

fn strip_git_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

fn parse_git_paths(rest: &str) -> (String, String) {
    fn unquote(s: &str) -> String {
        let trimmed = s.trim();
        if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
            trimmed[1..trimmed.len() - 1]
                .replace("\\\"", "\"")
                .replace("\\\\", "\\")
        } else {
            trimmed.to_string()
        }
    }
    if let Some(pos) = rest.rfind(" b/").or_else(|| rest.rfind(" \"b/")) {
        let old = unquote(&rest[..pos]);
        let new = unquote(&rest[pos + 1..]);
        (
            strip_git_prefix(&old).to_string(),
            strip_git_prefix(&new).to_string(),
        )
    } else {
        let p = strip_git_prefix(&unquote(rest)).to_string();
        (p.clone(), p)
    }
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@")?;
    let minus = rest.find('-')?;
    let after_minus = &rest[minus + 1..];
    let old: u32 = after_minus
        .split(|c: char| c == ',' || c.is_whitespace())
        .next()?
        .parse()
        .ok()?;
    let plus = rest.find('+')?;
    let after_plus = &rest[plus + 1..];
    let new: u32 = after_plus
        .split(|c: char| c == ',' || c.is_whitespace())
        .next()?
        .parse()
        .ok()?;
    Some((old, new))
}

pub fn parse_patch(patch: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut in_hunk = false;
    let mut old_no: u32 = 0;
    let mut new_no: u32 = 0;

    for raw in patch.lines() {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            let (old, new) = parse_git_paths(rest);
            let old_path = (old != new).then_some(old);
            files.push(FileDiff::new(new, old_path));
            in_hunk = false;
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };

        if raw.starts_with("@@") {
            if let Some((o, n)) = parse_hunk_header(raw) {
                old_no = o;
                new_no = n;
                file.hunks.push(Hunk {
                    header: raw.to_string(),
                    lines: Vec::new(),
                });
                in_hunk = true;
            }
            continue;
        }

        if in_hunk {
            let mut chars = raw.chars();
            let marker = chars.next();
            let body: String = chars.collect();
            let line = match marker {
                Some('+') => {
                    file.additions += 1;
                    let l = DiffLine {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(new_no),
                        text: body,
                    };
                    new_no += 1;
                    Some(l)
                }
                Some('-') => {
                    file.deletions += 1;
                    let l = DiffLine {
                        kind: LineKind::Del,
                        old_no: Some(old_no),
                        new_no: None,
                        text: body,
                    };
                    old_no += 1;
                    Some(l)
                }
                Some(' ') | None => {
                    let l = DiffLine {
                        kind: LineKind::Context,
                        old_no: Some(old_no),
                        new_no: Some(new_no),
                        text: body,
                    };
                    old_no += 1;
                    new_no += 1;
                    Some(l)
                }
                Some('\\') => Some(DiffLine {
                    kind: LineKind::Meta,
                    old_no: None,
                    new_no: None,
                    text: raw.trim_start_matches('\\').trim().to_string(),
                }),
                _ => {
                    in_hunk = false;
                    None
                }
            };
            if let Some(line) = line
                && let Some(hunk) = file.hunks.last_mut()
            {
                file.max_line = file
                    .max_line
                    .max(line.old_no.unwrap_or(0))
                    .max(line.new_no.unwrap_or(0));
                hunk.lines.push(line);
                continue;
            }
            if in_hunk {
                continue;
            }
        }

        if raw.starts_with("new file mode") {
            file.status = FileStatus::Added;
        } else if raw.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
        } else if let Some(from) = raw.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = Some(from.trim().to_string());
        } else if let Some(to) = raw.strip_prefix("rename to ") {
            file.status = FileStatus::Renamed;
            file.path = to.trim().to_string();
        } else if raw.starts_with("Binary files") || raw.starts_with("GIT binary patch") {
            file.binary = true;
        } else if let Some(mode) = raw.strip_prefix("new mode ") {
            file.notices
                .push(format!("Mode changed to {}", mode.trim()));
        } else if let Some(new) = raw.strip_prefix("+++ ") {
            let new = new.trim();
            if new == "/dev/null" {
                file.status = FileStatus::Deleted;
            } else if file.old_path.is_none() {
                file.path = strip_git_prefix(new).to_string();
            }
        } else if let Some(old) = raw.strip_prefix("--- ")
            && old.trim() == "/dev/null"
        {
            file.status = FileStatus::Added;
        }
    }
    files
}

pub fn file_notices(file: &FileDiff) -> Vec<String> {
    let mut notices = Vec::new();
    match file.status {
        FileStatus::Added => notices.push("New file".to_string()),
        FileStatus::Deleted => notices.push("Deleted file".to_string()),
        FileStatus::Renamed => {
            let from = file.old_path.as_deref().unwrap_or("?");
            notices.push(format!("Renamed from {from}"));
        }
        FileStatus::Modified => {}
    }
    if file.binary {
        notices.push("Binary file — contents not shown".to_string());
    }
    notices.extend(file.notices.iter().cloned());
    notices
}

pub fn truncate_file_lines(file: &mut FileDiff, max_lines: usize) {
    let total: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
    if total <= max_lines {
        return;
    }
    let mut budget = max_lines;
    file.hunks.retain_mut(|hunk| {
        if budget == 0 {
            return false;
        }
        if hunk.lines.len() > budget {
            hunk.lines.truncate(budget);
        }
        budget -= hunk.lines.len();
        true
    });
    file.notices.push(format!(
        "Diff truncated — showing first {max_lines} of {total} lines"
    ));
    file.max_line = file
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .map(|l| l.old_no.unwrap_or(0).max(l.new_no.unwrap_or(0)))
        .max()
        .unwrap_or(0);
}

pub fn body_height(file: &FileDiff) -> f32 {
    body_height_with(file, &[], None)
}

pub fn body_height_with(
    file: &FileDiff,
    comments: &[DiffComment],
    draft: Option<(CommentSide, u32)>,
) -> f32 {
    body_rows(0, file, comments, draft)
        .iter()
        .map(|row| row.height(comments))
        .sum()
}

pub fn line_anchor(line: &DiffLine) -> Option<(CommentSide, u32)> {
    match line.kind {
        LineKind::Meta => None,
        LineKind::Del => line.old_no.map(|no| (CommentSide::Old, no)),
        _ => line.new_no.map(|no| (CommentSide::New, no)),
    }
}

pub fn resolve_diff<'a>(diffs: &'a [CheckoutDiff], chat: &Chat) -> Option<&'a CheckoutDiff> {
    if let Some(checkout_id) = chat.checkout_id.as_deref()
        && let Some(diff) = diffs.iter().find(|d| d.checkout_id == checkout_id)
    {
        return Some(diff);
    }
    let cwd = chat.cwd.as_deref()?;
    diffs
        .iter()
        .find(|d| d.device_id == chat.device_id && d.cwd == cwd)
        .or_else(|| diffs.iter().find(|d| d.cwd == cwd))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffPhase {
    Preparing,
    Clean,
    List,
}

pub fn diff_phase(resolved: Option<&CheckoutDiff>) -> DiffPhase {
    match resolved {
        None => DiffPhase::Preparing,
        Some(diff) if diff.patch.trim().is_empty() && diff.files.is_empty() => DiffPhase::Clean,
        Some(_) => DiffPhase::List,
    }
}

pub fn uncommitted_label(count: usize) -> String {
    if count == 1 {
        "1 Uncommitted change".to_string()
    } else {
        format!("{count} Uncommitted changes")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffScope {
    #[default]
    WorkingTree,
    Branch,
    LatestTurn,
    History,
    Commit,
}

impl DiffScope {
    pub const ALL: [DiffScope; 4] = [
        Self::WorkingTree,
        Self::Branch,
        Self::LatestTurn,
        Self::History,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::WorkingTree => "Working tree",
            Self::Branch => "Branch changes",
            Self::LatestTurn => "Latest turn",
            Self::History => "History",
            Self::Commit => "Commit",
        }
    }

    pub fn mode(self) -> &'static str {
        match self {
            Self::WorkingTree => "workingTree",
            Self::Branch => "branch",
            Self::LatestTurn => "turn",
            Self::History => "history",
            Self::Commit => "commit",
        }
    }
}

pub fn scope_label(scope: DiffScope, count: usize, base: Option<&str>) -> String {
    let files = if count == 1 { "file" } else { "files" };
    match scope {
        DiffScope::WorkingTree => uncommitted_label(count),
        DiffScope::Branch => match base {
            Some(base) => format!("{count} Changed {files} vs {base}"),
            None => format!("{count} Changed {files}"),
        },
        DiffScope::LatestTurn => format!("{count} Changed {files} this turn"),
        DiffScope::History => "History".to_string(),
        DiffScope::Commit => format!("{count} Changed {files} in this commit"),
    }
}

pub fn default_base_ref(branches: &[String], current: Option<&str>) -> Option<String> {
    let first = branches.first()?;
    if current != Some(first.as_str()) {
        return Some(first.clone());
    }
    for candidate in ["main", "master"] {
        if branches.iter().any(|b| b == candidate) {
            return Some(candidate.to_string());
        }
    }
    branches
        .iter()
        .find(|b| current != Some(b.as_str()))
        .or(Some(first))
        .cloned()
}

pub fn clean_message(scope: DiffScope, base: Option<&str>) -> String {
    match scope {
        DiffScope::WorkingTree => "No uncommitted changes".to_string(),
        DiffScope::Branch => match base {
            Some(base) => format!("No changes vs {base}"),
            None => "No branch changes".to_string(),
        },
        DiffScope::LatestTurn => "No changes this turn".to_string(),
        DiffScope::History => "No commits found".to_string(),
        DiffScope::Commit => "Empty commit".to_string(),
    }
}

pub fn apply_diff_frame(diffs: &mut Vec<CheckoutDiff>, value: serde_json::Value) -> bool {
    if let Ok(all) = serde_json::from_value::<Vec<CheckoutDiff>>(value.clone()) {
        if *diffs != all {
            *diffs = all;
            return true;
        }
        return false;
    }
    match serde_json::from_value::<CheckoutDiff>(value) {
        Ok(one) => {
            if let Some(existing) = diffs.iter_mut().find(|d| d.checkout_id == one.checkout_id) {
                if *existing == one {
                    return false;
                }
                *existing = one;
            } else {
                diffs.push(one);
            }
            true
        }
        Err(err) => {
            tracing::warn!(error = %err, "changes: dropping malformed diff frame");
            false
        }
    }
}

fn comment_state_key(comments: &[DiffComment], draft: Option<&(String, CommentSide, u32)>) -> u64 {
    let mut parts: Vec<String> = comments.iter().map(|comment| comment.id.clone()).collect();
    if let Some((path, side, line)) = draft {
        parts.push(format!("draft:{path}:{}:{line}", side.tag()));
    }
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    hash64(&refs)
}

fn hash64(parts: &[&str]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    for p in parts {
        p.hash(&mut hasher);
    }
    hasher.finish()
}

const MAX_EXCERPT_SOURCE_LINES: usize = 200_000;

fn excerpt_side(
    file: &FileDiff,
    side: SourceSide,
    language: Lang,
    path: &str,
) -> Option<Arc<onyx_syntax::HighlightedDocument>> {
    let max_line = file
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter_map(|line| match side {
            SourceSide::Old => line.old_no,
            SourceSide::New => line.new_no,
        })
        .max()
        .unwrap_or(0) as usize;
    if max_line > MAX_EXCERPT_SOURCE_LINES {
        return None;
    }
    let mut lines = vec![Vec::new(); max_line];
    for hunk in &file.hunks {
        let visible = hunk
            .lines
            .iter()
            .filter_map(|line| {
                let number = match side {
                    SourceSide::Old => line.old_no,
                    SourceSide::New => line.new_no,
                }?;
                (line.kind != LineKind::Meta).then_some((number, line.text.as_str()))
            })
            .collect::<Vec<_>>();
        if visible.is_empty() {
            continue;
        }
        let source = visible
            .iter()
            .map(|(_, text)| *text)
            .collect::<Vec<_>>()
            .join("\n");
        let document = onyx_syntax::highlight(onyx_syntax::HighlightRequest {
            source: &source,
            path: Some(path),
            fence_tag: None,
        })
        .ok()?;
        for ((number, _), spans) in visible.into_iter().zip(document.lines) {
            lines[number as usize - 1] = spans;
        }
    }
    Some(Arc::new(onyx_syntax::HighlightedDocument {
        language,
        lines,
    }))
}

fn excerpt_highlights(file: &FileDiff, language: Lang) -> Option<DiffHighlights> {
    if !onyx_syntax::supports_language(language) {
        return None;
    }
    let old = if file.status == FileStatus::Added {
        None
    } else {
        Some(excerpt_side(
            file,
            SourceSide::Old,
            language,
            file.old_path.as_deref().unwrap_or(&file.path),
        )?)
    };
    let new = if file.status == FileStatus::Deleted {
        None
    } else {
        Some(excerpt_side(file, SourceSide::New, language, &file.path)?)
    };
    Some(DiffHighlights { old, new })
}

fn sources_match_patch(file: &FileDiff, response: &zeron_proto::CheckoutFileDiffText) -> bool {
    let old = response
        .old_text
        .as_deref()
        .map(|source| source.lines().collect::<Vec<_>>());
    let new = response
        .new_text
        .as_deref()
        .map(|source| source.lines().collect::<Vec<_>>());
    file.hunks.iter().flat_map(|hunk| &hunk.lines).all(|line| {
        let actual = match line.kind {
            LineKind::Del => line
                .old_no
                .and_then(|number| old.as_ref()?.get(number as usize - 1).copied()),
            LineKind::Add => line
                .new_no
                .and_then(|number| new.as_ref()?.get(number as usize - 1).copied()),
            LineKind::Context => line
                .new_no
                .and_then(|number| new.as_ref()?.get(number as usize - 1).copied())
                .or_else(|| {
                    line.old_no
                        .and_then(|number| old.as_ref()?.get(number as usize - 1).copied())
                }),
            LineKind::Meta => return true,
        };
        actual == Some(line.text.as_str())
    })
}

fn full_highlights(
    file: &FileDiff,
    language: Lang,
    response: &zeron_proto::CheckoutFileDiffText,
) -> Option<DiffHighlights> {
    if response.stale
        || response.binary
        || response.truncated
        || !sources_match_patch(file, response)
    {
        return None;
    }
    let parse = |source: &str, path: &str| {
        onyx_syntax::highlight(onyx_syntax::HighlightRequest {
            source,
            path: Some(path),
            fence_tag: None,
        })
        .ok()
        .map(Arc::new)
    };
    let old = match response.old_text.as_deref() {
        Some(source) => Some(parse(
            source,
            file.old_path.as_deref().unwrap_or(&file.path),
        )?),
        None => None,
    };
    let new = match response.new_text.as_deref() {
        Some(source) => Some(parse(source, &file.path)?),
        None => None,
    };
    if old.is_none() && new.is_none() && onyx_syntax::supports_language(language) {
        return None;
    }
    Some(DiffHighlights { old, new })
}

struct ParsedDiff {
    key: String,
    truncated: bool,
    additions: u32,
    deletions: u32,
    file_count: usize,
    files: Arc<Vec<FileDiff>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRow {
    FileHeader {
        file: u32,
    },
    Notice {
        file: u32,
        notice: u32,
    },
    HunkHeader {
        file: u32,
        hunk: u32,
    },
    Line {
        file: u32,
        hunk: u32,
        line: u32,
        flat: u32,
    },
    CommentCard {
        file: u32,
        card: u32,
    },
    CommentDraft {
        file: u32,
    },
    BodyPad {
        file: u32,
    },
    FoldingBody {
        file: u32,
    },
}

impl DiffRow {
    fn height(self, comments: &[DiffComment]) -> f32 {
        match self {
            DiffRow::FileHeader { .. } => FILE_HEADER_HEIGHT,
            DiffRow::Notice { .. } => NOTICE_HEIGHT,
            DiffRow::HunkHeader { .. } => HUNK_HEADER_HEIGHT,
            DiffRow::Line { .. } => DIFF_LINE_HEIGHT,
            DiffRow::CommentCard { card, .. } => comments
                .get(card as usize)
                .map(|comment| comments::card_height(&comment.body))
                .unwrap_or(0.0),
            DiffRow::CommentDraft { .. } => comments::DRAFT_CARD_HEIGHT,
            DiffRow::BodyPad { .. } => BODY_BOTTOM_PAD,
            DiffRow::FoldingBody { .. } => 0.0,
        }
    }
}

pub fn body_row_count(file: &FileDiff) -> usize {
    let lines: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
    file_notices(file).len() + file.hunks.len() + lines + 1
}

pub fn body_rows(
    file_ix: u32,
    file: &FileDiff,
    comments: &[DiffComment],
    draft: Option<(CommentSide, u32)>,
) -> Vec<DiffRow> {
    fn push_cards(
        rows: &mut Vec<DiffRow>,
        file_ix: u32,
        comments: &[DiffComment],
        draft: Option<(CommentSide, u32)>,
        anchors: &[Option<(CommentSide, u32)>],
    ) {
        for anchor in anchors.iter().flatten() {
            for (ix, comment) in comments.iter().enumerate() {
                if comment.anchor() == *anchor {
                    rows.push(DiffRow::CommentCard {
                        file: file_ix,
                        card: ix as u32,
                    });
                }
            }
            if draft == Some(*anchor) {
                rows.push(DiffRow::CommentDraft { file: file_ix });
            }
        }
    }

    let mut rows = Vec::with_capacity(body_row_count(file));
    for notice in 0..file_notices(file).len() {
        rows.push(DiffRow::Notice {
            file: file_ix,
            notice: notice as u32,
        });
    }
    let mut hunk_flat = 0u32;
    for (hunk_ix, hunk) in file.hunks.iter().enumerate() {
        rows.push(DiffRow::HunkHeader {
            file: file_ix,
            hunk: hunk_ix as u32,
        });
        for (line_ix, line) in hunk.lines.iter().enumerate() {
            rows.push(DiffRow::Line {
                file: file_ix,
                hunk: hunk_ix as u32,
                line: line_ix as u32,
                flat: hunk_flat + line_ix as u32,
            });
            push_cards(&mut rows, file_ix, comments, draft, &[line_anchor(line)]);
        }
        hunk_flat += hunk.lines.len() as u32;
    }
    rows.push(DiffRow::BodyPad { file: file_ix });
    rows
}

pub fn flatten_rows(
    files: &[FileDiff],
    comments: &[DiffComment],
    draft: Option<(&str, CommentSide, u32)>,
    mut collapsed: impl FnMut(usize) -> bool,
) -> (Vec<DiffRow>, Vec<std::ops::Range<usize>>) {
    let mut rows = Vec::new();
    let mut ranges = Vec::with_capacity(files.len());
    for (ix, file) in files.iter().enumerate() {
        let start = rows.len();
        rows.push(DiffRow::FileHeader { file: ix as u32 });
        if !collapsed(ix) {
            let file_comments: Vec<DiffComment> = comments
                .iter()
                .filter(|comment| comment.path == file.path)
                .cloned()
                .collect();
            let file_draft = draft
                .filter(|(path, _, _)| *path == file.path)
                .map(|(_, side, line)| (side, line));
            rows.extend(body_rows(ix as u32, file, &file_comments, file_draft));
        }
        ranges.push(start..rows.len());
    }
    (rows, ranges)
}

#[derive(Default, Clone, Copy)]
struct FileFold {
    collapsed: bool,
    epoch: usize,
    from: f32,
    to: f32,
    toggled_at: Option<std::time::Instant>,
}

const FOLD_TWEEN_WINDOW: Duration = Duration::from_millis(400);

const FOLD_TWEEN_MAX_PX: f32 = 2400.0;

impl FileFold {
    fn animating(&self) -> bool {
        self.epoch > 0
            && self
                .toggled_at
                .is_some_and(|at| at.elapsed() < FOLD_TWEEN_WINDOW)
    }
}

struct HighlightSlot {
    fingerprint: u64,
    state: DiffHighlightState,
    _excerpt_task: Option<Task<()>>,
    _fetch_task: Option<Task<()>>,
}

enum DiffHighlightState {
    Pending,
    Ready(Arc<DiffHighlights>),
    Excerpt(Arc<DiffHighlights>),
    Plain,
}

struct RefMenu {
    search: Entity<ComposerInput>,
    active: usize,
    focus: FocusHandle,
    list_scroll: gpui::ScrollHandle,
    _search_events: Subscription,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HoverRow {
    path: String,
    side: CommentSide,
    line: u32,
}

struct CommentDraft {
    key: String,
    path: String,
    old_path: Option<String>,
    side: CommentSide,
    line: u32,
    input: Entity<ComposerInput>,
    _events: Subscription,
}

pub struct Changes {
    state: Entity<AppState>,
    diffs: Vec<CheckoutDiff>,
    started: bool,
    error: Option<SharedString>,
    watch_target: Option<String>,
    watch_task: Option<Task<()>>,
    parsed: Option<ParsedDiff>,
    parse_task: Option<Task<()>>,
    folds: HashMap<String, FileFold>,
    highlights: HashMap<String, HighlightSlot>,
    rows: Vec<DiffRow>,
    row_ranges: Vec<std::ops::Range<usize>>,
    fold_settle: Option<Task<()>>,
    list: ListState,
    scope: DiffScope,
    base_ref: Option<String>,
    branches: Vec<String>,
    branches_for: Option<String>,
    branches_task: Option<Task<()>>,
    scoped: Option<CheckoutDiff>,
    scoped_for: Option<String>,
    scoped_error: Option<SharedString>,
    scoped_inflight: Option<String>,
    scoped_task: Option<Task<()>>,
    scope_menu: Popup<()>,
    ref_menu: Popup<RefMenu>,
    draft: Option<CommentDraft>,
    hover: Option<HoverRow>,
    comment_key: u64,
    history: Option<Entity<GitHistory>>,
    history_count: Option<Entity<GitHistoryCount>>,
    history_fetch_button: Option<Entity<GitHistoryFetchButton>>,
    history_events: Option<Subscription>,
    commit: Option<GitHistoryCommit>,
    _observe: Subscription,
}

pub enum ChangesEvent {
    OpenCommit(GitHistoryCommit),
}

impl gpui::EventEmitter<ChangesEvent> for Changes {}

impl Changes {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.sync(cx));
        Self {
            state,
            diffs: Vec::new(),
            started: false,
            error: None,
            watch_target: None,
            watch_task: None,
            parsed: None,
            parse_task: None,
            folds: HashMap::new(),
            highlights: HashMap::new(),
            rows: Vec::new(),
            row_ranges: Vec::new(),
            fold_settle: None,
            list: ListState::new(0, ListAlignment::Top, px(1024.0)),
            scope: DiffScope::default(),
            base_ref: None,
            branches: Vec::new(),
            branches_for: None,
            branches_task: None,
            scoped: None,
            scoped_for: None,
            scoped_error: None,
            scoped_inflight: None,
            scoped_task: None,
            scope_menu: Popup::default(),
            ref_menu: Popup::default(),
            draft: None,
            hover: None,
            comment_key: 0,
            history: None,
            history_count: None,
            history_fetch_button: None,
            history_events: None,
            commit: None,
            _observe: observe,
        }
    }

    pub fn for_commit(
        state: Entity<AppState>,
        commit: GitHistoryCommit,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut changes = Self::new(state, cx);
        changes.scope = DiffScope::Commit;
        changes.commit = Some(commit);
        changes
    }

    pub fn tab_title(&self) -> gpui::SharedString {
        if let Some(commit) = &self.commit {
            let subject = commit.subject.trim();
            if !subject.is_empty() {
                return subject.to_string().into();
            }
            return commit.sha.chars().take(7).collect::<String>().into();
        }
        gpui::SharedString::from(self.scope.label())
    }

    fn desired_target(&self, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        let device = state.selected_chat_row()?.device_id.clone();
        (state.local_device_id.as_deref() != Some(device.as_str())).then_some(device)
    }

    pub fn ensure_watch(&mut self, cx: &mut Context<Self>) {
        let target = self.desired_target(cx);
        if self.started && self.watch_target == target {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if self.started {
            self.diffs.clear();
            self.error = None;
        }
        self.started = true;
        self.watch_target = target.clone();
        self.watch_task = Some(Self::spawn_watch(engine, target, cx));
    }

    fn spawn_watch(
        engine: EngineHandle,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let mut params = serde_json::Map::new();
                if let Some(target) = &target {
                    params.insert(
                        "targetDeviceId".into(),
                        serde_json::Value::String(target.clone()),
                    );
                }
                let subscribed = engine
                    .client()
                    .subscribe(
                        methods::WATCH_CHECKOUT_DIFFS,
                        serde_json::Value::Object(params),
                    )
                    .await;
                match subscribed {
                    Ok(mut rx) => {
                        while let Some(value) = rx.recv().await {
                            let alive = this.update(cx, |changes, cx| {
                                changes.error = None;
                                if apply_diff_frame(&mut changes.diffs, value) {
                                    changes.sync(cx);
                                    cx.notify();
                                }
                            });
                            if alive.is_err() {
                                return;
                            }
                        }
                        if this
                            .update(cx, |changes, cx| {
                                changes.error = Some("Diff stream interrupted — retrying".into());
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(err) => {
                        if this
                            .update(cx, |changes, cx| {
                                changes.error =
                                    Some(format!("Diff watch unavailable: {err}").into());
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
    }

    fn resolved(&self, cx: &App) -> Option<CheckoutDiff> {
        let state = self.state.read(cx);
        let chat = state.selected_chat_row()?;
        resolve_diff(&self.diffs, chat).cloned()
    }

    fn scoped_cwd(&self, cx: &App) -> Option<String> {
        if let Some(diff) = self.resolved(cx) {
            return Some(diff.cwd);
        }
        self.state.read(cx).selected_chat_row()?.cwd.clone()
    }

    fn active_diff(&self, cx: &App) -> Option<CheckoutDiff> {
        match self.scope {
            DiffScope::WorkingTree => self.resolved(cx),
            DiffScope::Branch | DiffScope::LatestTurn | DiffScope::Commit => self.scoped.clone(),
            DiffScope::History => None,
        }
    }

    fn scope_key(&self) -> String {
        match self.scope {
            DiffScope::WorkingTree => "wt".to_string(),
            DiffScope::Branch => format!("br:{}", self.base_ref.as_deref().unwrap_or("")),
            DiffScope::LatestTurn => "turn".to_string(),
            DiffScope::History => "history".to_string(),
            DiffScope::Commit => format!(
                "commit:{}",
                self.commit.as_ref().map(|c| c.sha.as_str()).unwrap_or("")
            ),
        }
    }

    fn parse_key(&self, diff: &CheckoutDiff) -> String {
        format!(
            "{}:{}:{}",
            diff.checkout_id,
            diff.checksum,
            self.scope_key()
        )
    }

    fn ensure_branches(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.scoped_cwd(cx) else {
            return;
        };
        let target = self.desired_target(cx);
        let key = format!("{}:{}", target.as_deref().unwrap_or("local"), cwd);
        if self.branches_for.as_deref() == Some(key.as_str()) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.branches_for = Some(key.clone());
        self.branches_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert("repoPath".into(), serde_json::Value::String(cwd));
            if let Some(target) = target {
                params.insert("targetDeviceId".into(), serde_json::Value::String(target));
            }
            let result = engine
                .client()
                .call(methods::LIST_BRANCHES, serde_json::Value::Object(params))
                .await;
            this.update(cx, |changes, cx| {
                if changes.branches_for.as_deref() != Some(key.as_str()) {
                    return;
                }
                match result {
                    Ok(value) => {
                        changes.branches =
                            serde_json::from_value::<Vec<String>>(value).unwrap_or_default();
                        let keep = changes
                            .base_ref
                            .as_ref()
                            .is_some_and(|base| changes.branches.contains(base));
                        if !keep {
                            let current = changes
                                .state
                                .read(cx)
                                .selected_chat_row()
                                .and_then(|chat| chat.branch.clone());
                            changes.base_ref =
                                default_base_ref(&changes.branches, current.as_deref());
                        }
                        changes.sync(cx);
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "changes: branch list failed");
                        changes.branches_for = None;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn ensure_scoped(&mut self, cx: &mut Context<Self>) {
        if matches!(self.scope, DiffScope::WorkingTree | DiffScope::History) {
            self.scoped_inflight = None;
            self.scoped_task = None;
            return;
        }
        let Some(chat_id) = self
            .state
            .read(cx)
            .selected_chat_row()
            .map(|chat| chat.id.clone())
        else {
            return;
        };
        let Some(cwd) = self.scoped_cwd(cx) else {
            return;
        };
        let base = match self.scope {
            DiffScope::Branch => match &self.base_ref {
                Some(base) => Some(base.clone()),
                None => return,
            },
            _ => None,
        };
        let commit_sha = match self.scope {
            DiffScope::Commit => match &self.commit {
                Some(commit) => Some(commit.sha.clone()),
                None => return,
            },
            _ => None,
        };
        let target = self.desired_target(cx);
        let context = format!(
            "{}|{}|{}|{}|{}|{}",
            target.as_deref().unwrap_or("local"),
            chat_id,
            cwd,
            self.scope.mode(),
            base.as_deref().unwrap_or(""),
            commit_sha.as_deref().unwrap_or("")
        );
        let watch_sum = self.resolved(cx).map(|d| d.checksum).unwrap_or_default();
        let key = format!("{context}|{watch_sum}");
        if self.scoped_for.as_deref() == Some(key.as_str())
            || self.scoped_inflight.as_deref() == Some(key.as_str())
        {
            return;
        }
        if self
            .scoped_for
            .as_deref()
            .is_none_or(|prev| !prev.starts_with(&format!("{context}|")))
        {
            self.scoped = None;
            self.scoped_error = None;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let mode = self.scope.mode();
        self.scoped_inflight = Some(key.clone());
        self.scoped_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert("cwd".into(), serde_json::Value::String(cwd));
            params.insert("mode".into(), serde_json::Value::String(mode.to_string()));
            params.insert("chatId".into(), serde_json::Value::String(chat_id));
            if let Some(base) = base {
                params.insert("baseRef".into(), serde_json::Value::String(base));
            }
            if let Some(sha) = commit_sha {
                params.insert("commitSha".into(), serde_json::Value::String(sha));
            }
            if let Some(target) = target {
                params.insert("targetDeviceId".into(), serde_json::Value::String(target));
            }
            let result = engine
                .client()
                .call(
                    methods::GET_CHECKOUT_DIFF,
                    serde_json::Value::Object(params),
                )
                .await;
            this.update(cx, |changes, cx| {
                if changes.scoped_inflight.as_deref() != Some(key.as_str()) {
                    return;
                }
                changes.scoped_inflight = None;
                match result.and_then(|value| {
                    serde_json::from_value::<CheckoutDiff>(value)
                        .map_err(|e| zeron_rpc::RpcError::Failed(e.to_string()))
                }) {
                    Ok(diff) => {
                        changes.scoped = Some(diff);
                        changes.scoped_error = None;
                    }
                    Err(err) => {
                        changes.scoped = None;
                        changes.scoped_error = Some(err.to_string().into());
                    }
                }
                changes.scoped_for = Some(key);
                changes.sync(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn set_scope(&mut self, scope: DiffScope, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            if scope == DiffScope::History {
                self.history_pane(cx)
                    .update(cx, |history, cx| history.ensure_loaded(cx));
            }
            self.sync(cx);
        }
        cx.notify();
    }

    fn history_pane(&mut self, cx: &mut Context<Self>) -> Entity<GitHistory> {
        if let Some(history) = &self.history {
            return history.clone();
        }
        let history = cx.new(|cx| GitHistory::new(self.state.clone(), cx));
        self.history_events =
            Some(
                cx.subscribe(&history, |this: &mut Self, _, event, cx| match event {
                    GitHistoryEvent::OpenCommit(commit) => {
                        cx.emit(ChangesEvent::OpenCommit(commit.clone()));
                    }
                    GitHistoryEvent::FetchSucceeded => {
                        this.branches_for = None;
                        this.scoped_for = None;
                        this.scoped_inflight = None;
                        this.scoped_task = None;
                        this.ensure_branches(cx);
                        if this.scope != DiffScope::History {
                            this.ensure_scoped(cx);
                        }
                        cx.notify();
                    }
                }),
            );
        self.history = Some(history.clone());
        history
    }

    fn history_count(&mut self, cx: &mut Context<Self>) -> Entity<GitHistoryCount> {
        if let Some(count) = &self.history_count {
            return count.clone();
        }
        let history = self.history_pane(cx);
        let count = cx.new(|cx| GitHistoryCount::new(history, cx));
        self.history_count = Some(count.clone());
        count
    }

    fn history_fetch_button(&mut self, cx: &mut Context<Self>) -> Entity<GitHistoryFetchButton> {
        if let Some(button) = &self.history_fetch_button {
            return button.clone();
        }
        let history = self.history_pane(cx);
        let button = cx.new(|cx| GitHistoryFetchButton::new(history, cx));
        self.history_fetch_button = Some(button.clone());
        button
    }

    fn set_base_ref(&mut self, base: String, cx: &mut Context<Self>) {
        if self.base_ref.as_deref() != Some(base.as_str()) {
            self.base_ref = Some(base);
            self.sync(cx);
        }
        cx.notify();
    }

    pub fn ensure_content(&mut self, cx: &mut Context<Self>) {
        self.sync(cx);
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        self.discard_stale_draft(cx);
        self.ensure_watch(cx);
        if self.scope == DiffScope::History {
            self.history_pane(cx)
                .update(cx, |history, cx| history.ensure_loaded(cx));
            return;
        }
        if self.scope != DiffScope::Commit {
            self.ensure_branches(cx);
        }
        self.ensure_scoped(cx);
        let Some(diff) = self.active_diff(cx) else {
            if self.parsed.take().is_some() {
                self.rows.clear();
                self.row_ranges.clear();
                self.list.reset(0);
                self.folds.clear();
                self.highlights.clear();
                cx.notify();
            }
            return;
        };
        let key = self.parse_key(&diff);
        if self.parsed.as_ref().is_some_and(|p| p.key == key) {
            self.sync_comment_rows(cx);
            return;
        }
        let patch = diff.patch.clone();
        let truncated = diff.truncated;
        let additions = diff.additions;
        let deletions = diff.deletions;
        let file_count = diff.files.len();
        self.parse_task = Some(cx.spawn(async move |this, cx| {
            let files = cx
                .background_executor()
                .spawn(async move { parse_patch(&patch) })
                .await;
            this.update(cx, |changes, cx| {
                let current = changes.active_diff(cx).map(|d| changes.parse_key(&d));
                if current.as_deref() != Some(key.as_str()) {
                    return;
                }
                let file_count = if file_count > 0 {
                    file_count
                } else {
                    files.len()
                };
                changes.folds.clear();
                changes.highlights.clear();
                let staged = changes.staged_comments(cx);
                let draft = changes.draft_anchor();
                let (rows, ranges) = flatten_rows(
                    &files,
                    &staged,
                    draft
                        .as_ref()
                        .map(|(path, side, line)| (path.as_str(), *side, *line)),
                    |_| false,
                );
                changes.comment_key = comment_state_key(&staged, draft.as_ref());
                changes
                    .list
                    .reset_with_uniform_height(rows.len(), px(DIFF_LINE_HEIGHT));
                changes.rows = rows;
                changes.row_ranges = ranges;
                changes.parsed = Some(ParsedDiff {
                    key,
                    truncated,
                    additions,
                    deletions,
                    file_count,
                    files: Arc::new(files),
                });
                cx.notify();
            })
            .ok();
        }));
    }

    fn replace_file_body(&mut self, file_ix: usize, new_body: Vec<DiffRow>) {
        let Some(range) = self.row_ranges.get(file_ix).cloned() else {
            return;
        };
        let body = range.start + 1..range.end;
        let delta = new_body.len() as isize - body.len() as isize;
        let (prefix, suffix) = {
            let old = &self.rows[body.clone()];
            let prefix = old
                .iter()
                .zip(&new_body)
                .take_while(|(a, b)| a == b)
                .count();
            let suffix = old[prefix..]
                .iter()
                .rev()
                .zip(new_body[prefix..].iter().rev())
                .take_while(|(a, b)| a == b)
                .count();
            (prefix, suffix)
        };
        if delta == 0 && prefix + suffix >= body.len() {
            return;
        }
        let changed = body.start + prefix..body.end - suffix;
        let mid: Vec<DiffRow> = new_body[prefix..new_body.len() - suffix].to_vec();
        self.list.splice(changed.clone(), mid.len());
        self.rows.splice(changed, mid);
        self.row_ranges[file_ix] = range.start..(range.end as isize + delta) as usize;
        for r in &mut self.row_ranges[file_ix + 1..] {
            *r = (r.start as isize + delta) as usize..(r.end as isize + delta) as usize;
        }
    }

    fn toggle_fold(&mut self, file_ix: usize, cx: &mut Context<Self>) {
        let Some(parsed) = &self.parsed else {
            return;
        };
        let Some(file) = parsed.files.get(file_ix) else {
            return;
        };
        let expanded_height = body_height_with(
            file,
            &self.comments_for(&file.path, cx),
            self.draft_anchor_in(&file.path),
        );
        let fold = self.folds.entry(file.path.clone()).or_default();
        let currently_collapsed = fold.collapsed;
        fold.from = if currently_collapsed {
            0.0
        } else {
            expanded_height
        };
        fold.to = if currently_collapsed {
            expanded_height
        } else {
            0.0
        };
        fold.collapsed = !currently_collapsed;
        fold.epoch += 1;
        fold.toggled_at = Some(std::time::Instant::now());
        self.replace_file_body(
            file_ix,
            vec![DiffRow::FoldingBody {
                file: file_ix as u32,
            }],
        );
        self.ensure_fold_settle(cx);
    }

    fn ensure_fold_settle(&mut self, cx: &mut Context<Self>) {
        if self.fold_settle.is_some() {
            return;
        }
        self.fold_settle = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(FOLD_TWEEN_WINDOW).await;
                let more = this
                    .update(cx, |changes, cx| changes.settle_folds(cx))
                    .unwrap_or(false);
                if !more {
                    break;
                }
            }
            this.update(cx, |changes, _| changes.fold_settle = None)
                .ok();
        }));
    }

    fn settle_folds(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(parsed) = &self.parsed else {
            return false;
        };
        let files = parsed.files.clone();
        let mut pending = false;
        for file_ix in (0..self.row_ranges.len()).rev() {
            let range = &self.row_ranges[file_ix];
            let folding = self.rows.get(range.start + 1)
                == Some(&DiffRow::FoldingBody {
                    file: file_ix as u32,
                });
            if !folding {
                continue;
            }
            let Some(file) = files.get(file_ix) else {
                continue;
            };
            let fold = self.folds.get(&file.path).copied().unwrap_or_default();
            if fold.animating() {
                pending = true;
                continue;
            }
            let body = if fold.collapsed {
                Vec::new()
            } else {
                body_rows(
                    file_ix as u32,
                    file,
                    &self.comments_for(&file.path, cx),
                    self.draft_anchor_in(&file.path),
                )
            };
            self.replace_file_body(file_ix, body);
        }
        cx.notify();
        pending
    }

    fn all_collapsed(&self) -> bool {
        let Some(parsed) = &self.parsed else {
            return false;
        };
        !parsed.files.is_empty()
            && parsed.files.iter().all(|file| {
                self.folds
                    .get(&file.path)
                    .is_some_and(|fold| fold.collapsed)
            })
    }

    fn toggle_collapse_all(&mut self, cx: &mut Context<Self>) {
        let Some(parsed) = &self.parsed else {
            return;
        };
        let collapse = !self.all_collapsed();
        let files = parsed.files.clone();
        for file in files.iter() {
            let fold = self.folds.entry(file.path.clone()).or_default();
            fold.collapsed = collapse;
            fold.toggled_at = None;
        }
        let staged = self.staged_comments(cx);
        let draft = self.draft_anchor();
        for file_ix in (0..self.row_ranges.len().min(files.len())).rev() {
            let range = &self.row_ranges[file_ix];
            let body = range.start + 1..range.end;
            let new_len = if collapse {
                0
            } else {
                let file = &files[file_ix];
                let comments: Vec<DiffComment> = staged
                    .iter()
                    .filter(|comment| comment.path == file.path)
                    .cloned()
                    .collect();
                body_rows(
                    file_ix as u32,
                    file,
                    &comments,
                    self.draft_anchor_in(&file.path),
                )
                .len()
            };
            if body.len() != new_len {
                self.list.splice(body, new_len);
            }
        }
        let (rows, ranges) = flatten_rows(
            &files,
            &staged,
            draft
                .as_ref()
                .map(|(path, side, line)| (path.as_str(), *side, *line)),
            |_| collapse,
        );
        self.rows = rows;
        self.row_ranges = ranges;
        cx.notify();
    }

    fn staged_comments(&self, cx: &App) -> Vec<DiffComment> {
        let state = self.state.read(cx);
        state.diff_comments(&state.composer_key()).to_vec()
    }

    fn comments_for(&self, path: &str, cx: &App) -> Vec<DiffComment> {
        self.staged_comments(cx)
            .into_iter()
            .filter(|comment| comment.path == path)
            .collect()
    }

    fn old_path_of(&self, path: &str) -> Option<String> {
        self.parsed
            .as_ref()?
            .files
            .iter()
            .find(|file| file.path == path)?
            .old_path
            .clone()
    }

    fn discard_stale_draft(&mut self, cx: &mut Context<Self>) {
        let key = self.state.read(cx).composer_key();
        if self.draft.as_ref().is_some_and(|draft| draft.key != key) {
            self.draft = None;
            self.sync_comment_rows(cx);
            cx.notify();
        }
    }

    fn draft_anchor(&self) -> Option<(String, CommentSide, u32)> {
        self.draft
            .as_ref()
            .map(|draft| (draft.path.clone(), draft.side, draft.line))
    }

    fn draft_anchor_in(&self, path: &str) -> Option<(CommentSide, u32)> {
        self.draft
            .as_ref()
            .filter(|draft| draft.path == path)
            .map(|draft| (draft.side, draft.line))
    }

    fn sync_comment_rows(&mut self, cx: &mut Context<Self>) {
        if self.parsed.is_none() {
            return;
        }
        let staged = self.staged_comments(cx);
        let draft = self.draft_anchor();
        let key = comment_state_key(&staged, draft.as_ref());
        if key == self.comment_key {
            return;
        }
        self.comment_key = key;
        let Some(parsed) = &self.parsed else {
            return;
        };
        let files = parsed.files.clone();
        for file_ix in (0..self.row_ranges.len().min(files.len())).rev() {
            let file = &files[file_ix];
            if self
                .folds
                .get(&file.path)
                .is_some_and(|fold| fold.collapsed)
            {
                continue;
            }
            let range = &self.row_ranges[file_ix];
            if self.rows.get(range.start + 1)
                == Some(&DiffRow::FoldingBody {
                    file: file_ix as u32,
                })
            {
                continue;
            }
            let comments: Vec<DiffComment> = staged
                .iter()
                .filter(|comment| comment.path == file.path)
                .cloned()
                .collect();
            let body = body_rows(
                file_ix as u32,
                file,
                &comments,
                self.draft_anchor_in(&file.path),
            );
            self.replace_file_body(file_ix, body);
        }
        cx.notify();
    }

    fn set_hover(
        &mut self,
        path: &str,
        anchor: Option<(CommentSide, u32)>,
        cx: &mut Context<Self>,
    ) {
        let next = anchor.map(|(side, line)| HoverRow {
            path: path.to_string(),
            side,
            line,
        });
        if next != self.hover {
            self.hover = next;
            cx.notify();
        }
    }

    fn clear_hover_at(&mut self, path: &str, side: CommentSide, line: u32, cx: &mut Context<Self>) {
        if self
            .hover
            .as_ref()
            .is_some_and(|hover| hover.path == path && hover.side == side && hover.line == line)
        {
            self.hover = None;
            cx.notify();
        }
    }

    fn open_draft(
        &mut self,
        path: String,
        side: CommentSide,
        line: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| ComposerInput::new("Request a change…", cx));
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.commit_draft(cx),
            ComposerInputEvent::Edited => cx.notify(),
            _ => {}
        });
        let handle = input.read(cx).focus_handle(cx);
        let key = self.state.read(cx).composer_key();
        let old_path = self.old_path_of(&path);
        self.draft = Some(CommentDraft {
            key,
            path,
            old_path,
            side,
            line,
            input,
            _events: events,
        });
        window.focus(&handle, cx);
        self.sync_comment_rows(cx);
        cx.notify();
    }

    fn cancel_draft(&mut self, cx: &mut Context<Self>) {
        self.draft = None;
        self.sync_comment_rows(cx);
        cx.notify();
    }

    fn commit_draft(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.take() else {
            return;
        };
        let body = draft.input.read(cx).text().trim().to_string();
        if body.is_empty() {
            self.sync_comment_rows(cx);
            cx.notify();
            return;
        }
        let comment =
            DiffComment::new(draft.path, draft.side, draft.line, body).renamed_from(draft.old_path);
        let key = draft.key;
        self.state.update(cx, |state, cx| {
            state.add_diff_comment(&key, comment);
            cx.notify();
        });
        self.sync_comment_rows(cx);
        cx.notify();
    }

    fn remove_comment(&mut self, id: &str, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            let key = state.composer_key();
            state.remove_diff_comment(&key, id);
            cx.notify();
        });
        self.sync_comment_rows(cx);
        cx.notify();
    }

    fn close_scope_menu(&mut self, cx: &mut Context<Self>) {
        if self.scope_menu.begin_close() {
            popover::reap_popup(cx, |changes: &mut Self| &mut changes.scope_menu);
        }
    }

    fn close_ref_menu(&mut self, cx: &mut Context<Self>) {
        if self.ref_menu.begin_close() {
            popover::reap_popup(cx, |changes: &mut Self| &mut changes.ref_menu);
        }
    }

    fn open_ref_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let search =
            cx.new(|cx| ComposerInput::with_context("Search branches…", "PaletteSearch", cx));
        let search_events = cx.subscribe(&search, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                if let Some(menu) = this.ref_menu.open_mut() {
                    menu.active = 0;
                }
                cx.notify();
            }
        });
        let handle = search.read(cx).focus_handle(cx);
        let active = self
            .base_ref
            .as_ref()
            .and_then(|base| self.branches.iter().position(|b| b == base))
            .unwrap_or(0);
        self.ref_menu.open(RefMenu {
            search,
            active,
            focus: cx.focus_handle(),
            list_scroll: gpui::ScrollHandle::new(),
            _search_events: search_events,
        });
        window.focus(&handle, cx);
        cx.notify();
    }

    fn ref_menu_rows(&self, cx: &App) -> Vec<usize> {
        let query = self
            .ref_menu
            .get()
            .map(|menu| menu.search.read(cx).text().to_string())
            .unwrap_or_default();
        popover::filter_indices(&query, &self.branches)
    }

    fn ref_menu_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        if !self.ref_menu.is_open() {
            return;
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => self.close_ref_menu(cx),
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = self.ref_menu_rows(cx).len();
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(menu) = self.ref_menu.open_mut() {
                    menu.active = popover::menu_step(Some(menu.active), count, delta).unwrap_or(0);
                    menu.list_scroll.scroll_to_item(menu.active);
                    cx.notify();
                }
            }
            popover::MenuKey::Enter | popover::MenuKey::ModEnter => {
                let active = self.ref_menu.get().map(|m| m.active).unwrap_or(0);
                let pick = self
                    .ref_menu_rows(cx)
                    .get(active)
                    .and_then(|ix| self.branches.get(*ix).cloned());
                if let Some(branch) = pick {
                    self.set_base_ref(branch, cx);
                    self.close_ref_menu(cx);
                }
            }
            _ => {}
        }
    }

    fn request_highlight(
        &mut self,
        file: &FileDiff,
        parsed_key: &str,
        cx: &mut Context<Self>,
    ) -> Option<Arc<DiffHighlights>> {
        let lang = onyx_syntax::language_for_path(&file.path)?;
        let fingerprint = hash64(&[parsed_key, &file.path]);
        if let Some(slot) = self.highlights.get(&file.path)
            && slot.fingerprint == fingerprint
        {
            return match &slot.state {
                DiffHighlightState::Ready(highlights) | DiffHighlightState::Excerpt(highlights) => {
                    Some(highlights.clone())
                }
                DiffHighlightState::Pending | DiffHighlightState::Plain => None,
            };
        }
        if !onyx_syntax::supports_language(lang) {
            self.highlights.insert(
                file.path.clone(),
                HighlightSlot {
                    fingerprint,
                    state: DiffHighlightState::Plain,
                    _excerpt_task: None,
                    _fetch_task: None,
                },
            );
            return None;
        }
        let path = file.path.clone();
        let excerpt_file = file.clone();
        let excerpt_path = path.clone();
        let excerpt_task = cx.spawn(async move |this, cx| {
            let highlights = cx
                .background_executor()
                .spawn(async move { excerpt_highlights(&excerpt_file, lang).map(Arc::new) })
                .await;
            this.update(cx, |changes, cx| {
                if let Some(slot) = changes.highlights.get_mut(&excerpt_path)
                    && slot.fingerprint == fingerprint
                    && matches!(slot.state, DiffHighlightState::Pending)
                {
                    slot.state = match highlights {
                        Some(highlights) => DiffHighlightState::Excerpt(highlights),
                        None => DiffHighlightState::Plain,
                    };
                    cx.notify();
                }
            })
            .ok();
        });

        let active = self.active_diff(cx);
        let engine = self.state.read(cx).engine().cloned();
        let target = self.desired_target(cx);
        let chat_id = self
            .state
            .read(cx)
            .selected_chat_row()
            .map(|chat| chat.id.clone());
        let mode = self.scope.mode().to_string();
        let base_ref = self.base_ref.clone();
        let commit_sha = (self.scope == DiffScope::Commit)
            .then(|| self.commit.as_ref().map(|commit| commit.sha.clone()))
            .flatten();
        let fetch_file = file.clone();
        let fetch_path = path.clone();
        let fetch_task = match (active, engine) {
            (Some(diff), Some(engine)) => Some(cx.spawn(async move |this, cx| {
                let request = zeron_proto::GetCheckoutFileDiffTextRequest {
                    checkout_id: diff.checkout_id,
                    cwd: diff.cwd,
                    path: fetch_path.clone(),
                    mode,
                    base_ref,
                    chat_id,
                    commit_sha,
                    diff_checksum: diff.checksum,
                };
                let mut params = serde_json::to_value(request)
                    .ok()
                    .and_then(|value| value.as_object().cloned())
                    .unwrap_or_default();
                if let Some(target) = target {
                    params.insert("targetDeviceId".into(), serde_json::Value::String(target));
                }
                let response = engine
                    .client()
                    .call(
                        methods::GET_CHECKOUT_FILE_DIFF_TEXT,
                        serde_json::Value::Object(params),
                    )
                    .await
                    .ok()
                    .and_then(|value| {
                        serde_json::from_value::<zeron_proto::CheckoutFileDiffText>(value).ok()
                    });
                let highlights = match response {
                    Some(response) => {
                        cx.background_executor()
                            .spawn(async move {
                                full_highlights(&fetch_file, lang, &response).map(Arc::new)
                            })
                            .await
                    }
                    None => None,
                };
                this.update(cx, |changes, cx| {
                    if let Some(slot) = changes.highlights.get_mut(&fetch_path)
                        && slot.fingerprint == fingerprint
                        && let Some(highlights) = highlights
                    {
                        slot.state = DiffHighlightState::Ready(highlights);
                        cx.notify();
                    }
                })
                .ok();
            })),
            _ => None,
        };
        self.highlights.insert(
            file.path.clone(),
            HighlightSlot {
                fingerprint,
                state: DiffHighlightState::Pending,
                _excerpt_task: Some(excerpt_task),
                _fetch_task: fetch_task,
            },
        );
        None
    }

    fn render_row(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(parsed) = &self.parsed else {
            return gpui::Empty.into_any_element();
        };
        let files = parsed.files.clone();
        let parsed_key = parsed.key.clone();
        let Some(row) = self.rows.get(ix).copied() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        match row {
            DiffRow::FileHeader { file } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                let fold = self.folds.get(&file_diff.path).copied().unwrap_or_default();
                self.render_file_header(file as usize, file_diff, &fold, &theme, cx)
            }
            DiffRow::Notice { file, notice } => files
                .get(file as usize)
                .and_then(|f| file_notices(f).into_iter().nth(notice as usize))
                .map(|text| notice_row(text, &theme))
                .unwrap_or_else(|| gpui::Empty.into_any_element()),
            DiffRow::HunkHeader { file, hunk } => files
                .get(file as usize)
                .and_then(|f| f.hunks.get(hunk as usize))
                .map(|h| hunk_header_row(&h.header, &theme))
                .unwrap_or_else(|| gpui::Empty.into_any_element()),
            DiffRow::Line {
                file,
                hunk,
                line,
                flat: _,
            } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                let highlight = self.request_highlight(file_diff, &parsed_key, cx);
                let Some(line) = file_diff
                    .hunks
                    .get(hunk as usize)
                    .and_then(|h| h.lines.get(line as usize))
                else {
                    return gpui::Empty.into_any_element();
                };
                let spans = highlight
                    .as_deref()
                    .map(|highlights| highlights.spans(line))
                    .unwrap_or(&[]);
                let gutter_px = gutter_width(file_diff);
                let row = diff_line_row(line, spans, &theme, gutter_px);
                let Some((side, line_no)) = line_anchor(line) else {
                    return row;
                };
                let path = file_diff.path.clone();
                let hovered = self.hover.as_ref().is_some_and(|hover| {
                    hover.path == path && hover.side == side && hover.line == line_no
                });
                let move_path = path.clone();
                let leave_path = path.clone();
                div()
                    .id(("diff-line", ix))
                    .w_full()
                    .relative()
                    .child(row)
                    .when(hovered, |el| {
                        el.child(positioned_adder(
                            comment_adder_left(side, gutter_px),
                            render_comment_adder(&path, side, line_no, &theme, cx),
                        ))
                    })
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        this.set_hover(&move_path, Some((side, line_no)), cx);
                    }))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if !*hovered {
                            this.clear_hover_at(&leave_path, side, line_no, cx);
                        }
                    }))
                    .into_any_element()
            }
            DiffRow::CommentCard { file, card } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                let comments = self.comments_for(&file_diff.path, cx);
                match comments.get(card as usize) {
                    Some(comment) => render_comment_card(comment, &theme, cx),
                    None => gpui::Empty.into_any_element(),
                }
            }
            DiffRow::CommentDraft { file } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                match self
                    .draft
                    .as_ref()
                    .filter(|draft| draft.path == file_diff.path)
                {
                    Some(draft) => render_comment_draft(
                        draft_cite_path(draft),
                        draft.line,
                        draft.input.clone(),
                        &theme,
                        cx,
                    ),
                    None => gpui::Empty.into_any_element(),
                }
            }
            DiffRow::BodyPad { .. } => div().w_full().h(px(BODY_BOTTOM_PAD)).into_any_element(),
            DiffRow::FoldingBody { file } => {
                let Some(file_diff) = files.get(file as usize) else {
                    return gpui::Empty.into_any_element();
                };
                let fold = self.folds.get(&file_diff.path).copied().unwrap_or_default();
                let highlight = self.request_highlight(file_diff, &parsed_key, cx);
                let (from, to) = (fold.from, fold.to);
                let cap = from.max(to).min(FOLD_TWEEN_MAX_PX);
                let body = render_file_body_upto(file_diff, highlight, &theme, cap);
                let clipped = div().w_full().overflow_hidden().child(body);
                if fold.animating() {
                    clipped
                        .with_animation(
                            SharedString::from(format!("fold-{}-{}", file_diff.path, fold.epoch)),
                            COLLAPSE.animation(),
                            move |el, t| el.h(px(motion::lerp(from, to, t))),
                        )
                        .into_any_element()
                } else {
                    clipped.h(px(to)).into_any_element()
                }
            }
        }
    }

    fn render_file_header(
        &mut self,
        ix: usize,
        file: &FileDiff,
        fold: &FileFold,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed = fold.collapsed;
        let path = file.path.clone();
        let adds = file.additions;
        let dels = file.deletions;

        let chevron_icon = if collapsed {
            crate::icons::ALT_ARROW_RIGHT
        } else {
            crate::icons::ALT_ARROW_DOWN
        };
        let chevron = div().flex_none().size(px(14.0)).child(
            crate::icons::icon(chevron_icon)
                .size(px(13.0))
                .text_color(theme.text_muted.opacity(0.7)),
        );
        let chevron: AnyElement = if fold.animating() {
            chevron
                .with_animation(
                    SharedString::from(format!("chev-{path}-{}", fold.epoch)),
                    CHEVRON.animation(),
                    |el, t| el.opacity(0.25 + 0.75 * t),
                )
                .into_any_element()
        } else {
            chevron.into_any_element()
        };

        div()
            .id(SharedString::from(format!("file-hdr-{ix}")))
            .w_full()
            .h(px(FILE_HEADER_HEIGHT))
            .when(ix > 0, |el| {
                el.border_t_1().border_color(crate::theme::hairline(0.04))
            })
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(Theme::SPACE_MD))
            .bg(crate::theme::ink(0.025))
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::ink(0.05)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_fold(ix, cx);
                cx.notify();
            }))
            .child(chevron)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(12.0))
                    .text_color(theme.text_dim)
                    .child(SharedString::from(file.path.clone())),
            )
            .when(file.binary, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(10.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("BIN")),
                )
            })
            .when(adds > 0 || !file.binary, |el| {
                el.child(
                    div()
                        .flex_none()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(add_color(theme))
                        .child(SharedString::from(format!("+{adds}"))),
                )
            })
            .when(dels > 0 || !file.binary, |el| {
                el.child(
                    div()
                        .flex_none()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(del_color(theme))
                        .child(SharedString::from(format!("−{dels}"))),
                )
            })
            .into_any_element()
    }

    fn header_button(
        id: &'static str,
        icon_path: &'static str,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                id,
                crate::theme::wash(0.0),
                crate::theme::wash(0.14),
            ))
            .on_hover(motion::hover_listener(id))
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                window.prevent_default()
            })
            .child(
                crate::icons::icon(icon_path)
                    .size(px(14.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            )
    }

    pub fn render_header_controls(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        if let Some(commit) = self.commit.clone() {
            let short: String = commit.sha.chars().take(7).collect();
            return div()
                .size_full()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_none()
                        .h(px(22.0))
                        .px(px(6.0))
                        .rounded(px(5.0))
                        .flex()
                        .items_center()
                        .bg(crate::theme::ink(0.05))
                        .font_family(theme.font_mono.clone())
                        .text_size(px(10.5))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(short)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(theme.text)
                        .child(SharedString::from(commit.subject.clone())),
                )
                .child(
                    Self::header_button("changes-fold-all", crate::icons::FOLD_VERTICAL, &theme)
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_collapse_all(cx);
                        })),
                )
                .into_any_element();
        }
        let scope = self.scope;
        let history_count = (scope == DiffScope::History).then(|| self.history_count(cx));
        let history_fetch_button =
            (scope == DiffScope::History).then(|| self.history_fetch_button(cx));
        let trigger = div()
            .id("changes-scope-trigger")
            .h(px(24.0))
            .px(px(8.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                "changes-scope-trigger",
                crate::theme::wash(0.05),
                crate::theme::wash(0.14),
            ))
            .on_hover(motion::hover_listener("changes-scope-trigger"))
            .occlude()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.scope_menu.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                if this.scope_menu.take_press_was_open() {
                    this.close_scope_menu(cx);
                } else {
                    this.scope_menu.open(());
                }
                cx.notify();
            }))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.text)
                    .child(SharedString::from(scope.label())),
            )
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            );
        let trigger = if self.scope_menu.get().is_some() {
            let closing = self.scope_menu.closing_since();
            let menu = self.render_scope_menu(&theme, cx);
            trigger.relative().child(popover::anchored_menu_below_gap(
                "changes-scope-menu",
                menu,
                closing,
                10.0,
            ))
        } else {
            trigger
        };

        let trailing: AnyElement = if scope == DiffScope::History {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(2.0))
                .children(history_fetch_button)
                .child(
                    Self::header_button("history-refresh", crate::icons::REFRESH, &theme).on_click(
                        cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.history_pane(cx)
                                .update(cx, |history, cx| history.refresh(cx));
                        }),
                    ),
                )
                .into_any_element()
        } else {
            Self::header_button("changes-fold-all", crate::icons::FOLD_VERTICAL, &theme)
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_collapse_all(cx);
                }))
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(trigger)
            .when_some(history_count, |element, count| {
                element.child(div().flex_1().min_w_0().h_full().child(count))
            })
            .children(self.render_ref_selector(&theme, cx))
            .when(scope != DiffScope::History, |element| {
                element.child(div().flex_1())
            })
            .child(trailing)
            .into_any_element()
    }

    fn render_scope_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let current = self.scope;
        popover::popover_card(theme)
            .w(px(180.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_scope_menu(cx)))
            .child(div().flex().flex_col().gap(px(2.0)).children(
                DiffScope::ALL.into_iter().enumerate().map(|(ix, scope)| {
                    popover::menu_row(theme, scope == current, format!("changes-scope-row-{ix}"))
                        .id(("changes-scope-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_scope(scope, cx);
                            this.close_scope_menu(cx);
                        }))
                        .child(div().flex_1().child(SharedString::from(scope.label())))
                }),
            ))
            .into_any_element()
    }

    fn render_ref_selector(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.scope != DiffScope::Branch {
            return None;
        }
        let branch = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|chat| chat.branch.clone())
            .unwrap_or_else(|| "HEAD".to_string());
        let base = self.base_ref.clone().unwrap_or_else(|| "…".to_string());
        let branch_weight = (branch.chars().count().max(1) as f32).powi(2);
        let base_weight = (base.chars().count().max(1) as f32).powi(2);
        let trigger = div()
            .id("changes-ref-trigger")
            .h(px(22.0))
            .px(px(6.0))
            .min_w_0()
            .flex_shrink(base_weight)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .bg(motion::hover_blend(
                "changes-ref-trigger",
                crate::theme::wash(0.0),
                crate::theme::wash(0.12),
            ))
            .on_hover(motion::hover_listener("changes-ref-trigger"))
            .occlude()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, _| {
                    window.prevent_default();
                    this.ref_menu.note_trigger_press();
                }),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                if this.ref_menu.take_press_was_open() {
                    this.close_ref_menu(cx);
                    cx.notify();
                } else {
                    this.open_ref_menu(window, cx);
                }
            }))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.5))
                    .text_color(theme.text)
                    .child(SharedString::from(base)),
            )
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(11.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            );
        let trigger = if self.ref_menu.get().is_some() {
            let closing = self.ref_menu.closing_since();
            let menu = self.render_ref_menu(theme, cx);
            trigger.relative().child(popover::anchored_menu_below_gap(
                "changes-ref-menu",
                menu,
                closing,
                10.0,
            ))
        } else {
            trigger
        };
        Some(
            div()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .ml(px(6.0))
                .child(
                    div()
                        .min_w_0()
                        .flex_shrink(branch_weight)
                        .truncate()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.5))
                        .text_color(theme.text_dim)
                        .child(SharedString::from(branch)),
                )
                .child(
                    crate::icons::icon(crate::icons::ARROW_RIGHT)
                        .size(px(12.0))
                        .flex_none()
                        .text_color(theme.text_faint),
                )
                .child(trigger)
                .into_any_element(),
        )
    }

    fn render_ref_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (search, active, focus, list_scroll) = {
            let Some(menu) = self.ref_menu.get() else {
                return div().into_any_element();
            };
            (
                menu.search.clone(),
                menu.active,
                menu.focus.clone(),
                menu.list_scroll.clone(),
            )
        };
        let rows = self.ref_menu_rows(cx);
        let current = self.base_ref.clone();
        let branches = self.branches.clone();

        let list: AnyElement = if rows.is_empty() {
            div()
                .px(px(8.0))
                .py(px(6.0))
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from(if branches.is_empty() {
                    "No branches"
                } else {
                    "No matching branches"
                }))
                .into_any_element()
        } else {
            div()
                .id("changes-ref-list")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .max_h(px(240.0))
                .overflow_y_scroll()
                .track_scroll(&list_scroll)
                .children(rows.into_iter().enumerate().map(|(row_ix, branch_ix)| {
                    let name = branches[branch_ix].clone();
                    let selected = current.as_deref() == Some(name.as_str());
                    let label = name.clone();
                    popover::menu_row_nav(
                        theme,
                        selected,
                        row_ix == active,
                        format!("changes-ref-row-{row_ix}"),
                    )
                    .id(("changes-ref-row", row_ix))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_base_ref(name.clone(), cx);
                        this.close_ref_menu(cx);
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(12.0))
                            .child(SharedString::from(label)),
                    )
                }))
                .into_any_element()
        };

        popover::popover_card(theme)
            .w(px(240.0))
            .track_focus(&focus)
            .on_key_down(
                cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| this.ref_menu_key(event, cx)),
            )
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_ref_menu(cx)))
            .flex()
            .flex_col()
            .child(popover::search_input_frame(
                theme,
                search.into_any_element(),
            ))
            .child(list)
            .into_any_element()
    }

    fn render_header_strip(&self, theme: &Theme) -> Option<AnyElement> {
        let parsed = self.parsed.as_ref()?;
        Some(
            div()
                .flex_none()
                .h(px(36.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .px(px(Theme::SPACE_LG))
                .border_b_1()
                .border_color(crate::theme::hairline(0.06))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(scope_label(
                            self.scope,
                            parsed.file_count,
                            self.base_ref.as_deref(),
                        ))),
                )
                .child(
                    div()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(add_color(theme))
                        .child(SharedString::from(format!("+{}", parsed.additions))),
                )
                .child(
                    div()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(11.0))
                        .text_color(del_color(theme))
                        .child(SharedString::from(format!("−{}", parsed.deletions))),
                )
                .child(div().flex_1())
                .when(parsed.truncated, |el| {
                    el.child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .bg(theme.warning.opacity(0.08))
                            .text_color(theme.warning.opacity(0.75))
                            .child(SharedString::from("Partial snapshot")),
                    )
                })
                .into_any_element(),
        )
    }
}

fn add_color(theme: &Theme) -> gpui::Hsla {
    theme.diff_add
}

fn del_color(theme: &Theme) -> gpui::Hsla {
    theme.diff_del
}

fn notice_row(notice: String, theme: &Theme) -> AnyElement {
    div()
        .h(px(NOTICE_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .px(px(Theme::SPACE_LG))
        .text_size(px(11.0))
        .text_color(theme.text_faint)
        .child(SharedString::from(notice))
        .into_any_element()
}

fn hunk_header_row(header: &str, theme: &Theme) -> AnyElement {
    div()
        .h(px(HUNK_HEADER_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .px(px(Theme::SPACE_LG))
        .bg(theme.diff_hunk_bg)
        .font_family(theme.font_mono.clone())
        .text_size(px(11.0))
        .text_color(theme.text_faint)
        .child(SharedString::from(header.to_string()))
        .into_any_element()
}

fn diff_line_row(
    line: &DiffLine,
    spans: &[onyx_syntax::HighlightSpan],
    theme: &Theme,
    gutter_px: f32,
) -> AnyElement {
    if line.kind == LineKind::Meta {
        return div()
            .h(px(DIFF_LINE_HEIGHT))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .pl(px(ACCENT_BAR_WIDTH + 2.0 * gutter_px + MARKER_WIDTH + 12.0))
            .text_size(px(10.5))
            .text_color(theme.text_faint)
            .italic()
            .child(SharedString::from(line.text.clone()))
            .into_any_element();
    }

    let mut add_bg = add_color(theme);
    add_bg.a = 0.055;
    let mut del_bg = del_color(theme);
    del_bg.a = 0.055;

    let (marker, marker_color, row_bg, accent, number_color) = match line.kind {
        LineKind::Add => (
            "+",
            add_color(theme),
            Some(add_bg),
            Some(add_color(theme).opacity(0.55)),
            add_color(theme).opacity(0.9),
        ),
        LineKind::Del => (
            "−",
            del_color(theme),
            Some(del_bg),
            Some(del_color(theme).opacity(0.55)),
            del_color(theme).opacity(0.9),
        ),
        _ => (
            "·",
            theme.text_faint.opacity(0.5),
            None,
            None,
            theme.text_faint.opacity(0.8),
        ),
    };
    let gutter = |no: Option<u32>, color: gpui::Hsla| {
        div()
            .w(px(gutter_px))
            .flex_none()
            .font_family(theme.font_mono.clone())
            .text_size(px(11.0))
            .text_color(color)
            .flex()
            .justify_end()
            .pr(px(8.0))
            .child(SharedString::from(
                no.map(|n| n.to_string()).unwrap_or_default(),
            ))
    };
    let mono = font(theme.font_mono.clone());
    let runs = render::runs_for_syntax_line_with_plain(
        &line.text,
        spans,
        &mono,
        theme.text.opacity(0.92),
        theme,
    );
    div()
        .h(px(DIFF_LINE_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .when_some(row_bg, |el, bg| el.bg(bg))
        .child(
            div()
                .w(px(ACCENT_BAR_WIDTH))
                .h_full()
                .flex_none()
                .when_some(accent, |el, color| el.bg(color)),
        )
        .child(gutter(
            line.old_no,
            if line.kind == LineKind::Del {
                number_color
            } else {
                theme.text_faint.opacity(0.8)
            },
        ))
        .child(gutter(
            line.new_no,
            if line.kind == LineKind::Add {
                number_color
            } else {
                theme.text_faint.opacity(0.8)
            },
        ))
        .child(
            div()
                .w(px(MARKER_WIDTH))
                .flex_none()
                .flex()
                .justify_center()
                .text_size(px(DIFF_TEXT_SIZE))
                .text_color(marker_color)
                .font_family(theme.font_mono.clone())
                .child(SharedString::from(marker)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .pl(px(12.0))
                .font_family(theme.font_mono.clone())
                .text_size(px(DIFF_TEXT_SIZE))
                .whitespace_nowrap()
                .child(gpui::StyledText::new(line.text.clone()).with_runs(runs)),
        )
        .into_any_element()
}

pub const COMMENT_ADDER_SIZE: f32 = 16.0;

pub fn comment_adder_left(side: CommentSide, gutter_px: f32) -> f32 {
    let column = match side {
        CommentSide::Old => 0.0,
        CommentSide::New => gutter_px,
    };
    ACCENT_BAR_WIDTH + column + (gutter_px - COMMENT_ADDER_SIZE) / 2.0
}

fn positioned_adder(left: f32, adder: AnyElement) -> gpui::Div {
    div()
        .absolute()
        .left(px(left))
        .top(px(0.0))
        .h_full()
        .flex()
        .items_center()
        .child(adder)
}

fn render_comment_adder(
    path: &str,
    side: CommentSide,
    line: u32,
    theme: &Theme,
    cx: &Context<Changes>,
) -> AnyElement {
    let target = path.to_string();
    div()
        .id(SharedString::from(format!(
            "cmt-add-{path}-{}-{line}",
            side.tag()
        )))
        .size(px(COMMENT_ADDER_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .bg(theme.solid)
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_draft(target.clone(), side, line, window, cx);
        }))
        .child(
            crate::icons::icon(crate::icons::PLUS)
                .size(px(11.0))
                .text_color(theme.on_solid),
        )
        .into_any_element()
}

fn render_comment_card(comment: &DiffComment, theme: &Theme, cx: &Context<Changes>) -> AnyElement {
    let group: SharedString = format!("cmt-card-{}", comment.id).into();
    let id = comment.id.clone();
    div()
        .group(group.clone())
        .h(px(comments::card_height(&comment.body)))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .bg(crate::theme::ink(0.05))
        .child(comment_accent_bar(theme.solid.opacity(0.35)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .px(px(Theme::SPACE_LG))
                .py(px(comments::CARD_PAD_V / 2.0))
                .child(
                    div()
                        .h(px(comments::CARD_HEADER_HEIGHT))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                                .size(px(12.0))
                                .text_color(theme.text_faint),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(comment.location())),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("cmt-remove-{}", comment.id)))
                                .flex_none()
                                .size(px(16.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.0))
                                .cursor_pointer()
                                .opacity(0.0)
                                .group_hover(group, |s| s.opacity(1.0))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.remove_comment(&id, cx)),
                                )
                                .child(
                                    crate::icons::icon(crate::icons::CLOSE_CIRCLE)
                                        .size(px(12.0))
                                        .text_color(theme.text_muted),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .text_size(px(12.0))
                        .line_height(px(comments::CARD_LINE_HEIGHT))
                        .text_color(theme.text_dim)
                        .child(SharedString::from(comment.body.clone())),
                ),
        )
        .into_any_element()
}

fn comment_accent_bar(color: gpui::Hsla) -> gpui::Div {
    div().w(px(ACCENT_BAR_WIDTH)).h_full().flex_none().bg(color)
}

fn draft_cite_path(draft: &CommentDraft) -> &str {
    match draft.side {
        CommentSide::Old => draft.old_path.as_deref().unwrap_or(&draft.path),
        CommentSide::New => &draft.path,
    }
}

fn render_comment_draft(
    path: &str,
    line: u32,
    input: Entity<ComposerInput>,
    theme: &Theme,
    cx: &Context<Changes>,
) -> AnyElement {
    div()
        .h(px(comments::DRAFT_CARD_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .flex_row()
        .bg(crate::theme::ink(0.08))
        .child(comment_accent_bar(theme.solid.opacity(0.7)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .px(px(Theme::SPACE_LG))
                .py(px(10.0))
                .child(
                    div()
                        .h(px(comments::CARD_HEADER_HEIGHT))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            crate::icons::icon(crate::icons::CHAT_ROUND_LINE)
                                .size(px(12.0))
                                .text_color(theme.text_faint),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(theme.font_mono.clone())
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(format!("{path}:{line}"))),
                        ),
                )
                .child(
                    div()
                        .h(px(46.0))
                        .flex_none()
                        .overflow_hidden()
                        .text_size(px(12.0))
                        .child(input.into_any_element()),
                )
                .child(
                    div()
                        .h(px(28.0))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_end()
                        .gap(px(6.0))
                        .child(
                            comment_action("cmt-cancel", "Cancel", false, theme)
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_draft(cx))),
                        )
                        .child(
                            comment_action("cmt-commit", "Comment", true, theme)
                                .on_click(cx.listener(|this, _, _, cx| this.commit_draft(cx))),
                        ),
                ),
        )
        .into_any_element()
}

fn comment_action(
    id: &'static str,
    label: &'static str,
    primary: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(22.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .cursor_pointer()
        .when(primary, |el| el.bg(theme.solid).text_color(theme.on_solid))
        .when(!primary, |el| {
            el.text_color(motion::hover_blend(id, theme.text_muted, theme.text))
                .bg(motion::hover_blend(
                    id,
                    gpui::transparent_black(),
                    theme.element_hover,
                ))
                .on_hover(motion::hover_listener(id))
        })
        .child(SharedString::from(label))
}

pub(crate) fn render_file_body_with_syntax(
    file: &FileDiff,
    highlights: Option<Arc<DiffHighlights>>,
    theme: &Theme,
) -> AnyElement {
    let mut children: Vec<AnyElement> = Vec::new();
    let gutter_px = gutter_width(file);
    for notice in file_notices(file) {
        children.push(notice_row(notice, theme));
    }
    for hunk in &file.hunks {
        children.push(hunk_header_row(&hunk.header, theme));
        for line in &hunk.lines {
            let spans = highlights
                .as_deref()
                .map(|highlights| highlights.spans(line))
                .unwrap_or(&[]);
            children.push(diff_line_row(line, spans, theme, gutter_px));
        }
    }
    div()
        .flex()
        .flex_col()
        .pb(px(BODY_BOTTOM_PAD))
        .children(children)
        .into_any_element()
}

fn render_file_body_upto(
    file: &FileDiff,
    highlight: Option<Arc<DiffHighlights>>,
    theme: &Theme,
    max_px: f32,
) -> AnyElement {
    let mut children: Vec<AnyElement> = Vec::new();
    let mut y = 0.0f32;
    let gutter_px = gutter_width(file);

    'build: {
        for notice in file_notices(file) {
            if y >= max_px {
                break 'build;
            }
            children.push(notice_row(notice, theme));
            y += NOTICE_HEIGHT;
        }
        for hunk in &file.hunks {
            if y >= max_px {
                break 'build;
            }
            children.push(hunk_header_row(&hunk.header, theme));
            y += HUNK_HEADER_HEIGHT;
            for line in &hunk.lines {
                if y >= max_px {
                    break 'build;
                }
                let spans = highlight
                    .as_deref()
                    .map(|highlights| highlights.spans(line))
                    .unwrap_or(&[]);
                children.push(diff_line_row(line, spans, theme, gutter_px));
                y += DIFF_LINE_HEIGHT;
            }
        }
    }

    div()
        .flex()
        .flex_col()
        .pb(px(BODY_BOTTOM_PAD))
        .children(children)
        .into_any_element()
}

impl Render for Changes {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.scope == DiffScope::History {
            let history = self.history_pane(cx);
            history.update(cx, |history, cx| history.ensure_loaded(cx));
            return div().size_full().child(history).into_any_element();
        }
        let theme = Theme::of(cx).clone();
        let active = self.active_diff(cx);
        let scope = self.scope;
        let base = self.base_ref.clone();
        let no_chat = self.state.read(cx).selected_chat_row().is_none();
        let phase = if no_chat {
            DiffPhase::Clean
        } else {
            diff_phase(active.as_ref())
        };
        let error = self.error.clone();
        let scoped_notice: Option<(SharedString, bool)> = (!no_chat
            && scope != DiffScope::WorkingTree)
            .then(|| self.scoped_error.clone())
            .flatten()
            .map(|message| {
                if message.contains("no turn recorded") {
                    (
                        SharedString::from("No turn recorded yet — send a message first"),
                        false,
                    )
                } else if message.contains("unknown method") {
                    (
                        SharedString::from(
                            "This chat's device is running an older Zeron — update it to view branch and turn diffs",
                        ),
                        false,
                    )
                } else {
                    (message, true)
                }
            });

        let content: AnyElement = if let Some((message, warn)) = scoped_notice {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px(px(Theme::SPACE_LG))
                .text_size(px(12.0))
                .text_color(if warn {
                    theme.warning.opacity(0.85)
                } else {
                    theme.text_faint
                })
                .child(message)
                .into_any_element()
        } else {
            match phase {
                DiffPhase::Preparing => div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(crate::loaders::gradient_spinner(
                        "changes-preparing",
                        &theme,
                        3.0,
                        cx.entity_id(),
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_faint)
                            .child(SharedString::from("Preparing diff…")),
                    )
                    .into_any_element(),
                DiffPhase::Clean => div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(clean_message(scope, base.as_deref())))
                    .into_any_element(),
                DiffPhase::List => {
                    if self.parsed.is_some() {
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .children(self.render_header_strip(&theme))
                            .child(
                                list(self.list.clone(), cx.processor(Self::render_row))
                                    .flex_1()
                                    .with_sizing_behavior(gpui::ListSizingBehavior::Auto),
                            )
                            .into_any_element()
                    } else {
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(crate::loaders::gradient_spinner(
                                "changes-parsing",
                                &theme,
                                3.0,
                                cx.entity_id(),
                                cx,
                            ))
                            .into_any_element()
                    }
                }
            }
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .flex_none()
                        .px(px(Theme::SPACE_MD))
                        .py(px(4.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .text_size(px(11.0))
                        .text_color(theme.warning)
                        .child(message),
                )
            })
            .child(content)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    const PATCH: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 111..222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,5 @@ fn main
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
+    let x = 1;
 }
@@ -10,2 +11,2 @@
 // tail
-old_line
+new_line
diff --git a/added.txt b/added.txt
new file mode 100644
--- /dev/null
+++ b/added.txt
@@ -0,0 +1,2 @@
+first
+second
\\ No newline at end of file
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-bye
diff --git a/img.png b/img.png
new file mode 100644
Binary files /dev/null and b/img.png differ
diff --git a/old_name.rs b/new_name.rs
similarity index 90%
rename from old_name.rs
rename to new_name.rs
";

    #[test]
    fn parses_files_hunks_and_lines() {
        let files = parse_patch(PATCH);
        assert_eq!(files.len(), 5);

        let main = &files[0];
        assert_eq!(main.path, "src/main.rs");
        assert_eq!(main.status, FileStatus::Modified);
        assert_eq!(main.hunks.len(), 2);
        assert_eq!(main.additions, 3);
        assert_eq!(main.deletions, 2);
        let h0 = &main.hunks[0];
        assert_eq!(h0.header, "@@ -1,4 +1,5 @@ fn main");
        assert_eq!(h0.lines.len(), 5);
        assert_eq!(h0.lines[0].kind, LineKind::Context);
        assert_eq!(h0.lines[0].old_no, Some(1));
        assert_eq!(h0.lines[0].new_no, Some(1));
        assert_eq!(h0.lines[1].kind, LineKind::Del);
        assert_eq!(h0.lines[1].old_no, Some(2));
        assert_eq!(h0.lines[1].new_no, None);
        assert_eq!(h0.lines[2].kind, LineKind::Add);
        assert_eq!(h0.lines[2].new_no, Some(2));
        assert_eq!(h0.lines[3].kind, LineKind::Add);
        assert_eq!(h0.lines[3].new_no, Some(3));
        assert_eq!(h0.lines[4].old_no, Some(3));
        assert_eq!(h0.lines[4].new_no, Some(4));
        assert_eq!(main.hunks[1].lines[0].old_no, Some(10));
        assert_eq!(main.hunks[1].lines[0].new_no, Some(11));
    }

    #[test]
    fn detects_new_deleted_binary_and_renamed() {
        let files = parse_patch(PATCH);
        let added = &files[1];
        assert_eq!(added.status, FileStatus::Added);
        assert_eq!(added.additions, 2);
        let last = added.hunks[0].lines.last().unwrap();
        assert_eq!(last.kind, LineKind::Meta);
        assert!(last.text.contains("No newline"));
        assert!(file_notices(added).iter().any(|n| n == "New file"));

        let deleted = &files[2];
        assert_eq!(deleted.status, FileStatus::Deleted);
        assert_eq!(deleted.deletions, 1);
        assert!(file_notices(deleted).iter().any(|n| n == "Deleted file"));

        let binary = &files[3];
        assert!(binary.binary);
        assert_eq!(binary.status, FileStatus::Added);
        assert!(binary.hunks.is_empty());
        assert!(file_notices(binary).iter().any(|n| n.contains("Binary")));

        let renamed = &files[4];
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.path, "new_name.rs");
        assert_eq!(renamed.old_path.as_deref(), Some("old_name.rs"));
        assert!(
            file_notices(renamed)
                .iter()
                .any(|n| n.contains("old_name.rs"))
        );
    }

    #[test]
    fn empty_and_garbage_patches_parse_to_nothing() {
        assert!(parse_patch("").is_empty());
        assert!(parse_patch("not a diff\nat all\n").is_empty());
        let files = parse_patch("diff --git a/x b/x\n@@ -1,9 +1,9 @@\n ctx\n+add");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].hunks[0].lines.len(), 2);
        assert_eq!(files[0].additions, 1);
    }

    #[test]
    fn quoted_and_spaced_paths() {
        let (old, new) = parse_git_paths("a/simple.rs b/simple.rs");
        assert_eq!((old.as_str(), new.as_str()), ("simple.rs", "simple.rs"));
        let (old, new) = parse_git_paths("\"a/with space.rs\" \"b/with space.rs\"");
        assert_eq!(old, "with space.rs");
        assert_eq!(new, "with space.rs");
    }

    #[test]
    fn hunk_headers_parse_with_and_without_counts() {
        assert_eq!(parse_hunk_header("@@ -1,4 +2,5 @@"), Some((1, 2)));
        assert_eq!(parse_hunk_header("@@ -7 +9 @@ fn ctx"), Some((7, 9)));
        assert_eq!(parse_hunk_header("@@ garbage"), None);
    }

    #[test]
    fn rows_flatten_to_line_granularity() {
        let files = parse_patch(PATCH);
        let (rows, ranges) = flatten_rows(&files, &[], None, |_| false);
        assert_eq!(ranges.len(), files.len());
        for (ix, range) in ranges.iter().enumerate() {
            assert_eq!(rows[range.start], DiffRow::FileHeader { file: ix as u32 });
            assert_eq!(range.len(), 1 + body_row_count(&files[ix]));
        }
        assert_eq!(ranges.last().unwrap().end, rows.len());

        let main_rows = &rows[ranges[0].clone()];
        assert_eq!(main_rows.len(), 1 + 2 + 8 + 1);
        assert_eq!(main_rows[1], DiffRow::HunkHeader { file: 0, hunk: 0 });
        let flats: Vec<u32> = main_rows
            .iter()
            .filter_map(|r| match r {
                DiffRow::Line { flat, .. } => Some(*flat),
                _ => None,
            })
            .collect();
        assert_eq!(flats, (0..8).collect::<Vec<u32>>());
        assert_eq!(*main_rows.last().unwrap(), DiffRow::BodyPad { file: 0 });

        let (rows, ranges) = flatten_rows(&files, &[], None, |ix| ix == 0);
        assert_eq!(ranges[0].len(), 1);
        assert_eq!(rows[ranges[1].start], DiffRow::FileHeader { file: 1 });

        let added_rows = &rows[ranges[1].clone()];
        assert_eq!(added_rows[1], DiffRow::Notice { file: 1, notice: 0 });
    }

    #[test]
    fn truncate_caps_lines_and_appends_notice() {
        let mut file = parse_patch(PATCH).remove(0);
        let untouched = file.clone();
        truncate_file_lines(&mut file, 10);
        assert_eq!(file, untouched, "under the cap: untouched");

        truncate_file_lines(&mut file, 6);
        let lines: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(lines, 6);
        assert_eq!(file.hunks.len(), 2);
        assert!(
            file_notices(&file)
                .iter()
                .any(|n| n.contains("first 6 of 8 lines"))
        );
        assert_eq!(
            body_height(&file),
            NOTICE_HEIGHT + 2.0 * HUNK_HEADER_HEIGHT + 6.0 * DIFF_LINE_HEIGHT + BODY_BOTTOM_PAD
        );

        let mut file = parse_patch(PATCH).remove(0);
        truncate_file_lines(&mut file, 3);
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].lines.len(), 3);
    }

    #[test]
    fn gutters_fit_the_largest_line_number() {
        let files = parse_patch(PATCH);
        assert_eq!(files[0].max_line, 12);
        assert_eq!(gutter_width(&files[0]), GUTTER_WIDTH);

        let mut file = files[0].clone();
        for digits in 1..=7u32 {
            file.max_line = 10u32.pow(digits) - 1;
            let w = gutter_width(&file);
            assert!(w >= GUTTER_WIDTH);
            let left_gap = w - (digits as f32 * 6.6 + 8.0);
            assert!(
                left_gap >= 6.0,
                "{digits} digits: left gap {left_gap} < 6px"
            );
        }
        file.max_line = 9999;
        assert!(gutter_width(&file) > GUTTER_WIDTH);
        file.max_line = 27404;
        assert!(
            gutter_width(&file)
                > gutter_width(&{
                    let mut f = file.clone();
                    f.max_line = 9999;
                    f
                })
        );

        let mut file = files[0].clone();
        truncate_file_lines(&mut file, 3);
        assert_eq!(file.max_line, 2);
    }

    #[test]
    fn body_height_is_analytic() {
        let files = parse_patch(PATCH);
        let main = &files[0];
        let lines: usize = main.hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(
            body_height(main),
            2.0 * HUNK_HEADER_HEIGHT + lines as f32 * DIFF_LINE_HEIGHT + BODY_BOTTOM_PAD
        );
        let added = &files[1];
        assert_eq!(
            body_height(added),
            NOTICE_HEIGHT + HUNK_HEADER_HEIGHT + 3.0 * DIFF_LINE_HEIGHT + BODY_BOTTOM_PAD
        );
    }

    fn diff(checkout: &str, device: &str, cwd: &str, patch: &str) -> CheckoutDiff {
        CheckoutDiff {
            checkout_id: checkout.into(),
            device_id: device.into(),
            cwd: cwd.into(),
            patch: patch.into(),
            files: Vec::new(),
            additions: 0,
            deletions: 0,
            truncated: false,
            checksum: format!("sum-{}", patch.len()),
            updated_at: Utc::now(),
        }
    }

    fn chat(checkout: Option<&str>, device: &str, cwd: Option<&str>) -> Chat {
        Chat {
            id: "c1".into(),
            device_id: device.into(),
            title: None,
            archived: false,
            cwd: cwd.map(Into::into),
            branch: None,
            checkout_id: checkout.map(Into::into),
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
        }
    }

    #[test]
    fn diff_resolution_prefers_checkout_id_then_cwd() {
        let diffs = vec![
            diff("co-1", "dev-a", "/repo/one", "x"),
            diff("co-2", "dev-b", "/repo/two", "y"),
        ];
        let c = chat(Some("co-2"), "dev-a", Some("/repo/one"));
        assert_eq!(resolve_diff(&diffs, &c).unwrap().checkout_id, "co-2");
        let c = chat(Some("co-9"), "dev-a", Some("/repo/one"));
        assert_eq!(resolve_diff(&diffs, &c).unwrap().checkout_id, "co-1");
        let c = chat(None, "dev-z", Some("/repo/two"));
        assert_eq!(resolve_diff(&diffs, &c).unwrap().checkout_id, "co-2");
        let c = chat(None, "dev-a", None);
        assert!(resolve_diff(&diffs, &c).is_none());
        let c = chat(None, "dev-a", Some("/elsewhere"));
        assert!(resolve_diff(&diffs, &c).is_none());
    }

    #[test]
    fn phases() {
        assert_eq!(diff_phase(None), DiffPhase::Preparing);
        let clean = diff("co", "d", "/w", "  \n");
        assert_eq!(diff_phase(Some(&clean)), DiffPhase::Clean);
        let full = diff("co", "d", "/w", "diff --git a/x b/x\n");
        assert_eq!(diff_phase(Some(&full)), DiffPhase::List);
        let mut summarized = diff("co", "d", "/w", "");
        summarized.files.push(zeron_proto::DiffFileSummary {
            path: "x".into(),
            old_path: None,
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            binary: false,
        });
        assert_eq!(diff_phase(Some(&summarized)), DiffPhase::List);
    }

    #[test]
    fn header_label_pluralizes() {
        assert_eq!(uncommitted_label(0), "0 Uncommitted changes");
        assert_eq!(uncommitted_label(1), "1 Uncommitted change");
        assert_eq!(uncommitted_label(4), "4 Uncommitted changes");
    }

    #[test]
    fn scope_labels_and_clean_messages() {
        assert_eq!(
            scope_label(DiffScope::WorkingTree, 2, None),
            "2 Uncommitted changes"
        );
        assert_eq!(
            scope_label(DiffScope::Branch, 1, Some("main")),
            "1 Changed file vs main"
        );
        assert_eq!(scope_label(DiffScope::Branch, 3, None), "3 Changed files");
        assert_eq!(
            scope_label(DiffScope::LatestTurn, 2, None),
            "2 Changed files this turn"
        );
        assert_eq!(
            clean_message(DiffScope::WorkingTree, None),
            "No uncommitted changes"
        );
        assert_eq!(
            clean_message(DiffScope::Branch, Some("develop")),
            "No changes vs develop"
        );
        assert_eq!(
            clean_message(DiffScope::LatestTurn, None),
            "No changes this turn"
        );
    }

    #[test]
    fn base_ref_defaults_to_repo_default_then_main() {
        let branches =
            |names: &[&str]| -> Vec<String> { names.iter().map(|n| n.to_string()).collect() };
        let b = branches(&["main", "feature"]);
        assert_eq!(
            default_base_ref(&b, Some("feature")).as_deref(),
            Some("main")
        );
        let b = branches(&["feature", "main"]);
        assert_eq!(
            default_base_ref(&b, Some("feature")).as_deref(),
            Some("main")
        );
        let b = branches(&["feature", "master"]);
        assert_eq!(
            default_base_ref(&b, Some("feature")).as_deref(),
            Some("master")
        );
        let b = branches(&["feature", "develop"]);
        assert_eq!(
            default_base_ref(&b, Some("feature")).as_deref(),
            Some("develop")
        );
        let b = branches(&["main", "feature"]);
        assert_eq!(default_base_ref(&b, Some("main")).as_deref(), Some("main"));
        let b = branches(&["main"]);
        assert_eq!(default_base_ref(&b, Some("main")).as_deref(), Some("main"));
        assert_eq!(default_base_ref(&[], Some("main")), None);
    }

    #[test]
    fn scope_modes_are_wire_stable() {
        assert_eq!(DiffScope::WorkingTree.mode(), "workingTree");
        assert_eq!(DiffScope::Branch.mode(), "branch");
        assert_eq!(DiffScope::LatestTurn.mode(), "turn");
        assert_eq!(DiffScope::default(), DiffScope::WorkingTree);
    }

    #[test]
    fn diff_frames_replace_lists_and_upsert_singles() {
        let mut diffs = Vec::new();
        let one = diff("co-1", "d", "/w", "p1");
        assert!(apply_diff_frame(
            &mut diffs,
            serde_json::to_value(&one).unwrap()
        ));
        assert_eq!(diffs.len(), 1);
        assert!(!apply_diff_frame(
            &mut diffs,
            serde_json::to_value(&one).unwrap()
        ));
        let mut updated = one.clone();
        updated.patch = "p2".into();
        assert!(apply_diff_frame(
            &mut diffs,
            serde_json::to_value(&updated).unwrap()
        ));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].patch, "p2");
        let two = diff("co-2", "d", "/x", "q");
        assert!(apply_diff_frame(
            &mut diffs,
            serde_json::to_value(vec![two.clone()]).unwrap()
        ));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].checkout_id, "co-2");
        assert!(!apply_diff_frame(
            &mut diffs,
            serde_json::json!({"nope": true})
        ));
        assert_eq!(diffs[0].checkout_id, "co-2");
    }

    #[test]
    fn full_diff_highlights_map_old_new_and_context_by_source_line() {
        let old_source = "fn old() {\n    let value = 1;\n}\n";
        let new_source = "fn new() {\n    let value = 2;\n}\n";
        let parse = |source| {
            Arc::new(
                onyx_syntax::highlight(onyx_syntax::HighlightRequest {
                    source,
                    path: Some("src/lib.rs"),
                    fence_tag: None,
                })
                .unwrap(),
            )
        };
        let highlights = DiffHighlights {
            old: Some(parse(old_source)),
            new: Some(parse(new_source)),
        };
        let deleted = DiffLine {
            kind: LineKind::Del,
            old_no: Some(1),
            new_no: None,
            text: "fn old() {".into(),
        };
        let added = DiffLine {
            kind: LineKind::Add,
            old_no: None,
            new_no: Some(1),
            text: "fn new() {".into(),
        };
        let context = DiffLine {
            kind: LineKind::Context,
            old_no: Some(2),
            new_no: Some(2),
            text: "    let value = 2;".into(),
        };
        assert_eq!(
            highlights.source_ref(&deleted),
            Some(SourceLineRef {
                side: SourceSide::Old,
                line_number: 1
            })
        );
        assert_eq!(
            highlights.source_ref(&added),
            Some(SourceLineRef {
                side: SourceSide::New,
                line_number: 1
            })
        );
        assert_eq!(
            highlights.source_ref(&context),
            Some(SourceLineRef {
                side: SourceSide::New,
                line_number: 2
            })
        );
        assert!(
            highlights
                .spans(&deleted)
                .iter()
                .any(|span| span.kind == onyx_syntax::HighlightKind::Function)
        );
        assert!(
            highlights
                .spans(&added)
                .iter()
                .any(|span| span.kind == onyx_syntax::HighlightKind::Function)
        );
    }

    #[test]
    fn excerpt_parses_old_and_new_hunks_as_separate_documents() {
        let file = FileDiff {
            path: "src/lib.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            notices: vec![],
            hunks: vec![Hunk {
                header: "@@ -1,3 +1,3 @@".into(),
                lines: vec![
                    DiffLine {
                        kind: LineKind::Context,
                        old_no: Some(1),
                        new_no: Some(1),
                        text: "/* start".into(),
                    },
                    DiffLine {
                        kind: LineKind::Del,
                        old_no: Some(2),
                        new_no: None,
                        text: "old body".into(),
                    },
                    DiffLine {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(2),
                        text: "new body".into(),
                    },
                    DiffLine {
                        kind: LineKind::Context,
                        old_no: Some(3),
                        new_no: Some(3),
                        text: "end */".into(),
                    },
                ],
            }],
            additions: 1,
            deletions: 1,
            max_line: 3,
        };
        let highlights = excerpt_highlights(&file, Lang::Rust).expect("excerpt");
        let deleted = &file.hunks[0].lines[1];
        let added = &file.hunks[0].lines[2];
        assert!(
            highlights
                .spans(deleted)
                .iter()
                .any(|span| span.kind == onyx_syntax::HighlightKind::Comment)
        );
        assert!(
            highlights
                .spans(added)
                .iter()
                .any(|span| span.kind == onyx_syntax::HighlightKind::Comment)
        );
    }

    #[test]
    fn mismatched_full_sources_are_rejected_atomically() {
        let file = FileDiff {
            path: "src/lib.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            notices: vec![],
            hunks: vec![Hunk {
                header: "@@ -1 +1 @@".into(),
                lines: vec![
                    DiffLine {
                        kind: LineKind::Del,
                        old_no: Some(1),
                        new_no: None,
                        text: "let old = 1;".into(),
                    },
                    DiffLine {
                        kind: LineKind::Add,
                        old_no: None,
                        new_no: Some(1),
                        text: "let new = 2;".into(),
                    },
                ],
            }],
            additions: 1,
            deletions: 1,
            max_line: 1,
        };
        let response = zeron_proto::CheckoutFileDiffText {
            diff_checksum: "sum".into(),
            old_text: Some("let old = 1;\n".into()),
            new_text: Some("different snapshot\n".into()),
            old_content_hash: None,
            new_content_hash: None,
            binary: false,
            truncated: false,
            stale: false,
        };
        assert!(!sources_match_patch(&file, &response));
        assert!(full_highlights(&file, Lang::Rust, &response).is_none());
    }
}
