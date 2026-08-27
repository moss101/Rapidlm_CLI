//! Read-only diff panel over `MergePreview` / `PatchSummary`.
//!
//! [`DiffViewModel`] is a frontend projection: it never stages, applies, or
//! rolls back a change set. Large and binary bodies degrade to a metadata
//! plus artifact link. Unattributed shell mutations are labeled distinctly.
//! Untrusted path/content bytes are sanitized before they become visible.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Display, Formatter};

use agent_runtime::PatchSummary;
use protocol::{ArtifactId, WorkspaceViewId};
use workspace::{
    ExternalMutation, MergeConflict, MergeConflictKind, MergeOpKind, MergePreview, MutationKind,
    PatchOp,
};

use crate::sanitize::sanitize_untrusted;
use crate::state::CancellationToken;

/// Maximum files retained in one viewer.
pub const MAX_DIFF_FILES: usize = 4096;

/// Maximum UTF-8 bytes inlined for one file body.
pub const MAX_INLINE_DIFF_BYTES: usize = 64 * 1024;

/// Maximum lines inlined for one file body.
pub const MAX_INLINE_DIFF_LINES: usize = 1024;

/// Render width is clamped to this many columns.
pub const MAX_DIFF_COLS: u16 = 512;

/// Render height is clamped to this many rows.
pub const MAX_DIFF_ROWS: u16 = 256;

const CANCEL_STRIDE: usize = 8;
const ARTIFACT_PREFIX: &str = "artifact:";
const UNATTRIBUTED_LABEL: &str = "UNATTRIBUTED";
const ATTRIBUTED_LABEL: &str = "attributed";

/// How the selected file body is formatted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum DiffRenderMode {
    #[default]
    Unified,
    Semantic,
}

/// File-list cursor and body mode. Local chrome only.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct DiffSelection {
    file_index: usize,
    mode: DiffRenderMode,
}

/// Why an inline body was withheld.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DegradeReason {
    Binary,
    TooLarge,
}

/// Closed semantic operation shown in the file list.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SemanticOpKind {
    ReplaceRange,
    CreateFile,
    DeleteFile,
    MoveFile,
    Added,
    Modified,
    Deleted,
    Conflict,
}

/// How a file entered the viewer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DiffFileKind {
    Semantic,
    Conflict,
    External,
}

/// Provenance shown next to a path. Unattributed is a distinct token.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DiffAttribution {
    SemanticPatch,
    UnattributedShell,
}

/// Kind of a rendered body line.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DiffLineKind {
    Meta,
    Add,
    Delete,
    Context,
}

/// Which body the viewer would paint for the selected file.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DiffBodyKind {
    Unified,
    Semantic,
    Artifact,
}

/// Banner class. Unattributed shell mutations are never folded into ops.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DiffWarningKind {
    UnattributedShell,
    MergeConflict,
}

/// Typed viewer failure. Display never echoes paths or payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DiffError {
    Cancelled,
    BoundExceeded,
    InvalidSelection,
}

/// One projected path. Bodies are sanitized and bounded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffFile {
    path: String,
    dest: Option<String>,
    kind: DiffFileKind,
    op: SemanticOpKind,
    attribution: DiffAttribution,
    conflict_kind: Option<MergeConflictKind>,
    child_op: Option<MergeOpKind>,
    parent_op: Option<MergeOpKind>,
    unified: Vec<DiffLine>,
    semantic: Vec<String>,
    artifact: Option<ArtifactId>,
    artifact_bytes: Option<u64>,
    degrade: Option<DegradeReason>,
}

/// One sanitized unified-diff line.
#[derive(Clone, Debug, Eq, PartialEq)]
struct DiffLine {
    kind: DiffLineKind,
    text: String,
}

/// Observational warning. Not a capability grant.
#[derive(Clone, Debug, Eq, PartialEq)]
struct DiffWarning {
    kind: DiffWarningKind,
    path: String,
}

/// Frontend-only projection. Never issues apply/rollback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffViewModel {
    files: Vec<DiffFile>,
    selection: DiffSelection,
    files_changed: u32,
    additions: u32,
    deletions: u32,
    warnings: Vec<DiffWarning>,
    preview_hash: Option<ArtifactId>,
    parent_revision: Option<String>,
    child_view_id: Option<WorkspaceViewId>,
    parent_view_id: Option<WorkspaceViewId>,
    conflict_count: usize,
}

/// One painted frame. `golden` omits trailing pad.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffFrame {
    width: u16,
    height: u16,
    lines: Vec<String>,
}

impl DiffSelection {
    pub const fn new(file_index: usize, mode: DiffRenderMode) -> Self {
        Self { file_index, mode }
    }

    pub const fn file_index(self) -> usize {
        self.file_index
    }

    pub const fn mode(self) -> DiffRenderMode {
        self.mode
    }

    pub const fn with_index(self, file_index: usize) -> Self {
        Self { file_index, ..self }
    }

    pub const fn with_mode(self, mode: DiffRenderMode) -> Self {
        Self { mode, ..self }
    }
}

impl DiffRenderMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unified => "unified",
            Self::Semantic => "semantic",
        }
    }
}

