//! Semantic patch data model: create/delete/move/replace with preimages.
//!
//! Construction and decode validate ranges, size bounds, and overlapping
//! replace ops. Patch identity is a SHA-256 of an explicit canonical
//! encoding so JSON key order and host path separators cannot change the
//! digest. A patch also carries bounded intent metadata (the intent/goal/
//! evidence/message block that FR-WS-005 links to the textual diff).

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use protocol::{AgentId, ArtifactId, EvidenceId, GoalId, RepoPath};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};

use crate::view::{CancellationToken, MAX_BASE_REVISION_BYTES};

/// Wire schema name for [`SemanticPatch`].
pub const PATCH_SCHEMA: &str = "rapidlm.semantic_patch";

/// v2 schema version for semantic-patch objects. v2 adds the `metadata`
/// intent block; v1 documents are rejected as unsupported.
pub const PATCH_SCHEMA_VERSION: u16 = 2;

/// Maximum operations accepted in one patch.
pub const MAX_PATCH_OPS: usize = 4096;

/// Maximum payload bytes accepted on one create or replace op.
pub const MAX_OP_CONTENT_BYTES: usize = 8 * 1024 * 1024;

/// Maximum combined payload bytes accepted across one patch.
pub const MAX_PATCH_CONTENT_BYTES: usize = 32 * 1024 * 1024;

/// Maximum intent bytes accepted on the patch metadata block.
pub const MAX_PATCH_INTENT_BYTES: usize = 1024;

/// Maximum message bytes accepted on the patch metadata block.
pub const MAX_PATCH_MESSAGE_BYTES: usize = 4096;

/// Maximum evidence references accepted on the patch metadata block.
pub const MAX_PATCH_EVIDENCE_REFS: usize = 32;

const CANCEL_STRIDE: usize = 8;
const PATCH_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "author",
    "base_revision",
    "metadata",
    "ops",
];

/// Byte offset within a file. Replace ranges are `[start, end)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct BytePos(u64);

/// Create, delete, move, or replace one repository path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatchOp {
    ReplaceRange {
        path: RepoPath,
        preimage: ArtifactId,
        start: BytePos,
        end: BytePos,
        content: String,
    },
    CreateFile {
        path: RepoPath,
        content: Vec<u8>,
        executable: bool,
    },
    DeleteFile {
        path: RepoPath,
        preimage: ArtifactId,
    },
    MoveFile {
        from: RepoPath,
        to: RepoPath,
        preimage: ArtifactId,
    },
}

/// Bounded intent described by a patch (FR-WS-005 intent/evidence linkage).
///
/// Intents are free-form but bounded and control-character-free; goal and
/// evidence references are typed IDs and never display text. Duplicate
/// evidence refs and control characters are rejected fail-closed when the
/// owning patch is validated.
#[derive(Clone, Debug, Eq, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchMetadata {
    intent: String,
    goal: Option<GoalId>,
    evidence: Vec<EvidenceId>,
    message: Option<String>,
}

impl PatchMetadata {
    /// Build metadata with a non-blank intent.
    pub fn new(intent: impl Into<String>) -> Result<Self, PatchError> {
        let meta = Self {
            intent: intent.into(),
            ..Self::default()
        };
        meta.validate()?;
        if meta.intent.trim().is_empty() {
            return Err(PatchError::InvalidIntent);
        }
        Ok(meta)
    }

    /// Attach a goal reference.
    pub fn with_goal(mut self, goal: GoalId) -> Self {
        self.goal = Some(goal);
        self
    }

    /// Append evidence references. Duplicates and overflow are checked on
    /// patch validation.
    pub fn with_evidence(mut self, evidence: impl IntoIterator<Item = EvidenceId>) -> Self {
        self.evidence.extend(evidence);
        self
    }

    /// Attach an optional message.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn intent(&self) -> &str {
        &self.intent
    }

    pub fn goal(&self) -> Option<GoalId> {
        self.goal
    }

    pub fn evidence(&self) -> &[EvidenceId] {
        &self.evidence
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    fn validate(&self) -> Result<(), PatchError> {
        if self.intent.len() > MAX_PATCH_INTENT_BYTES {
            return Err(PatchError::IntentTooLong {
                limit: MAX_PATCH_INTENT_BYTES,
            });
        }
        if self.intent.chars().any(char::is_control) {
            return Err(PatchError::InvalidIntent);
        }
        if let Some(message) = &self.message {
            if message.len() > MAX_PATCH_MESSAGE_BYTES {
                return Err(PatchError::MessageTooLong {
                    limit: MAX_PATCH_MESSAGE_BYTES,
                });
            }
            if message.chars().any(char::is_control) {
                return Err(PatchError::InvalidMessage);
            }
        }
        if self.evidence.len() > MAX_PATCH_EVIDENCE_REFS {
            return Err(PatchError::TooManyEvidenceRefs {
                limit: MAX_PATCH_EVIDENCE_REFS,
            });
        }
        let mut seen = BTreeSet::new();
        for evidence in &self.evidence {
            if !seen.insert(*evidence) {
                return Err(PatchError::DuplicateEvidenceRef);
            }
        }
        Ok(())
    }
}

/// Versioned set of first-party file operations against one revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticPatch {
    ops: Vec<PatchOp>,
    author: AgentId,
    base_revision: String,
    metadata: PatchMetadata,
}