impl DegradeReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::TooLarge => "large",
        }
    }
}

impl SemanticOpKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReplaceRange => "replace_range",
            Self::CreateFile => "create_file",
            Self::DeleteFile => "delete_file",
            Self::MoveFile => "move_file",
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Conflict => "conflict",
        }
    }
}

impl DiffFileKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Semantic => "semantic",
            Self::Conflict => "conflict",
            Self::External => "external",
        }
    }
}

impl DiffAttribution {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SemanticPatch => ATTRIBUTED_LABEL,
            Self::UnattributedShell => UNATTRIBUTED_LABEL,
        }
    }

    pub const fn is_unattributed(self) -> bool {
        matches!(self, Self::UnattributedShell)
    }
}

impl DiffLineKind {
    pub const fn prefix(self) -> char {
        match self {
            Self::Meta => '#',
            Self::Add => '+',
            Self::Delete => '-',
            Self::Context => ' ',
        }
    }
}

impl DiffBodyKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unified => "unified",
            Self::Semantic => "semantic",
            Self::Artifact => "metadata",
        }
    }
}

impl DiffWarningKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnattributedShell => "unattributed-shell",
            Self::MergeConflict => "merge-conflict",
        }
    }
}

impl DiffFile {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn dest(&self) -> Option<&str> {
        self.dest.as_deref()
    }

    pub fn kind(&self) -> DiffFileKind {
        self.kind
    }

    pub fn op(&self) -> SemanticOpKind {
        self.op
    }

    pub fn attribution(&self) -> DiffAttribution {
        self.attribution
    }

    pub fn is_unattributed(&self) -> bool {
        self.attribution.is_unattributed()
    }

    pub fn degrade(&self) -> Option<DegradeReason> {
        self.degrade
    }

    pub fn artifact(&self) -> Option<ArtifactId> {
        self.artifact
    }

    pub fn artifact_bytes(&self) -> Option<u64> {
        self.artifact_bytes
    }

    pub fn conflict_kind(&self) -> Option<MergeConflictKind> {
        self.conflict_kind
    }

    pub fn child_op(&self) -> Option<MergeOpKind> {
        self.child_op
    }

    pub fn parent_op(&self) -> Option<MergeOpKind> {
        self.parent_op
    }

    pub fn body_kind(&self, mode: DiffRenderMode) -> DiffBodyKind {
        if self.degrade.is_some() {
            DiffBodyKind::Artifact
        } else {
            match mode {
                DiffRenderMode::Unified => DiffBodyKind::Unified,
                DiffRenderMode::Semantic => DiffBodyKind::Semantic,
            }
        }
    }
}

impl DiffViewModel {
    /// Project `preview` and/or `summary` plus selection. Mutations are
    /// warnings only. Nothing is applied.
    pub fn new(
        preview: Option<&MergePreview>,
        summary: Option<&PatchSummary>,
        mutations: &[ExternalMutation],
        selection: DiffSelection,
        cancel: &CancellationToken,
    ) -> Result<Self, DiffError> {
        check_cancel(cancel)?;
        let mut files = BTreeMap::new();
        if let Some(preview) = preview {
            ingest_preview(&mut files, preview, cancel)?;
        }
        ingest_mutations(&mut files, mutations, cancel)?;
        if files.len() > MAX_DIFF_FILES {
            return Err(DiffError::BoundExceeded);
        }

        let files: Vec<DiffFile> = files.into_values().map(|acc| acc.file).collect();
        let file_index = if files.is_empty() {
            0
        } else {
            selection.file_index.min(files.len() - 1)
        };
        let (derived_files, derived_add, derived_del) = derive_counts(&files);
        let (files_changed, additions, deletions) = match summary {
            Some(summary) => (
                summary.files_changed(),
                summary.additions(),
                summary.deletions(),
            ),
            None => (derived_files, derived_add, derived_del),
        };
        let warnings = collect_warnings(&files);
        Ok(Self {
            files,
            selection: DiffSelection {
                file_index,
                mode: selection.mode,
            },
            files_changed,
            additions,
            deletions,
            warnings,
            preview_hash: preview.map(MergePreview::preview_hash),
            parent_revision: preview.map(|p| p.parent_revision().to_owned()),
            child_view_id: preview.map(MergePreview::child_view_id),
            parent_view_id: preview.map(MergePreview::parent_view_id),
            conflict_count: preview.map(|p| p.conflicts().len()).unwrap_or(0),
        })
    }

    pub fn from_preview(
        preview: &MergePreview,
        summary: Option<&PatchSummary>,
        mutations: &[ExternalMutation],
        selection: DiffSelection,
        cancel: &CancellationToken,
    ) -> Result<Self, DiffError> {
        Self::new(Some(preview), summary, mutations, selection, cancel)
    }

    pub fn from_summary(
        summary: &PatchSummary,
        mutations: &[ExternalMutation],
        selection: DiffSelection,
        cancel: &CancellationToken,
    ) -> Result<Self, DiffError> {
        Self::new(None, Some(summary), mutations, selection, cancel)
    }

    pub fn files(&self) -> &[DiffFile] {
        &self.files
    }

    pub fn selected_file(&self) -> Option<&DiffFile> {
        self.files.get(self.selection.file_index)
    }

    pub fn selection(&self) -> DiffSelection {
        self.selection
    }

    pub fn files_changed(&self) -> u32 {
        self.files_changed
    }

    pub fn additions(&self) -> u32 {
        self.additions
    }

    pub fn deletions(&self) -> u32 {
        self.deletions
    }

    pub fn conflict_count(&self) -> usize {
        self.conflict_count
    }

    pub fn preview_hash(&self) -> Option<ArtifactId> {
        self.preview_hash
    }

    pub fn parent_revision(&self) -> Option<&str> {
        self.parent_revision.as_deref()
    }

    pub fn child_view_id(&self) -> Option<WorkspaceViewId> {
        self.child_view_id
    }

    pub fn parent_view_id(&self) -> Option<WorkspaceViewId> {
        self.parent_view_id
    }

    pub fn unattributed_count(&self) -> usize {
        self.files.iter().filter(|f| f.is_unattributed()).count()
    }

    pub fn warning_kinds(&self) -> impl Iterator<Item = DiffWarningKind> + '_ {
        self.warnings.iter().map(|w| w.kind)
    }

    /// This panel never applies, stages, or rolls back a change set.
    pub const fn applies_changes(&self) -> bool {
        false
    }

    pub fn select_file(&self, index: usize) -> Result<Self, DiffError> {
        if self.files.is_empty() {
            if index == 0 {
                return Ok(self.clone());
            }
            return Err(DiffError::InvalidSelection);
        }
        if index >= self.files.len() {
            return Err(DiffError::InvalidSelection);
        }
        let mut next = self.clone();
        next.selection.file_index = index;
        Ok(next)
    }

    pub fn select_next(&self) -> Self {
        let mut next = self.clone();
        if !self.files.is_empty() && self.selection.file_index + 1 < self.files.len() {
            next.selection.file_index += 1;
        }
        next
    }

    pub fn select_prev(&self) -> Self {
        let mut next = self.clone();
        next.selection.file_index = self.selection.file_index.saturating_sub(1);
        next
    }

    pub fn with_mode(&self, mode: DiffRenderMode) -> Self {
        let mut next = self.clone();
        next.selection.mode = mode;
        next
    }

    pub fn render(&self, width: u16, height: u16) -> DiffFrame {
        let width = width.min(MAX_DIFF_COLS);
        let height = height.min(MAX_DIFF_ROWS);
        if width == 0 || height == 0 {
            return DiffFrame {
                width,
                height,
                lines: Vec::new(),
            };
        }
        let mut lines = Vec::new();
        for warning in &self.warnings {
            lines.push(format!("WARN {} {}", warning.kind.as_str(), warning.path));
        }
        lines.push(format!(
            "summary files:{} +{} -{} conflicts:{}",
            self.files_changed, self.additions, self.deletions, self.conflict_count
        ));
        if self.files.is_empty() {
            lines.push("(empty)".to_owned());
        } else {
            for (index, file) in self.files.iter().enumerate() {
                let marker = if index == self.selection.file_index {
                    '>'
                } else {
                    ' '
                };
                let path = match file.dest.as_deref() {
                    Some(dest) => format!("{} -> {}", file.path, dest),
                    None => file.path.clone(),
                };
                lines.push(format!(
                    "{marker} {path} {} {}",
                    file.op.as_str(),
                    file.attribution.as_str()
                ));
            }
        }
        lines.push("--".to_owned());
        lines.extend(self.selected_body_lines());
        if lines.len() > usize::from(height) {
            lines.truncate(usize::from(height));
        }
        DiffFrame {
            width,
            height,
            lines,
        }
    }

    fn selected_body_lines(&self) -> Vec<String> {
        let Some(file) = self.selected_file() else {
            return Vec::new();
        };
        let mode = self.selection.mode;
        let kind = file.body_kind(mode);
        let mut lines = vec![format!("mode:{}", kind.as_str())];
        if let Some(reason) = file.degrade {
            let artifact = file
                .artifact
                .map(|id| id.to_string())
                .unwrap_or_else(|| "none".to_owned());
            let bytes = file
                .artifact_bytes
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_owned());
            lines.push(format!(
                "reason:{} {ARTIFACT_PREFIX}{artifact} bytes:{bytes}",
                reason.as_str()
            ));
            return lines;
        }
        match mode {
            DiffRenderMode::Unified => {
                for line in &file.unified {
                    lines.push(format!("{}{}", line.kind.prefix(), line.text));
                }
            }
            DiffRenderMode::Semantic => {
                lines.extend(file.semantic.iter().cloned());
            }
        }
        lines
    }
}

impl DiffFrame {
    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Stable dump used by golden tests. Trailing pad is omitted.
    pub fn golden(&self) -> String {
        self.lines.join("\n")
    }

    /// Exact-width rows written into the pane, padded/truncated to `height`.
    pub fn text(&self) -> String {
        let width = usize::from(self.width);
        let height = usize::from(self.height);
        let mut rows = Vec::with_capacity(height);
        for i in 0..height {
            let src = self.lines.get(i).map(String::as_str).unwrap_or("");
            rows.push(fit_width(src, width));
        }
        rows.join("\n")
    }
}