/// Typed patch-model failure. Display never echoes paths or payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatchError {
    Cancelled,
    InvalidBaseRevision,
    TooManyOps { limit: usize },
    ContentTooLarge { limit: usize },
    InvalidRange { path: RepoPath },
    OverlappingReplace { path: RepoPath },
    InvalidMove,
    InvalidContentEncoding,
    UnknownVariant,
    UnsupportedSchema,
    UnsupportedSchemaVersion,
    InvalidIntent,
    IntentTooLong { limit: usize },
    InvalidMessage,
    MessageTooLong { limit: usize },
    TooManyEvidenceRefs { limit: usize },
    DuplicateEvidenceRef,
}

impl BytePos {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for BytePos {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl PatchOp {
    pub fn replace_range(
        path: RepoPath,
        preimage: ArtifactId,
        start: impl Into<BytePos>,
        end: impl Into<BytePos>,
        content: impl Into<String>,
    ) -> Result<Self, PatchError> {
        let start = start.into();
        let end = end.into();
        let content = content.into();
        let op = Self::ReplaceRange {
            path,
            preimage,
            start,
            end,
            content,
        };
        op.check_bounds()?;
        Ok(op)
    }

    pub fn create_file(
        path: RepoPath,
        content: impl Into<Vec<u8>>,
        executable: bool,
    ) -> Result<Self, PatchError> {
        let op = Self::CreateFile {
            path,
            content: content.into(),
            executable,
        };
        op.check_bounds()?;
        Ok(op)
    }

    pub fn delete_file(path: RepoPath, preimage: ArtifactId) -> Self {
        Self::DeleteFile { path, preimage }
    }

    pub fn move_file(
        from: RepoPath,
        to: RepoPath,
        preimage: ArtifactId,
    ) -> Result<Self, PatchError> {
        if from == to {
            return Err(PatchError::InvalidMove);
        }
        Ok(Self::MoveFile { from, to, preimage })
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::ReplaceRange { .. } => "replace_range",
            Self::CreateFile { .. } => "create_file",
            Self::DeleteFile { .. } => "delete_file",
            Self::MoveFile { .. } => "move_file",
        }
    }

    fn content_len(&self) -> usize {
        match self {
            Self::ReplaceRange { content, .. } => content.len(),
            Self::CreateFile { content, .. } => content.len(),
            Self::DeleteFile { .. } | Self::MoveFile { .. } => 0,
        }
    }

    fn check_bounds(&self) -> Result<(), PatchError> {
        match self {
            Self::ReplaceRange {
                path, start, end, ..
            } if start.as_u64() > end.as_u64() => {
                Err(PatchError::InvalidRange { path: path.clone() })
            }
            Self::MoveFile { from, to, .. } if from == to => Err(PatchError::InvalidMove),
            _ => {
                if self.content_len() > MAX_OP_CONTENT_BYTES {
                    return Err(PatchError::ContentTooLarge {
                        limit: MAX_OP_CONTENT_BYTES,
                    });
                }
                Ok(())
            }
        }
    }
}

impl SemanticPatch {
    /// Build a patch after validating bounds, revision, and replace overlaps.
    /// The metadata intent block defaults to empty; use [`Self::with_metadata`]
    /// to attach intent, goal, evidence, or a message.
    pub fn new(
        ops: Vec<PatchOp>,
        author: AgentId,
        base_revision: impl Into<String>,
        cancel: &CancellationToken,
    ) -> Result<Self, PatchError> {
        Self::build(
            ops,
            author,
            base_revision.into(),
            PatchMetadata::default(),
            cancel,
        )
    }

    /// Build a patch with an attached metadata intent block.
    pub fn with_metadata(
        ops: Vec<PatchOp>,
        author: AgentId,
        base_revision: impl Into<String>,
        metadata: PatchMetadata,
        cancel: &CancellationToken,
    ) -> Result<Self, PatchError> {
        Self::build(ops, author, base_revision.into(), metadata, cancel)
    }

    fn build(
        ops: Vec<PatchOp>,
        author: AgentId,
        base_revision: String,
        metadata: PatchMetadata,
        cancel: &CancellationToken,
    ) -> Result<Self, PatchError> {
        let patch = Self {
            ops,
            author,
            base_revision: parse_base_revision(&base_revision)?,
            metadata,
        };
        patch.validate(cancel)?;
        Ok(patch)
    }

    pub fn ops(&self) -> &[PatchOp] {
        &self.ops
    }

    pub fn author(&self) -> AgentId {
        self.author
    }

    pub fn base_revision(&self) -> &str {
        &self.base_revision
    }

    pub fn metadata(&self) -> &PatchMetadata {
        &self.metadata
    }

    /// Replace the metadata block and re-validate. On failure the previous
    /// metadata is restored, so the patch stays reviewable.
    pub fn set_metadata(&mut self, metadata: PatchMetadata) -> Result<(), PatchError> {
        let original = std::mem::replace(&mut self.metadata, metadata);
        if let Err(err) = self.validate(&CancellationToken::new()) {
            self.metadata = original;
            return Err(err);
        }
        Ok(())
    }