impl Display for DiffError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("diff projection cancelled"),
            Self::BoundExceeded => f.write_str("diff projection resource bound exceeded"),
            Self::InvalidSelection => f.write_str("diff file selection is out of range"),
        }
    }
}

impl Error for DiffError {}

struct FileAcc {
    file: DiffFile,
}

fn ingest_preview(
    files: &mut BTreeMap<String, FileAcc>,
    preview: &MergePreview,
    cancel: &CancellationToken,
) -> Result<(), DiffError> {
    for (index, op) in preview.ops().iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        ingest_op(files, op)?;
    }
    for (index, conflict) in preview.conflicts().iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        ingest_conflict(files, conflict);
    }
    Ok(())
}

fn ingest_op(files: &mut BTreeMap<String, FileAcc>, op: &PatchOp) -> Result<(), DiffError> {
    match op {
        PatchOp::ReplaceRange {
            path,
            preimage,
            start,
            end,
            content,
        } => {
            let display = sanitize_path(path.as_str());
            let mut file = base_file(
                display.clone(),
                None,
                DiffFileKind::Semantic,
                SemanticOpKind::ReplaceRange,
                DiffAttribution::SemanticPatch,
            );
            fill_replace(&mut file, *preimage, start.as_u64(), end.as_u64(), content);
            insert_file(files, display, file);
        }
        PatchOp::CreateFile {
            path,
            content,
            executable,
        } => {
            let display = sanitize_path(path.as_str());
            let mut file = base_file(
                display.clone(),
                None,
                DiffFileKind::Semantic,
                SemanticOpKind::CreateFile,
                DiffAttribution::SemanticPatch,
            );
            fill_create(&mut file, content, *executable);
            insert_file(files, display, file);
        }
        PatchOp::DeleteFile { path, preimage } => {
            let display = sanitize_path(path.as_str());
            let mut file = base_file(
                display.clone(),
                None,
                DiffFileKind::Semantic,
                SemanticOpKind::DeleteFile,
                DiffAttribution::SemanticPatch,
            );
            fill_delete(&mut file, *preimage);
            insert_file(files, display, file);
        }
        PatchOp::MoveFile { from, to, preimage } => {
            let display = sanitize_path(from.as_str());
            let dest = sanitize_path(to.as_str());
            let mut file = base_file(
                display.clone(),
                Some(dest),
                DiffFileKind::Semantic,
                SemanticOpKind::MoveFile,
                DiffAttribution::SemanticPatch,
            );
            fill_move(&mut file, *preimage);
            insert_file(files, display, file);
        }
    }
    Ok(())
}

fn ingest_conflict(files: &mut BTreeMap<String, FileAcc>, conflict: &MergeConflict) {
    let display = sanitize_path(conflict.path().as_str());
    let acc = files.entry(display.clone()).or_insert_with(|| FileAcc {
        file: base_file(
            display,
            None,
            DiffFileKind::Conflict,
            SemanticOpKind::Conflict,
            DiffAttribution::SemanticPatch,
        ),
    });
    acc.file.kind = DiffFileKind::Conflict;
    acc.file.op = SemanticOpKind::Conflict;
    acc.file.conflict_kind = Some(conflict.kind());
    acc.file.child_op = Some(conflict.child_op());
    acc.file.parent_op = Some(conflict.parent_op());
    acc.file.semantic = vec![
        format!("kind:{}", conflict.kind().as_str()),
        format!("child_op:{}", conflict.child_op().as_str()),
        format!("parent_op:{}", conflict.parent_op().as_str()),
    ];
    if acc.file.unified.is_empty() {
        acc.file.unified = vec![
            DiffLine {
                kind: DiffLineKind::Meta,
                text: format!("conflict:{}", conflict.kind().as_str()),
            },
            DiffLine {
                kind: DiffLineKind::Meta,
                text: format!(
                    "child:{} parent:{}",
                    conflict.child_op().as_str(),
                    conflict.parent_op().as_str()
                ),
            },
        ];
    }
}

fn ingest_mutations(
    files: &mut BTreeMap<String, FileAcc>,
    mutations: &[ExternalMutation],
    cancel: &CancellationToken,
) -> Result<(), DiffError> {
    for (index, mutation) in mutations.iter().enumerate() {
        if index.is_multiple_of(CANCEL_STRIDE) {
            check_cancel(cancel)?;
        }
        let display = sanitize_path(mutation.path().as_str());
        let unattributed = mutation.is_unreconciled();
        if let Some(acc) = files.get_mut(&display) {
            if unattributed {
                acc.file.attribution = DiffAttribution::UnattributedShell;
            }
            continue;
        }
        if files.len() >= MAX_DIFF_FILES {
            return Err(DiffError::BoundExceeded);
        }
        let op = match mutation.kind() {
            MutationKind::Added => SemanticOpKind::Added,
            MutationKind::Modified => SemanticOpKind::Modified,
            MutationKind::Deleted => SemanticOpKind::Deleted,
        };
        let attribution = if unattributed {
            DiffAttribution::UnattributedShell
        } else {
            DiffAttribution::SemanticPatch
        };
        let mut file = base_file(
            display.clone(),
            None,
            DiffFileKind::External,
            op,
            attribution,
        );
        fill_external(&mut file, mutation);
        insert_file(files, display, file);
    }
    Ok(())
}

fn fill_replace(file: &mut DiffFile, preimage: ArtifactId, start: u64, end: u64, content: &str) {
    file.semantic = vec![
        format!("kind:{}", file.op.as_str()),
        format!("path:{}", file.path),
        format!("range:[{start},{end})"),
        format!("preimage:{preimage}"),
    ];
    if should_degrade_text(content) {
        degrade(
            file,
            DegradeReason::TooLarge,
            Some(ArtifactId::from_bytes(content.as_bytes())),
            content.len() as u64,
        );
        return;
    }
    let added = split_text_lines(content);
    let deleted = end.saturating_sub(start);
    file.unified.push(DiffLine {
        kind: DiffLineKind::Meta,
        text: format!("@@ -{start},{deleted} +{start},{} @@", added.len()),
    });
    file.unified.push(DiffLine {
        kind: DiffLineKind::Meta,
        text: format!("preimage:{preimage}"),
    });
    for line in added {
        file.unified.push(DiffLine {
            kind: DiffLineKind::Add,
            text: line,
        });
    }
}

fn fill_create(file: &mut DiffFile, content: &[u8], executable: bool) {
    file.semantic = vec![
        format!("kind:{}", file.op.as_str()),
        format!("path:{}", file.path),
        format!("executable:{executable}"),
        format!("bytes:{}", content.len()),
    ];
    if is_binary(content) {
        degrade(
            file,
            DegradeReason::Binary,
            Some(ArtifactId::from_bytes(content)),
            content.len() as u64,
        );
        return;
    }
    let text = match std::str::from_utf8(content) {
        Ok(text) => text,
        Err(_) => {
            degrade(
                file,
                DegradeReason::Binary,
                Some(ArtifactId::from_bytes(content)),
                content.len() as u64,
            );
            return;
        }
    };
    if should_degrade_text(text) {
        degrade(
            file,
            DegradeReason::TooLarge,
            Some(ArtifactId::from_bytes(content)),
            content.len() as u64,
        );
        return;
    }
    file.unified.push(DiffLine {
        kind: DiffLineKind::Meta,
        text: format!("@@ -0,0 +0,{} @@", split_text_lines(text).len()),
    });
    for line in split_text_lines(text) {
        file.unified.push(DiffLine {
            kind: DiffLineKind::Add,
            text: line,
        });
    }
}

fn fill_delete(file: &mut DiffFile, preimage: ArtifactId) {
    file.artifact = Some(preimage);
    file.semantic = vec![
        format!("kind:{}", file.op.as_str()),
        format!("path:{}", file.path),
        format!("preimage:{preimage}"),
    ];
    file.unified = vec![
        DiffLine {
            kind: DiffLineKind::Meta,
            text: "@@ -0,1 +0,0 @@".to_owned(),
        },
        DiffLine {
            kind: DiffLineKind::Delete,
            text: format!("{ARTIFACT_PREFIX}{preimage}"),
        },
    ];
}

fn fill_move(file: &mut DiffFile, preimage: ArtifactId) {
    file.artifact = Some(preimage);
    let dest = file.dest.clone().unwrap_or_default();
    file.semantic = vec![
        format!("kind:{}", file.op.as_str()),
        format!("from:{}", file.path),
        format!("to:{dest}"),
        format!("preimage:{preimage}"),
    ];
    file.unified = vec![DiffLine {
        kind: DiffLineKind::Meta,
        text: format!("rename {} -> {dest}", file.path),
    }];
}

fn fill_external(file: &mut DiffFile, mutation: &ExternalMutation) {
    let before = mutation
        .before()
        .map(|id| id.to_string())
        .unwrap_or_else(|| "-".to_owned());
    let after = mutation
        .after()
        .map(|id| id.to_string())
        .unwrap_or_else(|| "-".to_owned());
    file.artifact = mutation.after().or(mutation.before());
    file.semantic = vec![
        format!("kind:{}", file.op.as_str()),
        format!("path:{}", file.path),
        format!("source:external_mutation"),
        format!("attribution:{}", file.attribution.as_str()),
        format!("before:{before}"),
        format!("after:{after}"),
    ];
    file.unified = vec![
        DiffLine {
            kind: DiffLineKind::Meta,
            text: format!(
                "external {} {}",
                mutation.kind().as_str(),
                file.attribution.as_str()
            ),
        },
        DiffLine {
            kind: DiffLineKind::Meta,
            text: format!("before:{before}"),
        },
        DiffLine {
            kind: DiffLineKind::Meta,
            text: format!("after:{after}"),
        },
    ];
}

fn degrade(file: &mut DiffFile, reason: DegradeReason, artifact: Option<ArtifactId>, bytes: u64) {
    file.degrade = Some(reason);
    file.artifact = artifact;
    file.artifact_bytes = Some(bytes);
    file.unified.clear();
}