    /// Re-check invariants. Overlapping replace ops and invalid metadata fail
    /// closed.
    pub fn validate(&self, cancel: &CancellationToken) -> Result<(), PatchError> {
        check_cancel(cancel)?;
        self.metadata.validate()?;
        if self.ops.len() > MAX_PATCH_OPS {
            return Err(PatchError::TooManyOps {
                limit: MAX_PATCH_OPS,
            });
        }

        let mut total = 0usize;
        let mut ranges: BTreeMap<&RepoPath, Vec<(u64, u64)>> = BTreeMap::new();
        for (index, op) in self.ops.iter().enumerate() {
            if index % CANCEL_STRIDE == 0 {
                check_cancel(cancel)?;
            }
            op.check_bounds()?;
            total = total
                .checked_add(op.content_len())
                .ok_or(PatchError::ContentTooLarge {
                    limit: MAX_PATCH_CONTENT_BYTES,
                })?;
            if total > MAX_PATCH_CONTENT_BYTES {
                return Err(PatchError::ContentTooLarge {
                    limit: MAX_PATCH_CONTENT_BYTES,
                });
            }
            if let PatchOp::ReplaceRange {
                path, start, end, ..
            } = op
            {
                ranges
                    .entry(path)
                    .or_default()
                    .push((start.as_u64(), end.as_u64()));
            }
        }

        for (path, mut list) in ranges {
            list.sort_unstable();
            for pair in list.windows(2) {
                if ranges_overlap(pair[0], pair[1]) {
                    return Err(PatchError::OverlappingReplace { path: path.clone() });
                }
            }
        }
        Ok(())
    }

    /// SHA-256 of the canonical encoding. Independent of JSON key order.
    pub fn hash(&self, cancel: &CancellationToken) -> Result<ArtifactId, PatchError> {
        check_cancel(cancel)?;
        let bytes = self.canonical_bytes(cancel)?;
        Ok(ArtifactId::from_bytes(&bytes))
    }

    fn canonical_bytes(&self, cancel: &CancellationToken) -> Result<Vec<u8>, PatchError> {
        let mut buf = Vec::new();
        append_bytes(&mut buf, PATCH_SCHEMA.as_bytes());
        buf.extend_from_slice(&PATCH_SCHEMA_VERSION.to_be_bytes());
        append_bytes(&mut buf, self.author.as_uuid().as_bytes());
        append_bytes(&mut buf, self.base_revision.as_bytes());
        encode_metadata(&mut buf, &self.metadata);
        buf.extend_from_slice(&(self.ops.len() as u64).to_be_bytes());
        for (index, op) in self.ops.iter().enumerate() {
            if index % CANCEL_STRIDE == 0 {
                check_cancel(cancel)?;
            }
            encode_op(&mut buf, op);
        }
        Ok(buf)
    }
}

impl Serialize for SemanticPatch {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("SemanticPatch", PATCH_FIELDS.len())?;
        state.serialize_field("schema", PATCH_SCHEMA)?;
        state.serialize_field("schema_version", &PATCH_SCHEMA_VERSION)?;
        state.serialize_field("author", &self.author)?;
        state.serialize_field("base_revision", &self.base_revision)?;
        state.serialize_field("metadata", &self.metadata)?;
        state.serialize_field("ops", &self.ops)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for SemanticPatch {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawSemanticPatch::deserialize(deserializer)?;
        if raw.schema != PATCH_SCHEMA {
            return Err(de::Error::custom(PatchError::UnsupportedSchema));
        }
        if raw.schema_version != PATCH_SCHEMA_VERSION {
            return Err(de::Error::custom(PatchError::UnsupportedSchemaVersion));
        }
        let patch = SemanticPatch {
            ops: raw.ops,
            author: raw.author,
            base_revision: parse_base_revision(&raw.base_revision).map_err(de::Error::custom)?,
            metadata: raw.metadata,
        };
        patch
            .validate(&CancellationToken::new())
            .map_err(de::Error::custom)?;
        Ok(patch)
    }
}

impl Serialize for PatchOp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::ReplaceRange {
                path,
                preimage,
                start,
                end,
                content,
            } => {
                let mut state = serializer.serialize_struct("PatchOp", 6)?;
                state.serialize_field("op", "replace_range")?;
                state.serialize_field("path", path)?;
                state.serialize_field("preimage", preimage)?;
                state.serialize_field("start", &start.as_u64())?;
                state.serialize_field("end", &end.as_u64())?;
                state.serialize_field("content", content)?;
                state.end()
            }
            Self::CreateFile {
                path,
                content,
                executable,
            } => {
                let mut state = serializer.serialize_struct("PatchOp", 4)?;
                state.serialize_field("op", "create_file")?;
                state.serialize_field("path", path)?;
                state.serialize_field("content", &encode_hex(content))?;
                state.serialize_field("executable", executable)?;
                state.end()
            }
            Self::DeleteFile { path, preimage } => {
                let mut state = serializer.serialize_struct("PatchOp", 3)?;
                state.serialize_field("op", "delete_file")?;
                state.serialize_field("path", path)?;
                state.serialize_field("preimage", preimage)?;
                state.end()
            }
            Self::MoveFile { from, to, preimage } => {
                let mut state = serializer.serialize_struct("PatchOp", 4)?;
                state.serialize_field("op", "move_file")?;
                state.serialize_field("from", from)?;
                state.serialize_field("to", to)?;
                state.serialize_field("preimage", preimage)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for PatchOp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawPatchOp::deserialize(deserializer)?;
        let op = match raw {
            RawPatchOp::ReplaceRange {
                path,
                preimage,
                start,
                end,
                content,
            } => PatchOp::ReplaceRange {
                path,
                preimage,
                start: BytePos::new(start),
                end: BytePos::new(end),
                content,
            },
            RawPatchOp::CreateFile {
                path,
                content,
                executable,
            } => PatchOp::CreateFile {
                path,
                content: content.0,
                executable,
            },
            RawPatchOp::DeleteFile { path, preimage } => PatchOp::DeleteFile { path, preimage },
            RawPatchOp::MoveFile { from, to, preimage } => PatchOp::MoveFile { from, to, preimage },
        };
        op.check_bounds().map_err(de::Error::custom)?;
        Ok(op)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSemanticPatch {
    schema: String,
    schema_version: u16,
    author: AgentId,
    base_revision: String,
    metadata: PatchMetadata,
    ops: Vec<PatchOp>,
}

#[derive(Deserialize)]
#[serde(tag = "op", deny_unknown_fields)]
enum RawPatchOp {
    #[serde(rename = "replace_range")]
    ReplaceRange {
        path: RepoPath,
        preimage: ArtifactId,
        start: u64,
        end: u64,
        content: String,
    },
    #[serde(rename = "create_file")]
    CreateFile {
        path: RepoPath,
        content: HexBytes,
        executable: bool,
    },
    #[serde(rename = "delete_file")]
    DeleteFile {
        path: RepoPath,
        preimage: ArtifactId,
    },
    #[serde(rename = "move_file")]
    MoveFile {
        from: RepoPath,
        to: RepoPath,
        preimage: ArtifactId,
    },
}

struct HexBytes(Vec<u8>);

impl<'de> Deserialize<'de> for HexBytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        decode_hex(&raw).map(Self).map_err(de::Error::custom)
    }
}

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("semantic patch operation cancelled"),
            Self::InvalidBaseRevision => f.write_str("semantic patch base revision is invalid"),
            Self::TooManyOps { .. } => f.write_str("semantic patch operation limit reached"),
            Self::ContentTooLarge { .. } => f.write_str("semantic patch content exceeds the limit"),
            Self::InvalidRange { .. } => f.write_str("semantic patch replace range is invalid"),
            Self::OverlappingReplace { .. } => f.write_str("overlapping replace operations"),
            Self::InvalidMove => f.write_str("semantic patch move source and destination match"),
            Self::InvalidContentEncoding => {
                f.write_str("semantic patch content encoding is invalid")
            }
            Self::UnknownVariant => f.write_str("unknown semantic patch operation"),
            Self::UnsupportedSchema => f.write_str("unsupported semantic patch schema"),
            Self::UnsupportedSchemaVersion => {
                f.write_str("unsupported semantic patch schema version")
            }
            Self::InvalidIntent => f.write_str("semantic patch intent is invalid"),
            Self::IntentTooLong { .. } => f.write_str("semantic patch intent exceeds the limit"),
            Self::InvalidMessage => f.write_str("semantic patch message is invalid"),
            Self::MessageTooLong { .. } => f.write_str("semantic patch message exceeds the limit"),
            Self::TooManyEvidenceRefs { .. } => {
                f.write_str("semantic patch evidence limit reached")
            }
            Self::DuplicateEvidenceRef => f.write_str("duplicate semantic patch evidence"),
        }
    }
}

impl Error for PatchError {}

fn check_cancel(cancel: &CancellationToken) -> Result<(), PatchError> {
    if cancel.is_cancelled() {
        Err(PatchError::Cancelled)
    } else {
        Ok(())
    }
}

fn parse_base_revision(raw: &str) -> Result<String, PatchError> {
    if raw.is_empty() || raw.len() > MAX_BASE_REVISION_BYTES {
        return Err(PatchError::InvalidBaseRevision);
    }
    if raw.contains('\0')
        || raw.chars().any(char::is_control)
        || raw.chars().any(char::is_whitespace)
    {
        return Err(PatchError::InvalidBaseRevision);
    }
    Ok(raw.to_owned())
}

fn ranges_overlap(a: (u64, u64), b: (u64, u64)) -> bool {
    if a.0 == a.1 && b.0 == b.1 {
        return a.0 == b.0;
    }
    if a.0 == a.1 {
        return a.0 >= b.0 && a.0 < b.1;
    }
    if b.0 == b.1 {
        return b.0 >= a.0 && b.0 < a.1;
    }
    a.0 < b.1 && b.0 < a.1
}

fn encode_metadata(buf: &mut Vec<u8>, metadata: &PatchMetadata) {
    append_bytes(buf, metadata.intent().as_bytes());
    match metadata.goal() {
        Some(goal) => {
            buf.push(1);
            buf.extend_from_slice(goal.as_uuid().as_bytes());
        }
        None => buf.push(0),
    }
    buf.extend_from_slice(&(metadata.evidence().len() as u64).to_be_bytes());
    for evidence in metadata.evidence() {
        buf.extend_from_slice(evidence.as_uuid().as_bytes());
    }
    match metadata.message() {
        Some(message) => {
            buf.push(1);
            append_bytes(buf, message.as_bytes());
        }
        None => buf.push(0),
    }
}