fn base_file(
    path: String,
    dest: Option<String>,
    kind: DiffFileKind,
    op: SemanticOpKind,
    attribution: DiffAttribution,
) -> DiffFile {
    DiffFile {
        path,
        dest,
        kind,
        op,
        attribution,
        conflict_kind: None,
        child_op: None,
        parent_op: None,
        unified: Vec::new(),
        semantic: Vec::new(),
        artifact: None,
        artifact_bytes: None,
        degrade: None,
    }
}

fn insert_file(files: &mut BTreeMap<String, FileAcc>, key: String, file: DiffFile) {
    files.insert(key, FileAcc { file });
}

fn collect_warnings(files: &[DiffFile]) -> Vec<DiffWarning> {
    let mut warnings = Vec::new();
    for file in files {
        if file.is_unattributed() {
            warnings.push(DiffWarning {
                kind: DiffWarningKind::UnattributedShell,
                path: file.path.clone(),
            });
        }
        if file.kind == DiffFileKind::Conflict {
            warnings.push(DiffWarning {
                kind: DiffWarningKind::MergeConflict,
                path: file.path.clone(),
            });
        }
    }
    warnings
}

fn derive_counts(files: &[DiffFile]) -> (u32, u32, u32) {
    let files_changed = u32::try_from(files.len()).unwrap_or(u32::MAX);
    let mut additions = 0u32;
    let mut deletions = 0u32;
    for file in files {
        for line in &file.unified {
            match line.kind {
                DiffLineKind::Add => additions = additions.saturating_add(1),
                DiffLineKind::Delete => deletions = deletions.saturating_add(1),
                DiffLineKind::Meta | DiffLineKind::Context => {}
            }
        }
    }
    (files_changed, additions, deletions)
}

fn should_degrade_text(text: &str) -> bool {
    text.len() > MAX_INLINE_DIFF_BYTES
        || text.contains('\0')
        || line_count(text) > MAX_INLINE_DIFF_LINES
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0) || std::str::from_utf8(bytes).is_err()
}

fn split_text_lines(text: &str) -> Vec<String> {
    let sanitized = sanitize_untrusted(text);
    if sanitized.is_empty() {
        return Vec::new();
    }
    sanitized.lines().map(|line| line.to_owned()).collect()
}

fn line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count()
    }
}

fn sanitize_path(path: &str) -> String {
    sanitize_untrusted(path).into_owned()
}