fn encode_op(buf: &mut Vec<u8>, op: &PatchOp) {
    match op {
        PatchOp::ReplaceRange {
            path,
            preimage,
            start,
            end,
            content,
        } => {
            buf.push(1);
            append_bytes(buf, path.as_str().as_bytes());
            buf.extend_from_slice(preimage.as_digest());
            buf.extend_from_slice(&start.as_u64().to_be_bytes());
            buf.extend_from_slice(&end.as_u64().to_be_bytes());
            append_bytes(buf, content.as_bytes());
        }
        PatchOp::CreateFile {
            path,
            content,
            executable,
        } => {
            buf.push(2);
            append_bytes(buf, path.as_str().as_bytes());
            append_bytes(buf, content);
            buf.push(u8::from(*executable));
        }
        PatchOp::DeleteFile { path, preimage } => {
            buf.push(3);
            append_bytes(buf, path.as_str().as_bytes());
            buf.extend_from_slice(preimage.as_digest());
        }
        PatchOp::MoveFile { from, to, preimage } => {
            buf.push(4);
            append_bytes(buf, from.as_str().as_bytes());
            append_bytes(buf, to.as_str().as_bytes());
            buf.extend_from_slice(preimage.as_digest());
        }
    }
}

fn append_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(bytes);
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex(raw: &str) -> Result<Vec<u8>, PatchError> {
    if !raw.len().is_multiple_of(2) {
        return Err(PatchError::InvalidContentEncoding);
    }
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut index = 0;
    while index < bytes.len() {
        let hi = hex_nibble(bytes[index])?;
        let lo = hex_nibble(bytes[index + 1])?;
        out.push((hi << 4) | lo);
        index += 2;
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Result<u8, PatchError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(PatchError::InvalidContentEncoding),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    const AUTHOR: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ad";
    const EVIDENCE: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789ae";
    const GOAL: &str = "018f3c8a-7e2b-7a10-8c4d-0123456789af";
    const HELLO_PREIMAGE: &str =
        "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    const GOLDEN_JSON: &str = r#"{"schema":"rapidlm.semantic_patch","schema_version":2,"author":"018f3c8a-7e2b-7a10-8c4d-0123456789ad","base_revision":"deadbeef","metadata":{"intent":"","goal":null,"evidence":[],"message":null},"ops":[{"op":"replace_range","path":"src/lib.rs","preimage":"sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","start":0,"end":5,"content":"world"}]}"#;
    const GOLDEN_HASH: &str =
        "sha256:0b17cab2b7e66f0f734d83723990ddd96f393dc2dbfeccbc58345267c643e6b8";

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    fn author() -> AgentId {
        AgentId::from_str(AUTHOR).expect("author")
    }

    fn evidence() -> EvidenceId {
        EvidenceId::from_str(EVIDENCE).expect("evidence")
    }

    fn goal() -> GoalId {
        GoalId::from_str(GOAL).expect("goal")
    }

    fn hello_preimage() -> ArtifactId {
        ArtifactId::from_bytes(b"hello")
    }

    fn path(raw: &str) -> RepoPath {
        RepoPath::parse(raw).expect("repo path")
    }

    fn replace(raw_path: &str, start: u64, end: u64, content: &str) -> PatchOp {
        PatchOp::replace_range(
            path(raw_path),
            hello_preimage(),
            start,
            end,
            content.to_owned(),
        )
        .expect("replace")
    }

    fn patch(ops: Vec<PatchOp>) -> SemanticPatch {
        SemanticPatch::new(ops, author(), "deadbeef", &cancel()).expect("patch")
    }

    fn metadata(intent: &str) -> PatchMetadata {
        PatchMetadata::new(intent).expect("metadata")
    }

    #[test]
    fn golden_json_round_trips() {
        let built = patch(vec![replace("src/lib.rs", 0, 5, "world")]);
        let json = serde_json::to_string(&built).expect("serialize");
        assert_eq!(json, GOLDEN_JSON);
        let decoded = serde_json::from_str::<SemanticPatch>(GOLDEN_JSON).expect("deserialize");
        assert_eq!(decoded, built);
        assert_eq!(decoded.author(), author());
        assert_eq!(decoded.base_revision(), "deadbeef");
        assert_eq!(decoded.ops().len(), 1);
        assert_eq!(decoded.metadata().intent(), "");
        assert_eq!(decoded.metadata().goal(), None);
        assert!(decoded.metadata().evidence().is_empty());
        assert_eq!(decoded.metadata().message(), None);
    }

    #[test]
    fn hash_is_stable_across_json_key_order_and_separators() {
        let slash = patch(vec![replace("src/lib.rs", 0, 5, "world")]);
        let backslash = patch(vec![replace("src\\lib.rs", 0, 5, "world")]);
        let shuffled = serde_json::from_str::<SemanticPatch>(
            r#"{"ops":[{"end":5,"content":"world","preimage":"sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","path":"src\\lib.rs","start":0,"op":"replace_range"}],"metadata":{"message":null,"evidence":[],"goal":null,"intent":""},"base_revision":"deadbeef","author":"018f3c8a-7e2b-7a10-8c4d-0123456789ad","schema_version":2,"schema":"rapidlm.semantic_patch"}"#,
        )
        .expect("shuffled keys");
        let slash_hash = slash.hash(&cancel()).expect("slash hash");
        let backslash_hash = backslash.hash(&cancel()).expect("backslash hash");
        let shuffled_hash = shuffled.hash(&cancel()).expect("shuffled hash");
        assert_eq!(slash_hash, backslash_hash);
        assert_eq!(slash_hash, shuffled_hash);
        assert_eq!(slash_hash.to_string(), shuffled_hash.to_string());
        assert_eq!(slash_hash.to_string(), GOLDEN_HASH);
        match &backslash.ops()[0] {
            PatchOp::ReplaceRange { path, .. } => assert_eq!(path.as_str(), "src/lib.rs"),
            other => panic!("expected replace, got {other:?}"),
        }
    }

    #[test]
    fn overlapping_replace_ops_are_rejected() {
        let err = SemanticPatch::new(
            vec![
                replace("src/lib.rs", 0, 10, "aaaa"),
                replace("src/lib.rs", 5, 15, "bbbb"),
            ],
            author(),
            "deadbeef",
            &cancel(),
        )
        .expect_err("overlap");
        assert!(matches!(err, PatchError::OverlappingReplace { .. }));
        assert_eq!(err.to_string(), "overlapping replace operations");
        assert!(!err.to_string().contains("aaaa"));
        assert!(!err.to_string().contains("bbbb"));
    }

    #[test]
    fn overlapping_replace_via_platform_separators_is_rejected() {
        let err = SemanticPatch::new(
            vec![
                replace("crates/workspace/src/lib.rs", 0, 8, "SECRET"),
                replace("crates\\workspace\\src\\lib.rs", 7, 12, "LEAK"),
            ],
            author(),
            "deadbeef",
            &cancel(),
        )
        .expect_err("normalized overlap");
        assert!(matches!(
            err,
            PatchError::OverlappingReplace { ref path } if path.as_str() == "crates/workspace/src/lib.rs"
        ));
        let shown = err.to_string();
        assert!(!shown.contains("SECRET"));
        assert!(!shown.contains("LEAK"));
        assert!(!shown.contains("crates"));
    }

    #[test]
    fn adjacent_replaces_on_same_path_are_allowed() {
        let built = SemanticPatch::new(
            vec![
                replace("src/lib.rs", 0, 5, "hello"),
                replace("src/lib.rs", 5, 10, "world"),
            ],
            author(),
            "deadbeef",
            &cancel(),
        )
        .expect("adjacent");
        assert_eq!(built.ops().len(), 2);
    }

    #[test]
    fn create_delete_move_round_trip() {
        let preimage = hello_preimage();
        let built = patch(vec![
            PatchOp::create_file(path("bin/tool"), b"\x7fELF".to_vec(), true).expect("create"),
            PatchOp::delete_file(path("obsolete.rs"), preimage),
            PatchOp::move_file(path("src/a.rs"), path("src/b.rs"), preimage).expect("move"),
        ]);
        let json = serde_json::to_string(&built).expect("serialize");
        let decoded = serde_json::from_str::<SemanticPatch>(&json).expect("deserialize");
        assert_eq!(decoded, built);
        match &decoded.ops()[0] {
            PatchOp::CreateFile {
                content,
                executable,
                ..
            } => {
                assert_eq!(content, b"\x7fELF");
                assert!(*executable);
            }
            other => panic!("expected create, got {other:?}"),
        }
        assert!(json.contains("\"content\":\"7f454c46\""));
        assert_eq!(
            built.hash(&cancel()).expect("hash"),
            decoded.hash(&cancel()).expect("decoded hash")
        );
    }

    #[test]
    fn path_traversal_cannot_enter_a_patch() {
        assert!(RepoPath::parse("../secret").is_err());
        let json = r#"{"schema":"rapidlm.semantic_patch","schema_version":2,"author":"018f3c8a-7e2b-7a10-8c4d-0123456789ad","base_revision":"deadbeef","metadata":{"intent":"","goal":null,"evidence":[],"message":null},"ops":[{"op":"delete_file","path":"../secret","preimage":"sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"}]}"#;
        assert!(serde_json::from_str::<SemanticPatch>(json).is_err());
        let abs = r#"{"schema":"rapidlm.semantic_patch","schema_version":2,"author":"018f3c8a-7e2b-7a10-8c4d-0123456789ad","base_revision":"deadbeef","metadata":{"intent":"","goal":null,"evidence":[],"message":null},"ops":[{"op":"create_file","path":"/etc/passwd","content":"","executable":false}]}"#;
        assert!(serde_json::from_str::<SemanticPatch>(abs).is_err());
    }

    #[test]
    fn invalid_range_and_same_move_are_rejected() {
        let range_err = PatchOp::replace_range(path("src/lib.rs"), hello_preimage(), 9, 3, "x")
            .expect_err("range");
        assert!(matches!(range_err, PatchError::InvalidRange { .. }));
        assert!(!range_err.to_string().contains("x"));
        let move_err = PatchOp::move_file(path("src/a.rs"), path("src\\a.rs"), hello_preimage())
            .expect_err("same path");
        assert_eq!(move_err, PatchError::InvalidMove);
    }

    #[test]
    fn unknown_op_and_unknown_fields_are_rejected() {
        let unknown_op = format!(
            r#"{{"schema":"rapidlm.semantic_patch","schema_version":2,"author":"{AUTHOR}","base_revision":"deadbeef",{METADATA},"ops":[{{"op":"chmod","path":"src/lib.rs","preimage":"sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"}}]}}"#,
            AUTHOR = AUTHOR,
            METADATA = r#""metadata":{"intent":"","goal":null,"evidence":[],"message":null}"#
        );
        assert!(serde_json::from_str::<SemanticPatch>(&unknown_op).is_err());
        let extra = format!(
            r#"{{"schema":"rapidlm.semantic_patch","schema_version":2,"author":"{AUTHOR}","base_revision":"deadbeef",{METADATA},"ops":[{{"op":"delete_file","path":"src/lib.rs","preimage":"sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","extra":true}}]}}"#,
            AUTHOR = AUTHOR,
            METADATA = r#""metadata":{"intent":"","goal":null,"evidence":[],"message":null}"#
        );
        assert!(serde_json::from_str::<SemanticPatch>(&extra).is_err());
        let uppercase_hex = format!(
            r#"{{"schema":"rapidlm.semantic_patch","schema_version":2,"author":"{AUTHOR}","base_revision":"deadbeef",{METADATA},"ops":[{{"op":"create_file","path":"a.bin","content":"7F454C46","executable":false}}]}}"#,
            AUTHOR = AUTHOR,
            METADATA = r#""metadata":{"intent":"","goal":null,"evidence":[],"message":null}"#
        );
        assert!(serde_json::from_str::<SemanticPatch>(&uppercase_hex).is_err());
    }

    #[test]
    fn cancelled_validate_and_hash_fail_closed() {
        let built = patch(vec![replace("src/lib.rs", 0, 5, "world")]);
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            SemanticPatch::new(
                vec![replace("src/lib.rs", 0, 5, "world")],
                author(),
                "deadbeef",
                &token
            ),
            Err(PatchError::Cancelled)
        );
        assert_eq!(built.validate(&token), Err(PatchError::Cancelled));
        assert_eq!(built.hash(&token), Err(PatchError::Cancelled));
    }

    #[test]
    fn oversized_content_and_op_count_are_rejected() {
        let too_big = "a".repeat(MAX_OP_CONTENT_BYTES + 1);
        let err = PatchOp::replace_range(path("src/lib.rs"), hello_preimage(), 0, 1, too_big)
            .expect_err("op cap");
        assert!(
            matches!(err, PatchError::ContentTooLarge { limit } if limit == MAX_OP_CONTENT_BYTES)
        );
        let ops = (0..=MAX_PATCH_OPS)
            .map(|index| replace(&format!("src/f{index}.rs"), 0, 1, "x"))
            .collect();
        let err = SemanticPatch::new(ops, author(), "deadbeef", &cancel()).expect_err("count");
        assert!(matches!(err, PatchError::TooManyOps { limit } if limit == MAX_PATCH_OPS));
    }

    #[test]
    fn hello_preimage_matches_known_digest() {
        assert_eq!(hello_preimage().to_string(), HELLO_PREIMAGE);
    }

    #[test]
    fn metadata_with_intent_goal_evidence_and_message_round_trips() {
        let meta = metadata("refactor auth")
            .with_goal(goal())
            .with_evidence([evidence()])
            .with_message("drop the legacy token path");
        let built = SemanticPatch::with_metadata(
            vec![replace("src/lib.rs", 0, 5, "world")],
            author(),
            "deadbeef",
            meta,
            &cancel(),
        )
        .expect("with metadata");
        assert_eq!(built.metadata().intent(), "refactor auth");
        assert_eq!(built.metadata().goal(), Some(goal()));
        assert_eq!(built.metadata().evidence(), &[evidence()]);
        assert_eq!(
            built.metadata().message(),
            Some("drop the legacy token path")
        );

        let json = serde_json::to_string(&built).expect("serialize");
        assert!(json.contains("\"intent\":\"refactor auth\""));
        assert!(json.contains("\"goal\":\"018f3c8a-7e2b-7a10-8c4d-0123456789af\""));
        assert!(json.contains("\"evidence\":[\"018f3c8a-7e2b-7a10-8c4d-0123456789ae\"]"));
        assert!(json.contains("\"message\":\"drop the legacy token path\""));

        let decoded = serde_json::from_str::<SemanticPatch>(&json).expect("decode");
        assert_eq!(decoded, built);
    }

    #[test]
    fn metadata_is_part_of_patch_identity() {
        let base = patch(vec![replace("src/lib.rs", 0, 5, "world")]);
        let attributed = SemanticPatch::with_metadata(
            vec![replace("src/lib.rs", 0, 5, "world")],
            author(),
            "deadbeef",
            metadata("rename the field"),
            &cancel(),
        )
        .expect("attributed");
        let base_hash = base.hash(&cancel()).expect("base hash");
        let attributed_hash = attributed.hash(&cancel()).expect("attributed hash");
        assert_ne!(base_hash, attributed_hash);
        assert_eq!(
            base_hash,
            SemanticPatch::new(
                vec![replace("src/lib.rs", 0, 5, "world")],
                author(),
                "deadbeef",
                &cancel()
            )
            .expect("default metadata patch")
            .hash(&cancel())
            .expect("default metadata hash")
        );
    }

    #[test]
    fn set_metadata_replaces_block_and_revalidates() {
        let mut patch = patch(vec![replace("src/lib.rs", 0, 5, "world")]);
        patch
            .set_metadata(metadata("change intent").with_message("now attributed"))
            .expect("set metadata");
        assert_eq!(patch.metadata().intent(), "change intent");
        assert_eq!(patch.metadata().message(), Some("now attributed"));
        // Replacing metadata changes the patch identity.
        assert_ne!(
            patch.hash(&cancel()).expect("hash"),
            SemanticPatch::new(
                vec![replace("src/lib.rs", 0, 5, "world")],
                author(),
                "deadbeef",
                &cancel()
            )
            .expect("default")
            .hash(&cancel())
            .expect("default hash")
        );
    }

    #[test]
    fn failed_set_metadata_restores_previous_block() {
        let mut patch = patch(vec![replace("src/lib.rs", 0, 5, "world")]);
        patch
            .set_metadata(metadata("first").with_message("kept"))
            .expect("valid metadata");
        let before = patch.metadata().clone();
        assert_eq!(
            patch.set_metadata(metadata("second").with_message("bad\u{0002}m")),
            Err(PatchError::InvalidMessage)
        );
        assert_eq!(patch.metadata(), &before);
    }

    #[test]
    fn blank_intent_is_rejected() {
        assert_eq!(PatchMetadata::new(""), Err(PatchError::InvalidIntent));
        assert_eq!(PatchMetadata::new("   "), Err(PatchError::InvalidIntent));
        // A patch built through the default-metadata constructor stays valid
        // (intent is optional at the patch level, enforced at attribution).
        let empty_intent = patch(vec![replace("src/lib.rs", 0, 5, "world")]);
        assert_eq!(empty_intent.metadata().intent(), "");
    }

    #[test]
    fn overlong_intent_and_message_are_rejected() {
        let long_intent = "a".repeat(MAX_PATCH_INTENT_BYTES + 1);
        assert!(matches!(
            PatchMetadata::new(long_intent),
            Err(PatchError::IntentTooLong { limit }) if limit == MAX_PATCH_INTENT_BYTES
        ));
        let err = SemanticPatch::with_metadata(
            vec![replace("src/lib.rs", 0, 5, "world")],
            author(),
            "deadbeef",
            PatchMetadata::new("ok")
                .expect("intent")
                .with_message("b".repeat(MAX_PATCH_MESSAGE_BYTES + 1)),
            &cancel(),
        )
        .expect_err("long message");
        assert!(matches!(
            err,
            PatchError::MessageTooLong { limit } if limit == MAX_PATCH_MESSAGE_BYTES
        ));
    }

    #[test]
    fn control_characters_in_metadata_are_rejected() {
        assert!(matches!(
            PatchMetadata::new("bad\u{0000}intent"),
            Err(PatchError::InvalidIntent)
        ));
        let err = SemanticPatch::with_metadata(
            vec![replace("src/lib.rs", 0, 5, "world")],
            author(),
            "deadbeef",
            metadata("ok").with_message("bad\u{0001}message"),
            &cancel(),
        )
        .expect_err("control message");
        assert_eq!(err, PatchError::InvalidMessage);
    }

    #[test]
    fn too_many_and_duplicate_evidence_refs_are_rejected() {
        let too_many = (0..=MAX_PATCH_EVIDENCE_REFS)
            .map(|_| EvidenceId::new())
            .collect::<Vec<_>>();
        assert!(matches!(
            SemanticPatch::with_metadata(
                vec![replace("src/lib.rs", 0, 5, "world")],
                author(),
                "deadbeef",
                metadata("ok").with_evidence(too_many),
                &cancel(),
            ),
            Err(PatchError::TooManyEvidenceRefs { limit }) if limit == MAX_PATCH_EVIDENCE_REFS
        ));
        let dup = metadata("ok").with_evidence([evidence(), evidence()]);
        assert_eq!(dup.validate(), Err(PatchError::DuplicateEvidenceRef));
        assert_eq!(
            SemanticPatch::with_metadata(
                vec![replace("src/lib.rs", 0, 5, "world")],
                author(),
                "deadbeef",
                dup,
                &cancel(),
            )
            .expect_err("duplicate evidence"),
            PatchError::DuplicateEvidenceRef
        );
    }

    #[test]
    fn v1_patch_and_metadata_unknown_fields_are_rejected() {
        let v1 = r#"{"schema":"rapidlm.semantic_patch","schema_version":1,"author":"018f3c8a-7e2b-7a10-8c4d-0123456789ad","base_revision":"deadbeef","ops":[{"op":"delete_file","path":"src/lib.rs","preimage":"sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"}]}"#;
        assert!(serde_json::from_str::<SemanticPatch>(v1).is_err());
        let extra_meta = r#"{"schema":"rapidlm.semantic_patch","schema_version":2,"author":"018f3c8a-7e2b-7a10-8c4d-0123456789ad","base_revision":"deadbeef","metadata":{"intent":"","goal":null,"evidence":[],"message":null,"kind":"fix"},"ops":[{"op":"delete_file","path":"src/lib.rs","preimage":"sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"}]}"#;
        assert!(serde_json::from_str::<SemanticPatch>(extra_meta).is_err());
        let missing_meta = r#"{"schema":"rapidlm.semantic_patch","schema_version":2,"author":"018f3c8a-7e2b-7a10-8c4d-0123456789ad","base_revision":"deadbeef","ops":[{"op":"delete_file","path":"src/lib.rs","preimage":"sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"}]}"#;
        assert!(serde_json::from_str::<SemanticPatch>(missing_meta).is_err());
    }
}