fn fit_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let cols = text.chars().count();
    if cols == width {
        return text.to_owned();
    }
    if cols < width {
        let mut out = text.to_owned();
        out.extend(std::iter::repeat_n(' ', width - cols));
        return out;
    }
    text.chars().take(width).collect()
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), DiffError> {
    if cancel.is_cancelled() {
        Err(DiffError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn check_file_bound_for_test(n: usize) -> Result<(), DiffError> {
    if n > MAX_DIFF_FILES {
        Err(DiffError::BoundExceeded)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{AgentId, ArtifactId, RepoId, RepoPath};
    use std::str::FromStr;
    use workspace::{
        CreateView, ReconcileHint, SemanticPatch, ViewAccess, ViewRegistry, WorkspaceBackend,
        WorkspaceSnapshot, detect, preview_merge, reconcile,
    };

    const AUTHOR: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ad";
    const HELLO_PREIMAGE: &str =
        "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    const GOLDEN_80: &str = "\
WARN unattributed-shell tmp/out.bin
summary files:2 +1 -0 conflicts:0
> src/lib.rs replace_range attributed
  tmp/out.bin modified UNATTRIBUTED
--
mode:unified
#@@ -0,5 +0,1 @@
#preimage:sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
+world";

    const GOLDEN_SEMANTIC: &str = "\
WARN unattributed-shell tmp/out.bin
summary files:2 +1 -0 conflicts:0
> src/lib.rs replace_range attributed
  tmp/out.bin modified UNATTRIBUTED
--
mode:semantic
kind:replace_range
path:src/lib.rs
range:[0,5)
preimage:sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn ui_cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn ws_cancel() -> workspace::CancellationToken {
        workspace::CancellationToken::new()
    }

    fn author() -> AgentId {
        AgentId::from_str(AUTHOR).expect("author")
    }

    fn repo_path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn hello_preimage() -> ArtifactId {
        ArtifactId::from_bytes(b"hello")
    }

    fn replace(path: &str, start: u64, end: u64, content: &str) -> PatchOp {
        PatchOp::replace_range(
            repo_path(path),
            hello_preimage(),
            start,
            end,
            content.to_owned(),
        )
        .expect("replace")
    }

    fn patch(ops: Vec<PatchOp>) -> SemanticPatch {
        SemanticPatch::new(ops, author(), "deadbeef", &ws_cancel()).expect("patch")
    }

    fn empty_patch() -> SemanticPatch {
        patch(Vec::new())
    }

    fn live_pair() -> (
        ViewRegistry,
        workspace::WorkspaceView,
        workspace::WorkspaceView,
    ) {
        let registry = ViewRegistry::new();
        let repo = RepoId::new();
        let child = registry
            .create(
                CreateView::new(
                    repo,
                    WorkspaceBackend::GitWorktree,
                    "deadbeef",
                    ViewAccess::ReadWrite,
                ),
                &ws_cancel(),
            )
            .expect("child");
        let parent = registry
            .create(
                CreateView::new(
                    repo,
                    WorkspaceBackend::Direct,
                    "deadbeef",
                    ViewAccess::ReadWrite,
                ),
                &ws_cancel(),
            )
            .expect("parent");
        let child = registry.quiesce(child.id(), &ws_cancel()).expect("quiesce");
        (registry, child, parent)
    }

    fn fixture_preview() -> MergePreview {
        let (_registry, child, parent) = live_pair();
        preview_merge(
            &child,
            &parent,
            &patch(vec![replace("src/lib.rs", 0, 5, "world")]),
            &empty_patch(),
            &ws_cancel(),
        )
        .expect("preview")
    }

    fn conflict_preview() -> MergePreview {
        let (_registry, child, parent) = live_pair();
        preview_merge(
            &child,
            &parent,
            &patch(vec![replace("src/lib.rs", 0, 5, "child")]),
            &patch(vec![replace("src/lib.rs", 0, 5, "sib")]),
            &ws_cancel(),
        )
        .expect("preview")
    }

    fn unattributed_out_bin() -> Vec<ExternalMutation> {
        let before = WorkspaceSnapshot::from_hashed([(
            repo_path("tmp/out.bin"),
            ArtifactId::from_bytes(b"old"),
        )])
        .expect("before");
        let after = WorkspaceSnapshot::from_hashed([(
            repo_path("tmp/out.bin"),
            ArtifactId::from_bytes(b"new"),
        )])
        .expect("after");
        detect(&before, &after)
    }

    fn fixture_model() -> DiffViewModel {
        DiffViewModel::from_preview(
            &fixture_preview(),
            None,
            &unattributed_out_bin(),
            DiffSelection::default(),
            &ui_cancel(),
        )
        .expect("model")
    }

    #[test]
    fn hello_preimage_matches_workspace_golden() {
        assert_eq!(hello_preimage().to_string(), HELLO_PREIMAGE);
    }

    #[test]
    fn golden_80_120_200() {
        let model = fixture_model();
        assert_eq!(model.render(80, 16).golden(), GOLDEN_80);
        assert_eq!(model.render(120, 16).golden(), GOLDEN_80);
        assert_eq!(model.render(200, 16).golden(), GOLDEN_80);
        assert_eq!(model.render(80, 16).text().lines().count(), 16);
        assert!(
            model
                .render(80, 16)
                .text()
                .lines()
                .all(|line| line.chars().count() == 80)
        );
    }

    #[test]
    fn semantic_mode_shows_op_metadata() {
        let model = fixture_model().with_mode(DiffRenderMode::Semantic);
        assert_eq!(model.render(80, 16).golden(), GOLDEN_SEMANTIC);
    }

    #[test]
    fn file_navigation_moves_selection() {
        let model = fixture_model();
        assert_eq!(
            model.selected_file().map(DiffFile::path),
            Some("src/lib.rs")
        );
        let next = model.select_next();
        assert_eq!(
            next.selected_file().map(DiffFile::path),
            Some("tmp/out.bin")
        );
        assert!(next.render(80, 16).golden().contains("> tmp/out.bin"));
        assert!(!next.applies_changes());
        let prev = next.select_prev();
        assert_eq!(prev.selected_file().map(DiffFile::path), Some("src/lib.rs"));
        assert!(model.select_file(9).is_err());
    }

    #[test]
    fn unattributed_shell_mutation_is_visibly_distinguished() {
        let model = fixture_model();
        assert_eq!(model.unattributed_count(), 1);
        let golden = model.render(80, 16).golden();
        assert!(golden.contains("WARN unattributed-shell tmp/out.bin"));
        assert!(golden.contains("UNATTRIBUTED"));
        let file = model
            .files()
            .iter()
            .find(|f| f.path() == "tmp/out.bin")
            .expect("mutation file");
        assert!(file.is_unattributed());
        assert_eq!(file.kind(), DiffFileKind::External);
        assert_eq!(file.attribution(), DiffAttribution::UnattributedShell);
    }

    #[test]
    fn binary_create_degrades_to_artifact_link() {
        let (_registry, child, parent) = live_pair();
        let preview = preview_merge(
            &child,
            &parent,
            &patch(vec![
                PatchOp::create_file(repo_path("assets/icon.bin"), vec![0xff, 0x00, 0x10], false)
                    .expect("create"),
            ]),
            &empty_patch(),
            &ws_cancel(),
        )
        .expect("preview");
        let model = DiffViewModel::from_preview(
            &preview,
            None,
            &[],
            DiffSelection::default(),
            &ui_cancel(),
        )
        .expect("model");
        let file = model.selected_file().expect("file");
        assert_eq!(file.degrade(), Some(DegradeReason::Binary));
        assert_eq!(
            file.body_kind(DiffRenderMode::Unified),
            DiffBodyKind::Artifact
        );
        let golden = model.render(80, 12).golden();
        assert!(golden.contains("mode:metadata"));
        assert!(golden.contains("reason:binary artifact:"));
        assert!(!golden.contains('\u{0000}'));
    }

    #[test]
    fn large_replace_degrades_to_artifact_link() {
        let (_registry, child, parent) = live_pair();
        let huge = "x".repeat(MAX_INLINE_DIFF_BYTES + 8);
        let preview = preview_merge(
            &child,
            &parent,
            &patch(vec![replace("src/big.rs", 0, 5, &huge)]),
            &empty_patch(),
            &ws_cancel(),
        )
        .expect("preview");
        let model = DiffViewModel::from_preview(
            &preview,
            None,
            &[],
            DiffSelection::default(),
            &ui_cancel(),
        )
        .expect("model");
        let file = model.selected_file().expect("file");
        assert_eq!(file.degrade(), Some(DegradeReason::TooLarge));
        assert!(file.artifact().is_some());
        let golden = model.render(80, 12).golden();
        assert!(golden.contains("reason:large artifact:"));
        assert!(!golden.contains(&huge));
    }

    #[test]
    fn merge_conflict_is_warned_and_listed() {
        let model = DiffViewModel::from_preview(
            &conflict_preview(),
            None,
            &[],
            DiffSelection::default(),
            &ui_cancel(),
        )
        .expect("model");
        assert_eq!(model.conflict_count(), 1);
        let golden = model.render(80, 16).golden();
        assert!(golden.contains("WARN merge-conflict src/lib.rs"));
        assert!(golden.contains("conflict"));
        assert!(!model.applies_changes());
    }

    #[test]
    fn patch_summary_only_is_metadata() {
        let summary = PatchSummary::new(3, 10, 4);
        let model =
            DiffViewModel::from_summary(&summary, &[], DiffSelection::default(), &ui_cancel())
                .expect("model");
        assert_eq!(model.files_changed(), 3);
        assert_eq!(model.additions(), 10);
        assert_eq!(model.deletions(), 4);
        assert!(model.files().is_empty());
        assert!(
            model
                .render(80, 8)
                .golden()
                .contains("summary files:3 +10 -4")
        );
        assert!(!model.applies_changes());
    }

    #[test]
    fn untrusted_content_is_sanitized() {
        let (_registry, child, parent) = live_pair();
        let dirty = "ok\u{1b}]8;;https://evil.test\u{07}link";
        let preview = preview_merge(
            &child,
            &parent,
            &patch(vec![replace("src/lib.rs", 0, 5, dirty)]),
            &empty_patch(),
            &ws_cancel(),
        )
        .expect("preview");
        let model = DiffViewModel::from_preview(
            &preview,
            None,
            &[],
            DiffSelection::default(),
            &ui_cancel(),
        )
        .expect("model");
        let golden = model.render(80, 16).golden();
        assert!(golden.contains("+oklink"));
        assert!(!golden.contains('\u{1b}'));
        assert!(!golden.contains("evil.test"));
    }

    #[test]
    fn cancelled_build_fails_closed() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = DiffViewModel::from_preview(
            &fixture_preview(),
            None,
            &[],
            DiffSelection::default(),
            &cancel,
        )
        .expect_err("cancelled");
        assert_eq!(err, DiffError::Cancelled);
        assert_eq!(err.to_string(), "diff projection cancelled");
    }

    #[test]
    fn viewer_does_not_apply_preview_ops() {
        let preview = fixture_preview();
        assert!(preview.is_conflict_free());
        let model = DiffViewModel::from_preview(
            &preview,
            Some(&PatchSummary::new(1, 1, 0)),
            &[],
            DiffSelection::default(),
            &ui_cancel(),
        )
        .expect("model");
        assert!(!model.applies_changes());
        assert_eq!(model.files_changed(), 1);
        assert_eq!(model.additions(), 1);
    }

    #[test]
    fn reconciled_mutation_is_not_unattributed() {
        let before = WorkspaceSnapshot::from_hashed([(
            repo_path("src/keep.rs"),
            ArtifactId::from_bytes(b"old"),
        )])
        .expect("before");
        let after = WorkspaceSnapshot::from_hashed([(
            repo_path("src/keep.rs"),
            ArtifactId::from_bytes(b"new"),
        )])
        .expect("after");
        let mut mutations = detect(&before, &after);
        let hints = [ReconcileHint::new(
            repo_path("src/keep.rs"),
            Some(ArtifactId::from_bytes(b"old")),
            Some(ArtifactId::from_bytes(b"new")),
        )];
        reconcile(&mut mutations, &hints, &ws_cancel()).expect("reconcile");
        let model = DiffViewModel::from_summary(
            &PatchSummary::new(1, 0, 0),
            &mutations,
            DiffSelection::default(),
            &ui_cancel(),
        )
        .expect("model");
        assert_eq!(model.unattributed_count(), 0);
        let golden = model.render(80, 12).golden();
        assert!(!golden.contains("UNATTRIBUTED"));
        assert!(golden.contains("attributed"));
    }

    #[test]
    fn bound_exceeded_is_typed() {
        assert_eq!(
            super::check_file_bound_for_test(MAX_DIFF_FILES + 1),
            Err(DiffError::BoundExceeded)
        );
        assert_eq!(super::check_file_bound_for_test(0), Ok(()));
    }
}
