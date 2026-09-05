//! Skill discovery and activation.
//!
//! `SKILL.md` bundles are untrusted instruction/resource packages. Discovery
//! parses frontmatter and activation yields prompt data only. Skills cannot
//! issue capability leases or tool implementations (T-006, FR-EXT-005).
//! Project skills are ignored until an explicit project-trust grant.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, FileType};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use capability_broker::CancellationToken;
use protocol::{ApiError, ErrorCode, RepoPath, RepoPathError, TraceId};
use serde::de::{self, Deserializer};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::manifest::MAX_IDENT_BYTES;

/// Wire schema name for [`SkillDescriptor`].
pub const SKILL_DESCRIPTOR_SCHEMA: &str = "rapidlm.skill_descriptor";

/// v1 schema version for skill descriptors.
pub const SKILL_DESCRIPTOR_SCHEMA_VERSION: u16 = 1;

/// Canonical bundle filename.
pub const SKILL_FILE_NAME: &str = "SKILL.md";

/// User-config relative skill root (`~/.config/rapidlm/skills`).
pub const USER_SKILLS_DIR: &str = "skills";

/// Marketplace catalog relative skill root.
pub const MARKETPLACE_SKILLS_DIR: &str = "skills";

/// Project-controlled skill root. Inactive until project trust.
pub const PROJECT_SKILLS_DIR: &str = ".rapidlm/skills";

/// Default UTF-8 byte cap for one `SKILL.md`.
pub const DEFAULT_MAX_SKILL_BYTES: usize = 64 * 1024;

/// Default UTF-8 byte cap for one resource file.
pub const DEFAULT_MAX_RESOURCE_BYTES: usize = 64 * 1024;

/// Default UTF-8 byte cap for the instruction body.
pub const DEFAULT_MAX_INSTRUCTIONS_BYTES: usize = 64 * 1024;

/// Default UTF-8 byte cap for a description field.
pub const DEFAULT_MAX_DESCRIPTION_BYTES: usize = 1_024;

/// Default resource files accepted on one skill.
pub const DEFAULT_MAX_RESOURCES: usize = 16;

/// Default path-trigger globs accepted on one skill.
pub const DEFAULT_MAX_PATH_TRIGGERS: usize = 32;

/// Default discovered skills retained after merge.
pub const DEFAULT_MAX_SKILLS: usize = 64;

/// Default descriptive `allowed-tools` names on one skill.
pub const DEFAULT_MAX_ALLOWED_TOOLS: usize = 16;

/// Default directory children scanned under one skill root.
pub const DEFAULT_MAX_DIR_ENTRIES: usize = 256;

/// Default conservative token budget for one activation.
pub const DEFAULT_TOKEN_BUDGET: u32 = 4_000;

/// Default wall-clock bound for one discover or activate call.
pub const DEFAULT_MAX_DURATION: Duration = Duration::from_secs(1);

const CANCEL_STRIDE: usize = 16;
const DESCRIPTOR_FIELDS: &[&str] = &[
    "schema",
    "schema_version",
    "id",
    "description",
    "instructions_path",
    "resources",
];
const FRONTMATTER_KEYS: &[&str] = &[
    "name",
    "description",
    "paths",
    "user_invocable",
    "user-invocable",
    "model_invocable",
    "model-invocable",
    "resources",
    "allowed-tools",
    "allowed_tools",
    "update",
];
const PRIVILEGE_KEYS: &[&str] = &[
    "capability",
    "capabilities",
    "requested_caps",
    "requested-caps",
    "lease",
    "leases",
    "capability_lease",
    "capability-lease",
    "tools",
    "tool",
    "grant",
    "grants",
    "permission",
    "permissions",
    "privilege",
    "privileges",
    "command",
    "commands",
    "hook",
    "hooks",
    "wasm",
    "wit",
    "entrypoint",
    "plugin",
    "plugins",
    "mcp",
    "mcp_servers",
    "mcp-servers",
    "env",
    "environment",
    "executable",
    "exec",
    "run",
    "helper",
    "helpers",
];

/// Bounded skill identity. Same token rules as plugin extension refs.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SkillId(String);

/// Discovered skill identity and locator. Activation metadata is not a grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillDescriptor {
    id: SkillId,
    description: String,
    instructions_path: RepoPath,
    resources: Vec<RepoPath>,
}

/// Frontmatter contract from `plugins-skills-and-hooks.md`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillManifest {
    name: SkillId,
    description: String,
    paths: Vec<RepoPath>,
    user_invocable: bool,
    model_invocable: bool,
    resources: Vec<RepoPath>,
}

/// Where a bundle was found. Not a privilege grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SkillOrigin {
    User,
    Marketplace,
    Project,
}

/// Project-trust input from the kernel store. Default is untrusted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum ProjectTrust {
    #[default]
    Untrusted,
    Trusted,
}

/// Declared upstream update pointer from skill frontmatter (`update`).
/// Descriptive only: it never triggers downloads or grants capability.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct UpdatePointer {
    channel: String,
    version: String,
}

impl UpdatePointer {
    /// Parse `channel@version`. Both sides must be non-empty and bounded.
    pub fn parse(raw: &str) -> Result<Self, SkillError> {
        const MAX_UPDATE_BYTES: usize = 128;
        if raw.is_empty() || raw.len() > MAX_UPDATE_BYTES {
            return Err(SkillError::InvalidFrontmatter);
        }
        let (channel, version) = raw.split_once('@').ok_or(SkillError::InvalidFrontmatter)?;
        if channel.is_empty() || version.is_empty() {
            return Err(SkillError::InvalidFrontmatter);
        }
        if !valid_update_token(channel) || !valid_update_token(version) {
            return Err(SkillError::InvalidFrontmatter);
        }
        Ok(Self {
            channel: channel.to_owned(),
            version: version.to_owned(),
        })
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn version(&self) -> &str {
        &self.version
    }
}

fn valid_update_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 64
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Marketplace metadata triple required by the architecture seam:
/// where a bundle came from, whether its root is trusted, and any declared
/// upstream update pointer. Metadata never grants capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarketplaceMetadata {
    origin: SkillOrigin,
    trust: ProjectTrust,
    update: Option<UpdatePointer>,
}

impl MarketplaceMetadata {
    pub fn of(
        origin: SkillOrigin,
        trust: ProjectTrust,
        update: Option<UpdatePointer>,
    ) -> Self {
        Self {
            origin,
            trust,
            update,
        }
    }

    pub fn source(&self) -> SkillOrigin {
        self.origin
    }

    pub fn trust(&self) -> ProjectTrust {
        self.trust
    }

    pub fn update(&self) -> Option<&UpdatePointer> {
        self.update.as_ref()
    }
}

/// Filesystem roots plus the project-trust gate.
#[derive(Clone, Debug, Default)]
pub struct SkillRoots {
    project_root: Option<PathBuf>,
    user_root: Option<PathBuf>,
    marketplace_root: Option<PathBuf>,
    project_trust: ProjectTrust,
}

/// Resource bounds for one discover/activate call. Zero timeout is immediate.
#[derive(Clone, Debug)]
pub struct SkillLimits {
    max_skill_bytes: usize,
    max_resource_bytes: usize,
    max_instructions_bytes: usize,
    max_description_bytes: usize,
    max_resources: usize,
    max_path_triggers: usize,
    max_skills: usize,
    max_allowed_tools: usize,
    max_dir_entries: usize,
    token_budget: u32,
    timeout: Duration,
    cancel: CancellationToken,
}

/// Discovered bundle. `allowed-tools` is descriptive/narrowing only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillRecord {
    descriptor: SkillDescriptor,
    origin: SkillOrigin,
    user_invocable: bool,
    model_invocable: bool,
    paths: Vec<RepoPath>,
    allowed_tools: Vec<String>,
    marketplace_update: Option<UpdatePointer>,
    skill_file: PathBuf,
}

/// Path/task metadata used to decide activation. Empty paths never auto-inject.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ActivationQuery {
    task_paths: Vec<RepoPath>,
    user_requested: Option<SkillId>,
}

/// Prompt-only activation result. No lease, capability, or tool field exists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillPromptData {
    descriptor: SkillDescriptor,
    instructions: String,
    resources: Vec<SkillResource>,
    token_estimate: u32,
}

/// Resource file loaded as prompt text. Never executed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillResource {
    path: RepoPath,
    text: String,
}

/// Host-owned loader. Callers supply trust; this module never grants it.
pub struct SkillLoader {
    roots: SkillRoots,
    limits: SkillLimits,
}

/// Typed parse/discover/activate failure. Display never echoes file text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum SkillError {
    Cancelled,
    Timeout,
    TooLarge,
    TooManySkills,
    TooManyResources,
    TooManyPaths,
    TooManyTools,
    InvalidUtf8,
    MissingFrontmatter,
    InvalidFrontmatter,
    UnknownField,
    MissingField,
    InvalidIdent,
    InvalidPath,
    InvalidDescription,
    PrivilegeInjection,
    ProjectUntrusted,
    NotFound,
    NotInvocable,
    NotTriggered,
    TokenBudget,
    Symlink,
    Unreadable,
    EmptySkill,
}

impl SkillId {
    pub fn parse(value: &str) -> Result<Self, SkillError> {
        Ok(Self(parse_ident(value)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for SkillId {
    type Err = SkillError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl SkillDescriptor {
    pub fn id(&self) -> &SkillId {
        &self.id
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn instructions_path(&self) -> &RepoPath {
        &self.instructions_path
    }

    pub fn resources(&self) -> &[RepoPath] {
        &self.resources
    }
}

impl SkillManifest {
    pub fn name(&self) -> &SkillId {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn paths(&self) -> &[RepoPath] {
        &self.paths
    }

    pub fn user_invocable(&self) -> bool {
        self.user_invocable
    }

    pub fn model_invocable(&self) -> bool {
        self.model_invocable
    }

    pub fn resources(&self) -> &[RepoPath] {
        &self.resources
    }
}

impl ProjectTrust {
    pub const fn is_trusted(self) -> bool {
        matches!(self, Self::Trusted)
    }
}

impl SkillRoots {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn project_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.project_root = Some(root.into());
        self
    }

    pub fn user_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.user_root = Some(root.into());
        self
    }

    pub fn marketplace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.marketplace_root = Some(root.into());
        self
    }

    pub fn project_trust(mut self, trust: ProjectTrust) -> Self {
        self.project_trust = trust;
        self
    }

    pub fn trust(&self) -> ProjectTrust {
        self.project_trust
    }
}

impl SkillLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn token_budget(mut self, value: u32) -> Self {
        self.token_budget = value;
        self
    }

    pub fn timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }

    pub fn cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn max_skill_bytes(mut self, value: usize) -> Self {
        self.max_skill_bytes = value;
        self
    }

    pub fn max_resource_bytes(mut self, value: usize) -> Self {
        self.max_resource_bytes = value;
        self
    }

    pub fn max_skills(mut self, value: usize) -> Self {
        self.max_skills = value;
        self
    }
}

impl Default for SkillLimits {
    fn default() -> Self {
        Self {
            max_skill_bytes: DEFAULT_MAX_SKILL_BYTES,
            max_resource_bytes: DEFAULT_MAX_RESOURCE_BYTES,
            max_instructions_bytes: DEFAULT_MAX_INSTRUCTIONS_BYTES,
            max_description_bytes: DEFAULT_MAX_DESCRIPTION_BYTES,
            max_resources: DEFAULT_MAX_RESOURCES,
            max_path_triggers: DEFAULT_MAX_PATH_TRIGGERS,
            max_skills: DEFAULT_MAX_SKILLS,
            max_allowed_tools: DEFAULT_MAX_ALLOWED_TOOLS,
            max_dir_entries: DEFAULT_MAX_DIR_ENTRIES,
            token_budget: DEFAULT_TOKEN_BUDGET,
            timeout: DEFAULT_MAX_DURATION,
            cancel: CancellationToken::new(),
        }
    }
}

impl ActivationQuery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn task_paths<I, S>(mut self, paths: I) -> Result<Self, SkillError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut out = Vec::new();
        for path in paths {
            out.push(parse_repo_path(path.as_ref())?);
        }
        self.task_paths = out;
        Ok(self)
    }

    pub fn user_requested(mut self, id: SkillId) -> Self {
        self.user_requested = Some(id);
        self
    }

    pub fn task_path_refs(&self) -> &[RepoPath] {
        &self.task_paths
    }

    pub fn requested_id(&self) -> Option<&SkillId> {
        self.user_requested.as_ref()
    }
}

impl SkillRecord {
    pub fn descriptor(&self) -> &SkillDescriptor {
        &self.descriptor
    }

    pub fn origin(&self) -> SkillOrigin {
        self.origin
    }

    pub fn user_invocable(&self) -> bool {
        self.user_invocable
    }

    pub fn model_invocable(&self) -> bool {
        self.model_invocable
    }

    pub fn paths(&self) -> &[RepoPath] {
        &self.paths
    }

    /// Descriptive/narrowing metadata only. Never a capability grant.
    pub fn descriptive_allowed_tools(&self) -> &[String] {
        &self.allowed_tools
    }

    /// Declared upstream update pointer from frontmatter, when present.
    pub fn marketplace_update(&self) -> Option<&UpdatePointer> {
        self.marketplace_update.as_ref()
    }

    /// Marketplace seam triple: source/trust/update. Trust is supplied by
    /// the caller (kernel project-trust store), never inferred from content.
    pub fn marketplace_metadata(&self, trust: ProjectTrust) -> MarketplaceMetadata {
        MarketplaceMetadata::of(self.origin, trust, self.marketplace_update.clone())
    }

    pub fn matches(&self, query: &ActivationQuery) -> bool {
        if query
            .user_requested
            .as_ref()
            .is_some_and(|id| id == &self.descriptor.id)
        {
            return self.user_invocable;
        }
        if !self.model_invocable || self.paths.is_empty() {
            return false;
        }
        query.task_paths.iter().any(|task| {
            self.paths
                .iter()
                .any(|glob| glob_matches(glob.as_str(), task.as_str()))
        })
    }
}

impl SkillPromptData {
    pub fn descriptor(&self) -> &SkillDescriptor {
        &self.descriptor
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    pub fn resources(&self) -> &[SkillResource] {
        &self.resources
    }

    pub fn token_estimate(&self) -> u32 {
        self.token_estimate
    }
}

impl SkillResource {
    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

impl SkillLoader {
    pub fn new(roots: SkillRoots, limits: SkillLimits) -> Self {
        Self { roots, limits }
    }

    pub fn discover(&self) -> Result<Vec<SkillRecord>, SkillError> {
        discover(&self.roots, &self.limits)
    }

    pub fn activate(
        &self,
        id: &SkillId,
        query: &ActivationQuery,
    ) -> Result<SkillPromptData, SkillError> {
        let records = discover(&self.roots, &self.limits)?;
        let record = records
            .iter()
            .find(|record| record.descriptor.id() == id)
            .ok_or(not_found_or_untrusted(id, &self.roots))?;
        activate(record, &self.limits, query)
    }

    pub fn activate_matching(
        &self,
        query: &ActivationQuery,
    ) -> Result<Vec<SkillPromptData>, SkillError> {
        activate_matching(&self.roots, &self.limits, query)
    }
}

/// Discover bundles from user, marketplace, and trusted project roots.
pub fn discover(roots: &SkillRoots, limits: &SkillLimits) -> Result<Vec<SkillRecord>, SkillError> {
    let started = Instant::now();
    check_limits(limits, started)?;
    let mut found = Vec::new();
    if let Some(user) = &roots.user_root {
        scan_root(
            user,
            USER_SKILLS_DIR,
            SkillOrigin::User,
            None,
            limits,
            started,
            &mut found,
        )?;
    }
    if let Some(market) = &roots.marketplace_root {
        scan_root(
            market,
            MARKETPLACE_SKILLS_DIR,
            SkillOrigin::Marketplace,
            None,
            limits,
            started,
            &mut found,
        )?;
    }
    if let Some(project) = &roots.project_root
        && roots.project_trust.is_trusted() {
            scan_root(
                project,
                PROJECT_SKILLS_DIR,
                SkillOrigin::Project,
                Some(project.as_path()),
                limits,
                started,
                &mut found,
            )?;
        }
    merge_records(found, limits)
}

/// Activate one already-discovered record into prompt data only.
pub fn activate(
    record: &SkillRecord,
    limits: &SkillLimits,
    query: &ActivationQuery,
) -> Result<SkillPromptData, SkillError> {
    let started = Instant::now();
    check_limits(limits, started)?;
    if !record.matches(query) {
        return Err(activation_mismatch(record, query));
    }
    load_prompt(record, limits, started, limits.token_budget, true)
}

/// Activate every matching skill that fits the remaining token budget.
pub fn activate_matching(
    roots: &SkillRoots,
    limits: &SkillLimits,
    query: &ActivationQuery,
) -> Result<Vec<SkillPromptData>, SkillError> {
    let started = Instant::now();
    check_limits(limits, started)?;
    let records = discover(roots, limits)?;
    let mut remaining = limits.token_budget;
    let mut out = Vec::new();
    for (i, record) in records.iter().enumerate() {
        if i % CANCEL_STRIDE == 0 {
            check_limits(limits, started)?;
        }
        if !record.matches(query) {
            continue;
        }
        let user_required = query
            .user_requested
            .as_ref()
            .is_some_and(|id| id == record.descriptor.id());
        match load_prompt(record, limits, started, remaining, user_required) {
            Ok(data) => {
                remaining = remaining.saturating_sub(data.token_estimate);
                out.push(data);
            }
            Err(SkillError::TokenBudget) if !user_required => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(out)
}

/// Parse one `SKILL.md` document. Privilege keys fail closed.
pub fn parse_skill(
    bytes: &[u8],
    limits: &SkillLimits,
) -> Result<(SkillManifest, String), SkillError> {
    parse_skill_inner(bytes, limits, Instant::now())
}

fn parse_skill_inner(
    bytes: &[u8],
    limits: &SkillLimits,
    started: Instant,
) -> Result<(SkillManifest, String), SkillError> {
    check_limits(limits, started)?;
    if bytes.len() > limits.max_skill_bytes {
        return Err(SkillError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| SkillError::InvalidUtf8)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text.strip_prefix("---") else {
        return Err(SkillError::MissingFrontmatter);
    };
    let rest = strip_one_newline(rest);
    let Some((front, body)) = split_frontmatter(rest) else {
        return Err(SkillError::MissingFrontmatter);
    };
    let parsed = parse_frontmatter(front, limits, started)?;
    let instructions = strip_one_newline(body);
    if instructions.len() > limits.max_instructions_bytes {
        return Err(SkillError::TooLarge);
    }
    if instructions.trim().is_empty() && parsed.manifest.resources.is_empty() {
        return Err(SkillError::EmptySkill);
    }
    Ok((parsed.manifest, instructions.to_owned()))
}

struct ParsedFrontmatter {
    manifest: SkillManifest,
    allowed_tools: Vec<String>,
}

fn parse_frontmatter(
    front: &str,
    limits: &SkillLimits,
    started: Instant,
) -> Result<ParsedFrontmatter, SkillError> {
    check_limits(limits, started)?;
    let fields = decode_frontmatter_map(front, limits, started)?;
    let name = SkillId::parse(require_field(&fields, "name")?)?;
    let description = parse_description(require_field(&fields, "description")?, limits)?;
    let paths = parse_path_list(
        optional_list(&fields, "paths"),
        limits.max_path_triggers,
        SkillError::TooManyPaths,
    )?;
    let resources = parse_path_list(
        optional_list(&fields, "resources"),
        limits.max_resources,
        SkillError::TooManyResources,
    )?;
    let user_invocable = optional_bool(&fields, &["user_invocable", "user-invocable"], true)?;
    let model_invocable = optional_bool(&fields, &["model_invocable", "model-invocable"], true)?;
    let allowed_raw =
        optional_list(&fields, "allowed-tools").or_else(|| optional_list(&fields, "allowed_tools"));
    let allowed_tools = parse_tool_names(allowed_raw, limits.max_allowed_tools)?;
    Ok(ParsedFrontmatter {
        manifest: SkillManifest {
            name,
            description,
            paths,
            user_invocable,
            model_invocable,
            resources,
        },
        allowed_tools,
    })
}

fn scan_root(
    root: &Path,
    rel: &str,
    origin: SkillOrigin,
    project_root: Option<&Path>,
    limits: &SkillLimits,
    started: Instant,
    out: &mut Vec<SkillRecord>,
) -> Result<(), SkillError> {
    check_limits(limits, started)?;
    let skills_dir = join_rel(root, rel);
    let entries = match fs::read_dir(&skills_dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    let mut seen = 0usize;
    for entry in entries {
        if seen.is_multiple_of(CANCEL_STRIDE) {
            check_limits(limits, started)?;
        }
        seen = seen.saturating_add(1);
        if seen > limits.max_dir_entries {
            return Err(SkillError::TooManySkills);
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if let Some(record) = load_bundle_entry(
            &entry.path(),
            &file_type,
            origin,
            project_root,
            limits,
            started,
        )? {
            out.push(record);
        }
    }
    Ok(())
}

fn load_bundle_entry(
    dir: &Path,
    file_type: &FileType,
    origin: SkillOrigin,
    project_root: Option<&Path>,
    limits: &SkillLimits,
    started: Instant,
) -> Result<Option<SkillRecord>, SkillError> {
    if file_type.is_symlink() || !file_type.is_dir() {
        return Ok(None);
    }
    let dir_name = match dir.file_name().and_then(|name| name.to_str()) {
        Some(name) => name,
        None => return Ok(None),
    };
    if parse_ident(dir_name).is_err() {
        return Ok(None);
    }
    let skill_file = dir.join(SKILL_FILE_NAME);
    match load_record(&skill_file, origin, dir_name, project_root, limits, started) {
        Ok(record) => Ok(Some(record)),
        Err(err @ (SkillError::Cancelled | SkillError::Timeout | SkillError::TooManySkills)) => {
            Err(err)
        }
        Err(_) => Ok(None),
    }
}

fn load_record(
    skill_file: &Path,
    origin: SkillOrigin,
    dir_name: &str,
    project_root: Option<&Path>,
    limits: &SkillLimits,
    started: Instant,
) -> Result<SkillRecord, SkillError> {
    let bytes = read_bounded(skill_file, limits.max_skill_bytes)?;
    let (manifest, _instructions) = parse_skill_inner(&bytes, limits, started)?;
    let allowed = parse_skill_allowed_tools(&bytes, limits, started)?;
    let instructions_path = instructions_locator(origin, dir_name, project_root, skill_file)?;
    Ok(SkillRecord {
        descriptor: SkillDescriptor {
            id: manifest.name.clone(),
            description: manifest.description.clone(),
            instructions_path,
            resources: manifest.resources.clone(),
        },
        origin,
        user_invocable: manifest.user_invocable,
        model_invocable: manifest.model_invocable,
        paths: manifest.paths,
        allowed_tools: allowed,
        marketplace_update: parse_skill_update(skill_file, limits, started)?,
        skill_file: skill_file.to_path_buf(),
    })
}

fn parse_skill_allowed_tools(
    bytes: &[u8],
    limits: &SkillLimits,
    started: Instant,
) -> Result<Vec<String>, SkillError> {
    let text = std::str::from_utf8(bytes).map_err(|_| SkillError::InvalidUtf8)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text
        .strip_prefix("---")
        .ok_or(SkillError::MissingFrontmatter)?;
    let rest = strip_one_newline(rest);
    let (front, _) = split_frontmatter(rest).ok_or(SkillError::MissingFrontmatter)?;
    Ok(parse_frontmatter(front, limits, started)?.allowed_tools)
}

/// Parse the optional `update` frontmatter pointer (`channel@version`).
fn parse_skill_update(
    skill_file: &Path,
    limits: &SkillLimits,
    started: Instant,
) -> Result<Option<UpdatePointer>, SkillError> {
    let bytes = read_bounded(skill_file, limits.max_skill_bytes)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| SkillError::InvalidUtf8)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text
        .strip_prefix("---")
        .ok_or(SkillError::MissingFrontmatter)?;
    let rest = strip_one_newline(rest);
    let (front, _) = split_frontmatter(rest).ok_or(SkillError::MissingFrontmatter)?;
    check_limits(limits, started)?;
    let fields = decode_frontmatter_map(front, limits, started)?;
    match fields.values.iter().find(|(key, _)| key == "update") {
        Some((_, FrontmatterValue::Scalar(raw))) => {
            UpdatePointer::parse(raw).map(Some)
        }
        Some((_, FrontmatterValue::List(_))) => Err(SkillError::InvalidFrontmatter),
        None => Ok(None),
    }
}

fn load_prompt(
    record: &SkillRecord,
    limits: &SkillLimits,
    started: Instant,
    budget: u32,
    required: bool,
) -> Result<SkillPromptData, SkillError> {
    check_limits(limits, started)?;
    let bundle_dir = record.skill_file.parent().ok_or(SkillError::Unreadable)?;
    let bytes = read_skill_file(&record.skill_file, limits.max_skill_bytes)?;
    let (manifest, instructions) = parse_skill_inner(&bytes, limits, started)?;
    if manifest.name != record.descriptor.id {
        return Err(SkillError::InvalidIdent);
    }
    let instruction_tokens = estimate_tokens(&instructions);
    if instruction_tokens > budget {
        return Err(SkillError::TokenBudget);
    }
    let mut remaining = budget.saturating_sub(instruction_tokens);
    let mut resources = Vec::new();
    for (i, path) in manifest.resources.iter().enumerate() {
        if i % CANCEL_STRIDE == 0 {
            check_limits(limits, started)?;
        }
        let bytes = match read_confined(bundle_dir, path, limits.max_resource_bytes) {
            Ok(bytes) => bytes,
            Err(err) if !required => return Err(err),
            Err(err) => return Err(err),
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| SkillError::InvalidUtf8)?;
        let tokens = estimate_tokens(text);
        if tokens > remaining {
            if required && resources.is_empty() && instruction_tokens == 0 {
                return Err(SkillError::TokenBudget);
            }
            continue;
        }
        remaining = remaining.saturating_sub(tokens);
        resources.push(SkillResource {
            path: path.clone(),
            text: text.to_owned(),
        });
    }
    let token_estimate = budget.saturating_sub(remaining);
    Ok(SkillPromptData {
        descriptor: SkillDescriptor {
            id: manifest.name,
            description: manifest.description,
            instructions_path: record.descriptor.instructions_path.clone(),
            resources: resources.iter().map(|item| item.path.clone()).collect(),
        },
        instructions,
        resources,
        token_estimate,
    })
}

fn merge_records(
    mut records: Vec<SkillRecord>,
    limits: &SkillLimits,
) -> Result<Vec<SkillRecord>, SkillError> {
    records.sort_by(|left, right| {
        left.descriptor
            .id
            .cmp(&right.descriptor.id)
            .then(origin_rank(left.origin).cmp(&origin_rank(right.origin)))
    });
    let mut out = Vec::new();
    for record in records {
        if out
            .last()
            .is_some_and(|prev: &SkillRecord| prev.descriptor.id == record.descriptor.id)
        {
            continue;
        }
        out.push(record);
    }
    if out.len() > limits.max_skills {
        return Err(SkillError::TooManySkills);
    }
    Ok(out)
}

fn origin_rank(origin: SkillOrigin) -> u8 {
    match origin {
        SkillOrigin::User => 0,
        SkillOrigin::Marketplace => 1,
        SkillOrigin::Project => 2,
    }
}

fn not_found_or_untrusted(id: &SkillId, roots: &SkillRoots) -> SkillError {
    if !roots.project_trust.is_trusted()
        && let Some(project) = &roots.project_root {
            let dir = join_rel(project, PROJECT_SKILLS_DIR).join(id.as_str());
            if let Ok(meta) = fs::symlink_metadata(&dir)
                && meta.file_type().is_dir() && !meta.file_type().is_symlink() {
                    return SkillError::ProjectUntrusted;
                }
        }
    SkillError::NotFound
}

fn activation_mismatch(record: &SkillRecord, query: &ActivationQuery) -> SkillError {
    if query
        .user_requested
        .as_ref()
        .is_some_and(|id| id == record.descriptor.id())
        && !record.user_invocable
    {
        SkillError::NotInvocable
    } else {
        SkillError::NotTriggered
    }
}

fn check_limits(limits: &SkillLimits, started: Instant) -> Result<(), SkillError> {
    if limits.cancel.is_cancelled() {
        return Err(SkillError::Cancelled);
    }
    if limits.timeout.is_zero() || started.elapsed() > limits.timeout {
        return Err(SkillError::Timeout);
    }
    Ok(())
}

fn read_skill_file(path: &Path, max: usize) -> Result<Vec<u8>, SkillError> {
    let parent = path.parent().ok_or(SkillError::Unreadable)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(SkillError::InvalidPath)?;
    read_confined(parent, &parse_repo_path(name)?, max)
}

fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>, SkillError> {
    read_skill_file(path, max)
}

fn read_confined(root: &Path, relative: &RepoPath, max: usize) -> Result<Vec<u8>, SkillError> {
    let _ = resolve_under(root, relative)?;
    let file = open_confined(root, relative)?;
    read_file_bounded(file, max)
}

fn read_file_bounded(file: File, max: usize) -> Result<Vec<u8>, SkillError> {
    let meta = file.metadata().map_err(|_| SkillError::Unreadable)?;
    if meta.file_type().is_symlink() {
        return Err(SkillError::Symlink);
    }
    if !meta.is_file() {
        return Err(SkillError::Unreadable);
    }
    if meta.len() > max as u64 {
        return Err(SkillError::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SkillError::Unreadable)?;
    if bytes.len() > max {
        return Err(SkillError::TooLarge);
    }
    Ok(bytes)
}

fn validate_path_component(component: &str) -> Result<(), SkillError> {
    if component.is_empty() || component == ".." || component == "." {
        return Err(SkillError::InvalidPath);
    }
    if component.contains('\0') || component.contains('/') || component.contains('\\') {
        return Err(SkillError::InvalidPath);
    }
    Ok(())
}

/// Lstat every hop from the bundle root. Any file or directory symlink fails closed.
fn resolve_under(root: &Path, relative: &RepoPath) -> Result<PathBuf, SkillError> {
    let root_meta = fs::symlink_metadata(root).map_err(|_| SkillError::Unreadable)?;
    if root_meta.file_type().is_symlink() {
        return Err(SkillError::Symlink);
    }
    if !root_meta.is_dir() {
        return Err(SkillError::Unreadable);
    }
    let mut out = root.to_path_buf();
    let mut any = false;
    for component in relative.components() {
        validate_path_component(component)?;
        out.push(component);
        let meta = fs::symlink_metadata(&out).map_err(|_| SkillError::Unreadable)?;
        if meta.file_type().is_symlink() {
            return Err(SkillError::Symlink);
        }
        any = true;
    }
    if !any {
        return Err(SkillError::InvalidPath);
    }
    Ok(out)
}

#[cfg(unix)]
fn open_confined(root: &Path, relative: &RepoPath) -> Result<File, SkillError> {
    use rustix::fs::{open, openat, statat, AtFlags, FileType as RxFileType, Mode, OFlags};

    let names: Vec<&str> = relative.components().collect();
    if names.is_empty() {
        return Err(SkillError::InvalidPath);
    }
    for name in &names {
        validate_path_component(name)?;
    }

    let root_stat = rustix::fs::lstat(root).map_err(map_rustix_err)?;
    if RxFileType::from_raw_mode(root_stat.st_mode) == RxFileType::Symlink {
        return Err(SkillError::Symlink);
    }
    if RxFileType::from_raw_mode(root_stat.st_mode) != RxFileType::Directory {
        return Err(SkillError::Unreadable);
    }

    let mut dir = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(map_rustix_err)?;

    for (i, name) in names.iter().enumerate() {
        let last = i + 1 == names.len();
        let stat = statat(&dir, *name, AtFlags::SYMLINK_NOFOLLOW).map_err(map_rustix_err)?;
        let kind = RxFileType::from_raw_mode(stat.st_mode);
        if kind == RxFileType::Symlink {
            return Err(SkillError::Symlink);
        }
        if last {
            if kind != RxFileType::RegularFile {
                return Err(SkillError::Unreadable);
            }
            let fd = openat(
                &dir,
                *name,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(map_rustix_err)?;
            return Ok(File::from(fd));
        }
        if kind != RxFileType::Directory {
            return Err(SkillError::Unreadable);
        }
        dir = openat(
            &dir,
            *name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(map_rustix_err)?;
    }
    Err(SkillError::Unreadable)
}

#[cfg(not(unix))]
fn open_confined(root: &Path, relative: &RepoPath) -> Result<File, SkillError> {
    let path = resolve_under(root, relative)?;
    let meta = fs::symlink_metadata(&path).map_err(|_| SkillError::Unreadable)?;
    if meta.file_type().is_symlink() {
        return Err(SkillError::Symlink);
    }
    if !meta.is_file() {
        return Err(SkillError::Unreadable);
    }
    File::open(path).map_err(|_| SkillError::Unreadable)
}

#[cfg(unix)]
fn map_rustix_err(err: rustix::io::Errno) -> SkillError {
    if err == rustix::io::Errno::LOOP {
        SkillError::Symlink
    } else {
        SkillError::Unreadable
    }
}

fn join_rel(root: &Path, rel: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for part in rel.split('/') {
        if !part.is_empty() {
            out.push(part);
        }
    }
    out
}

fn instructions_locator(
    origin: SkillOrigin,
    dir_name: &str,
    project_root: Option<&Path>,
    skill_file: &Path,
) -> Result<RepoPath, SkillError> {
    match origin {
        SkillOrigin::Project => {
            if let Some(project) = project_root
                && let Some(rel) = strip_prefix_path(skill_file, project) {
                    return parse_repo_path(&rel);
                }
            parse_repo_path(&format!(
                "{PROJECT_SKILLS_DIR}/{dir_name}/{SKILL_FILE_NAME}"
            ))
        }
        SkillOrigin::User => {
            parse_repo_path(&format!("{USER_SKILLS_DIR}/{dir_name}/{SKILL_FILE_NAME}"))
        }
        SkillOrigin::Marketplace => parse_repo_path(&format!(
            "marketplace/{MARKETPLACE_SKILLS_DIR}/{dir_name}/{SKILL_FILE_NAME}"
        )),
    }
}

fn strip_prefix_path(path: &Path, root: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        let text = component.as_os_str().to_str()?;
        if text == ".." {
            return None;
        }
        if text != "." {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn split_frontmatter(rest: &str) -> Option<(&str, &str)> {
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            let front = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Some((front, body));
        }
        offset += line.len();
    }
    None
}

fn strip_one_newline(text: &str) -> &str {
    text.strip_prefix("\r\n")
        .or_else(|| text.strip_prefix('\n'))
        .unwrap_or(text)
}

struct FrontmatterFields {
    values: Vec<(String, FrontmatterValue)>,
}

enum FrontmatterValue {
    Scalar(String),
    List(Vec<String>),
}

fn decode_frontmatter_map(
    front: &str,
    limits: &SkillLimits,
    started: Instant,
) -> Result<FrontmatterFields, SkillError> {
    let mut values = Vec::new();
    let mut current_key: Option<String> = None;
    let mut current_list: Option<Vec<String>> = None;
    for (i, raw_line) in front.lines().enumerate() {
        if i % CANCEL_STRIDE == 0 {
            check_limits(limits, started)?;
        }
        if raw_line.trim().is_empty() || raw_line.trim_start().starts_with('#') {
            continue;
        }
        if let Some(item) = parse_list_item(raw_line) {
            let list = current_list
                .as_mut()
                .ok_or(SkillError::InvalidFrontmatter)?;
            list.push(item?);
            continue;
        }
        if raw_line.starts_with(' ') || raw_line.starts_with('\t') {
            return Err(SkillError::InvalidFrontmatter);
        }
        flush_field(&mut values, &mut current_key, &mut current_list)?;
        let (key, value) = parse_key_line(raw_line)?;
        reject_privilege_key(&key)?;
        if !FRONTMATTER_KEYS.contains(&key.as_str()) {
            return Err(SkillError::UnknownField);
        }
        if values.iter().any(|(existing, _)| existing == &key) || alias_conflict(&values, &key) {
            return Err(SkillError::InvalidFrontmatter);
        }
        match value {
            None => {
                current_key = Some(key);
                current_list = Some(Vec::new());
            }
            Some(FrontmatterValue::List(list)) => {
                values.push((key, FrontmatterValue::List(list)));
                current_key = None;
                current_list = None;
            }
            Some(FrontmatterValue::Scalar(scalar)) => {
                values.push((key, FrontmatterValue::Scalar(scalar)));
                current_key = None;
                current_list = None;
            }
        }
    }
    flush_field(&mut values, &mut current_key, &mut current_list)?;
    Ok(FrontmatterFields { values })
}

fn alias_conflict(values: &[(String, FrontmatterValue)], key: &str) -> bool {
    const ALIASES: &[&[&str]] = &[
        &["user_invocable", "user-invocable"],
        &["model_invocable", "model-invocable"],
        &["allowed-tools", "allowed_tools"],
    ];
    ALIASES.iter().any(|group| {
        group.contains(&key)
            && values
                .iter()
                .any(|(existing, _)| group.contains(&existing.as_str()))
    })
}

fn flush_field(
    values: &mut Vec<(String, FrontmatterValue)>,
    current_key: &mut Option<String>,
    current_list: &mut Option<Vec<String>>,
) -> Result<(), SkillError> {
    if let Some(key) = current_key.take() {
        let list = current_list.take().unwrap_or_default();
        values.push((key, FrontmatterValue::List(list)));
    }
    Ok(())
}

fn parse_key_line(line: &str) -> Result<(String, Option<FrontmatterValue>), SkillError> {
    let (raw_key, rest) = line.split_once(':').ok_or(SkillError::InvalidFrontmatter)?;
    let key = raw_key.trim();
    if key.is_empty() || key.contains(' ') {
        return Err(SkillError::InvalidFrontmatter);
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return Ok((key.to_owned(), None));
    }
    if rest == "[]" {
        return Ok((key.to_owned(), Some(FrontmatterValue::List(Vec::new()))));
    }
    if rest.starts_with('[') {
        return Err(SkillError::InvalidFrontmatter);
    }
    Ok((
        key.to_owned(),
        Some(FrontmatterValue::Scalar(parse_yaml_scalar(rest)?)),
    ))
}

fn parse_list_item(line: &str) -> Option<Result<String, SkillError>> {
    let trimmed = if let Some(rest) = line.strip_prefix("  - ") {
        rest
    } else if let Some(rest) = line.strip_prefix("\t- ") {
        rest
    } else {
        let rest = line.strip_prefix("- ")?;
        if line.starts_with('-') && !line.starts_with(' ') && !line.starts_with('\t') {
            return Some(Err(SkillError::InvalidFrontmatter));
        }
        rest
    };
    Some(parse_yaml_scalar(trimmed.trim()))
}

fn parse_yaml_scalar(raw: &str) -> Result<String, SkillError> {
    if raw.len() > DEFAULT_MAX_SKILL_BYTES {
        return Err(SkillError::TooLarge);
    }
    if raw.chars().any(char::is_control) {
        return Err(SkillError::InvalidFrontmatter);
    }
    if let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return unescape_quoted(inner);
    }
    if raw.starts_with('\'') {
        return Err(SkillError::InvalidFrontmatter);
    }
    Ok(raw.to_owned())
}

fn unescape_quoted(inner: &str) -> Result<String, SkillError> {
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(_) | None => return Err(SkillError::InvalidFrontmatter),
            }
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

fn reject_privilege_key(key: &str) -> Result<(), SkillError> {
    let normalized = key.replace('-', "_").to_ascii_lowercase();
    if PRIVILEGE_KEYS
        .iter()
        .any(|denied| denied.replace('-', "_") == normalized)
    {
        return Err(SkillError::PrivilegeInjection);
    }
    Ok(())
}

fn require_field<'a>(fields: &'a FrontmatterFields, key: &str) -> Result<&'a str, SkillError> {
    match fields
        .values
        .iter()
        .find(|(existing, _)| existing == key)
        .map(|(_, value)| value)
    {
        Some(FrontmatterValue::Scalar(value)) => Ok(value.as_str()),
        Some(FrontmatterValue::List(_)) => Err(SkillError::InvalidFrontmatter),
        None => Err(SkillError::MissingField),
    }
}

fn optional_list<'a>(fields: &'a FrontmatterFields, key: &str) -> Option<&'a [String]> {
    fields
        .values
        .iter()
        .find(|(existing, _)| existing == key)
        .and_then(|(_, value)| match value {
            FrontmatterValue::List(list) => Some(list.as_slice()),
            FrontmatterValue::Scalar(_) => None,
        })
}

fn optional_bool(
    fields: &FrontmatterFields,
    keys: &[&str],
    default: bool,
) -> Result<bool, SkillError> {
    for key in keys {
        if let Some((_, value)) = fields.values.iter().find(|(existing, _)| existing == key) {
            let raw = match value {
                FrontmatterValue::Scalar(value) => value.as_str(),
                FrontmatterValue::List(_) => return Err(SkillError::InvalidFrontmatter),
            };
            return match raw {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(SkillError::InvalidFrontmatter),
            };
        }
    }
    Ok(default)
}

fn parse_path_list(
    items: Option<&[String]>,
    max: usize,
    overflow: SkillError,
) -> Result<Vec<RepoPath>, SkillError> {
    let Some(items) = items else {
        return Ok(Vec::new());
    };
    if items.len() > max {
        return Err(overflow);
    }
    let mut out = Vec::with_capacity(items.len());
    let mut seen = BTreeSet::new();
    for item in items {
        let path = parse_repo_path(item)?;
        if !seen.insert(path.as_str().to_owned()) {
            return Err(SkillError::InvalidPath);
        }
        out.push(path);
    }
    Ok(out)
}

fn parse_tool_names(items: Option<&[String]>, max: usize) -> Result<Vec<String>, SkillError> {
    let Some(items) = items else {
        return Ok(Vec::new());
    };
    if items.len() > max {
        return Err(SkillError::TooManyTools);
    }
    let mut out = Vec::with_capacity(items.len());
    let mut seen = BTreeSet::new();
    for item in items {
        let name = parse_tool_token(item)?;
        if !seen.insert(name.clone()) {
            return Err(SkillError::InvalidFrontmatter);
        }
        out.push(name);
    }
    Ok(out)
}

fn parse_tool_token(value: &str) -> Result<String, SkillError> {
    if value.is_empty() || value.len() > MAX_IDENT_BYTES {
        return Err(SkillError::InvalidIdent);
    }
    if value.contains('\0') || value.chars().any(char::is_control) {
        return Err(SkillError::InvalidIdent);
    }
    if value.split(['/', '\\']).any(|part| part == "..") {
        return Err(SkillError::InvalidIdent);
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'*' | b'@'))
    {
        return Err(SkillError::InvalidIdent);
    }
    Ok(value.to_owned())
}

fn parse_description(value: &str, limits: &SkillLimits) -> Result<String, SkillError> {
    if value.is_empty() || value.len() > limits.max_description_bytes {
        return Err(SkillError::InvalidDescription);
    }
    if value.contains('\0') || value.chars().any(|ch| ch.is_control() && ch != '\n') {
        return Err(SkillError::InvalidDescription);
    }
    Ok(value.to_owned())
}

fn parse_ident(value: &str) -> Result<String, SkillError> {
    if value.is_empty() || value.len() > MAX_IDENT_BYTES {
        return Err(SkillError::InvalidIdent);
    }
    if value.contains('\0') || value.chars().any(char::is_control) {
        return Err(SkillError::InvalidIdent);
    }
    if value.split(['/', '\\']).any(|part| part == "..") {
        return Err(SkillError::InvalidIdent);
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
    {
        return Err(SkillError::InvalidIdent);
    }
    Ok(value.to_owned())
}

fn parse_repo_path(value: &str) -> Result<RepoPath, SkillError> {
    RepoPath::parse(value).map_err(map_repo_path)
}

fn map_repo_path(err: RepoPathError) -> SkillError {
    let _ = err;
    SkillError::InvalidPath
}

fn estimate_tokens(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let mut units = 0u64;
    for ch in text.chars() {
        units = units.saturating_add(if ch.is_ascii() { 1 } else { 2 });
    }
    let tokens = units.div_ceil(3);
    u32::try_from(tokens).unwrap_or(u32::MAX).max(1)
}

/// Bounds total recursive calls across one `glob_matches` invocation.
///
/// `RepoPath` (both a skill's own `paths` globs and a query's `task_paths`)
/// already caps total bytes at `protocol::MAX_REPO_PATH_BYTES` (4096), but a
/// pattern that is one long run of `*` makes `match_segment_chars` branch
/// twice per character — a plain per-call depth counter still lets the
/// *total* number of calls across all branches blow up combinatorially
/// (backtracking over which of a short segment's few remaining bytes each
/// step consumes), turning the match into a hang long before any single
/// call chain gets deep enough to threaten the stack. A shared call budget,
/// decremented on every call and checked before recursing further, bounds
/// both total work *and* max depth (depth can never exceed calls spent) in
/// one guard. No real glob pattern or path is remotely close to exhausting
/// it in genuine use — real matches resolve in a handful of calls.
const MAX_GLOB_MATCH_CALLS: u32 = 10_000;

fn glob_matches(glob: &str, path: &str) -> bool {
    let glob_parts: Vec<&str> = glob.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();
    let mut budget = MAX_GLOB_MATCH_CALLS;
    match_glob_parts(&glob_parts, &path_parts, &mut budget)
}

fn match_glob_parts(glob: &[&str], path: &[&str], budget: &mut u32) -> bool {
    let Some(remaining) = budget.checked_sub(1) else {
        return false;
    };
    *budget = remaining;
    match (glob.first().copied(), path.first().copied()) {
        (None, None) => true,
        (Some("**"), _) => {
            match_glob_parts(&glob[1..], path, budget)
                || (!path.is_empty() && match_glob_parts(glob, &path[1..], budget))
        }
        (Some(pattern), Some(segment)) if match_segment(pattern, segment, budget) => {
            match_glob_parts(&glob[1..], &path[1..], budget)
        }
        _ => false,
    }
}

fn match_segment(pattern: &str, segment: &str, budget: &mut u32) -> bool {
    match_segment_chars(pattern.as_bytes(), segment.as_bytes(), budget)
}

fn match_segment_chars(pattern: &[u8], segment: &[u8], budget: &mut u32) -> bool {
    let Some(remaining) = budget.checked_sub(1) else {
        return false;
    };
    *budget = remaining;
    match (pattern.first().copied(), segment.first().copied()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            match_segment_chars(&pattern[1..], segment, budget)
                || (!segment.is_empty() && match_segment_chars(pattern, &segment[1..], budget))
        }
        (Some(b'?'), Some(b)) if b != b'/' => {
            match_segment_chars(&pattern[1..], &segment[1..], budget)
        }
        (Some(p), Some(s)) if p == s => match_segment_chars(&pattern[1..], &segment[1..], budget),
        _ => false,
    }
}

impl SkillError {
    pub fn code(self) -> Option<ErrorCode> {
        match self {
            Self::Cancelled | Self::Timeout => None,
            Self::PrivilegeInjection | Self::ProjectUntrusted => {
                Some(ErrorCode::PluginCapabilityDenied)
            }
            Self::TooLarge
            | Self::TooManySkills
            | Self::TooManyResources
            | Self::TooManyPaths
            | Self::TooManyTools
            | Self::InvalidUtf8
            | Self::MissingFrontmatter
            | Self::InvalidFrontmatter
            | Self::UnknownField
            | Self::MissingField
            | Self::InvalidIdent
            | Self::InvalidPath
            | Self::InvalidDescription
            | Self::NotFound
            | Self::NotInvocable
            | Self::NotTriggered
            | Self::TokenBudget
            | Self::Symlink
            | Self::Unreadable
            | Self::EmptySkill => Some(ErrorCode::ConfigInvalid),
        }
    }

    pub fn into_api_error(self, trace_id: TraceId) -> Option<ApiError> {
        let code = self.code()?;
        Some(
            ApiError::new(code, self.as_str(), trace_id)
                .unwrap_or_else(|_| ApiError::from_unknown(trace_id, &self)),
        )
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "skill operation cancelled",
            Self::Timeout => "skill operation exceeded the configured time bound",
            Self::TooLarge => "skill document or resource exceeds the configured bound",
            Self::TooManySkills => "skill count exceeds the configured bound",
            Self::TooManyResources => "skill resource count exceeds the configured bound",
            Self::TooManyPaths => "skill path-trigger count exceeds the configured bound",
            Self::TooManyTools => "skill allowed-tools count exceeds the configured bound",
            Self::InvalidUtf8 => "skill document is not valid UTF-8",
            Self::MissingFrontmatter => "skill document is missing closed frontmatter",
            Self::InvalidFrontmatter => "skill frontmatter is not a closed YAML subset",
            Self::UnknownField => "unknown skill frontmatter field",
            Self::MissingField => "missing required skill frontmatter field",
            Self::InvalidIdent => "skill identifier is invalid",
            Self::InvalidPath => "skill path or resource locator is invalid",
            Self::InvalidDescription => "skill description is invalid",
            Self::PrivilegeInjection => "skill cannot declare capabilities, leases, or tools",
            Self::ProjectUntrusted => "project skill is ignored until project trust is granted",
            Self::NotFound => "skill was not discovered",
            Self::NotInvocable => "skill is not user-invocable",
            Self::NotTriggered => "skill activation triggers were not satisfied",
            Self::TokenBudget => "skill activation exceeds the token budget",
            Self::Symlink => "skill bundle rejected a symbolic link",
            Self::Unreadable => "skill document or resource could not be read",
            Self::EmptySkill => "skill has no instructions or resources",
        }
    }
}

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Error for SkillError {}

impl Serialize for SkillDescriptor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("SkillDescriptor", 6)?;
        state.serialize_field("schema", SKILL_DESCRIPTOR_SCHEMA)?;
        state.serialize_field("schema_version", &SKILL_DESCRIPTOR_SCHEMA_VERSION)?;
        state.serialize_field("id", self.id.as_str())?;
        state.serialize_field("description", &self.description)?;
        state.serialize_field("instructions_path", self.instructions_path.as_str())?;
        state.serialize_field(
            "resources",
            &self
                .resources
                .iter()
                .map(RepoPath::as_str)
                .collect::<Vec<_>>(),
        )?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for SkillDescriptor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        decode_descriptor(&value).map_err(de::Error::custom)
    }
}

fn decode_descriptor(value: &Value) -> Result<SkillDescriptor, SkillError> {
    let object = value.as_object().ok_or(SkillError::InvalidFrontmatter)?;
    for key in object.keys() {
        if !DESCRIPTOR_FIELDS.contains(&key.as_str()) {
            return Err(SkillError::UnknownField);
        }
    }
    match object.get("schema").and_then(Value::as_str) {
        Some(schema) if schema == SKILL_DESCRIPTOR_SCHEMA => {}
        Some(_) => return Err(SkillError::UnknownField),
        None => return Err(SkillError::MissingField),
    }
    match object.get("schema_version") {
        Some(Value::Number(number)) => {
            let version = number
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .ok_or(SkillError::UnknownField)?;
            if version != SKILL_DESCRIPTOR_SCHEMA_VERSION {
                return Err(SkillError::UnknownField);
            }
        }
        Some(_) => return Err(SkillError::InvalidFrontmatter),
        None => return Err(SkillError::MissingField),
    }
    let id = SkillId::parse(require_json_str(object, "id")?)?;
    let description = require_json_str(object, "description")?.to_owned();
    if description.is_empty() || description.len() > DEFAULT_MAX_DESCRIPTION_BYTES {
        return Err(SkillError::InvalidDescription);
    }
    let instructions_path = parse_repo_path(require_json_str(object, "instructions_path")?)?;
    let resources = match object.get("resources") {
        Some(Value::Array(items)) => {
            if items.len() > DEFAULT_MAX_RESOURCES {
                return Err(SkillError::TooManyResources);
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let raw = item.as_str().ok_or(SkillError::InvalidPath)?;
                out.push(parse_repo_path(raw)?);
            }
            out
        }
        Some(_) => return Err(SkillError::InvalidFrontmatter),
        None => return Err(SkillError::MissingField),
    };
    Ok(SkillDescriptor {
        id,
        description,
        instructions_path,
        resources,
    })
}

fn require_json_str<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, SkillError> {
    match object.get(key) {
        Some(Value::String(value)) => Ok(value.as_str()),
        Some(_) => Err(SkillError::InvalidFrontmatter),
        None => Err(SkillError::MissingField),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

    fn temp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "rapidlm-skill-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_skill(root: &Path, rel_dir: &str, name: &str, body: &str) -> PathBuf {
        let mut dir = root.to_path_buf();
        for part in rel_dir.split('/') {
            dir.push(part);
        }
        fs::create_dir_all(&dir).expect("skill dir");
        let path = dir.join(SKILL_FILE_NAME);
        fs::write(&path, body).expect("write skill");
        let _ = name;
        path
    }

    fn valid_skill(name: &str, extra: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: Format Rust sources\npaths:\n  - src/**/*.rs\nuser_invocable: true\nmodel_invocable: true\nresources:\n  - style.md\n{extra}---\nUse rustfmt on changed Rust files.\n"
        )
    }

    fn limits() -> SkillLimits {
        SkillLimits::new()
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn glob_matches_ordinary_patterns_and_bounds_pathological_recursion() {
        assert!(glob_matches("src/**/*.rs", "src/lib/mod.rs"));
        assert!(!glob_matches("src/**/*.rs", "docs/readme.md"));

        // A long run of `*` is redundant but not pathological on its own —
        // it still just means "match anything" and resolves correctly and
        // quickly under the call budget.
        let redundant_stars = "*".repeat(4096);
        assert!(glob_matches(&redundant_stars, "short.rs"));

        // A run of `*` that can *never* match (an impossible trailing
        // literal) is the actually pathological case: naive backtracking
        // without a call budget explores a combinatorial number of ways to
        // interleave "consume a star" against "consume a segment byte"
        // before concluding no match is possible — this used to hang for
        // minutes on a mere 8-byte segment, not just risk the stack.
        // MAX_GLOB_MATCH_CALLS must make it resolve to `false` quickly.
        let unmatchable = format!("{}Z", "*".repeat(4096));
        assert!(!glob_matches(&unmatchable, "short.rs"));
    }

    #[test]
    fn parse_manifest_and_descriptor_golden() {
        let (manifest, instructions) =
            parse_skill(valid_skill("fmt", "").as_bytes(), &limits()).expect("parse");
        assert_eq!(manifest.name().as_str(), "fmt");
        assert_eq!(manifest.description(), "Format Rust sources");
        assert_eq!(manifest.paths()[0].as_str(), "src/**/*.rs");
        assert!(manifest.user_invocable());
        assert!(manifest.model_invocable());
        assert_eq!(manifest.resources()[0].as_str(), "style.md");
        assert_eq!(instructions, "Use rustfmt on changed Rust files.\n");

        let descriptor = SkillDescriptor {
            id: SkillId::parse("fmt").expect("id"),
            description: "Format Rust sources".into(),
            instructions_path: RepoPath::parse(".rapidlm/skills/fmt/SKILL.md").expect("path"),
            resources: vec![RepoPath::parse("style.md").expect("res")],
        };
        let encoded = serde_json::to_string(&descriptor).expect("encode");
        assert_eq!(
            encoded,
            r#"{"schema":"rapidlm.skill_descriptor","schema_version":1,"id":"fmt","description":"Format Rust sources","instructions_path":".rapidlm/skills/fmt/SKILL.md","resources":["style.md"]}"#
        );
        let decoded: SkillDescriptor = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, descriptor);
    }

    #[test]
    fn project_skill_ignored_until_trusted() {
        let project = temp_dir();
        write_skill(
            &project,
            ".rapidlm/skills/fmt",
            "fmt",
            &valid_skill("fmt", ""),
        );
        fs::write(project.join(".rapidlm/skills/fmt/style.md"), "style").expect("resource");

        let roots = SkillRoots::new()
            .project_root(&project)
            .project_trust(ProjectTrust::Untrusted);
        let found = discover(&roots, &limits()).expect("discover untrusted");
        assert!(found.is_empty());

        let loader = SkillLoader::new(roots, limits());
        let err = loader
            .activate(&SkillId::parse("fmt").expect("id"), &ActivationQuery::new())
            .expect_err("untrusted");
        assert_eq!(err, SkillError::ProjectUntrusted);
        assert!(err.to_string().contains("project trust"));
        assert!(!err.to_string().contains("fmt"));

        let trusted = SkillRoots::new()
            .project_root(&project)
            .project_trust(ProjectTrust::Trusted);
        let found = discover(&trusted, &limits()).expect("discover trusted");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].origin(), SkillOrigin::Project);
        assert_eq!(found[0].descriptor().id().as_str(), "fmt");
        cleanup(&project);
    }

    #[test]
    fn privilege_keys_cannot_inject_lease_or_tool() {
        for field in [
            "capability: fs.read\n",
            "lease: abc\n",
            "tools:\n  - shell\n",
            "grant: plugin.invoke\n",
            "requested_caps:\n  - fs.write\n",
            "wasm: plugin.wasm\n",
            "command: ./run.sh\n",
        ] {
            let err = parse_skill(valid_skill("evil", field).as_bytes(), &limits())
                .expect_err("privilege");
            assert_eq!(err, SkillError::PrivilegeInjection);
            assert!(!err.to_string().contains("fs.read"));
            assert!(!err.to_string().contains("plugin.wasm"));
            assert!(!err.to_string().contains("./run.sh"));
        }
    }

    #[test]
    fn allowed_tools_are_descriptive_and_activation_is_prompt_only() {
        let user = temp_dir();
        write_skill(
            &user,
            "skills/fmt",
            "fmt",
            &valid_skill("fmt", "allowed-tools:\n  - fs.read\n"),
        );
        fs::write(user.join("skills/fmt/style.md"), "# style\n").expect("resource");
        fs::write(user.join("skills/fmt/run.sh"), "#!/bin/sh\necho pwn\n").expect("helper");

        let roots = SkillRoots::new().user_root(&user);
        let records = discover(&roots, &limits()).expect("discover");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].descriptive_allowed_tools(),
            &["fs.read".to_owned()]
        );

        let query = ActivationQuery::new()
            .task_paths(["src/lib.rs"])
            .expect("paths");
        let data = activate(&records[0], &limits(), &query).expect("activate");
        assert_eq!(data.descriptor().id().as_str(), "fmt");
        assert!(data.instructions().contains("rustfmt"));
        assert_eq!(data.resources().len(), 1);
        assert_eq!(data.resources()[0].path().as_str(), "style.md");
        assert_eq!(data.resources()[0].text(), "# style\n");
        assert!(data.token_estimate() > 0);
        let encoded = serde_json::to_value(data.descriptor()).expect("json");
        assert!(encoded.get("capability").is_none());
        assert!(encoded.get("lease").is_none());
        assert!(encoded.get("tools").is_none());
        assert!(encoded.get("allowed-tools").is_none());
        cleanup(&user);
    }

    #[test]
    fn empty_paths_do_not_auto_inject() {
        let user = temp_dir();
        write_skill(
            &user,
            "skills/manual",
            "manual",
            "---\nname: manual\ndescription: Manual only\npaths: []\nmodel_invocable: true\nuser_invocable: true\n---\nAsk first.\n",
        );
        let roots = SkillRoots::new().user_root(&user);
        let records = discover(&roots, &limits()).expect("discover");
        let query = ActivationQuery::new()
            .task_paths(["src/lib.rs"])
            .expect("paths");
        assert!(!records[0].matches(&query));
        let err = activate(&records[0], &limits(), &query).expect_err("no auto");
        assert_eq!(err, SkillError::NotTriggered);

        let requested =
            ActivationQuery::new().user_requested(SkillId::parse("manual").expect("id"));
        let data = activate(&records[0], &limits(), &requested).expect("user");
        assert_eq!(data.instructions(), "Ask first.\n");
        cleanup(&user);
    }

    #[test]
    fn user_skill_outranks_project_duplicate() {
        let project = temp_dir();
        let user = temp_dir();
        write_skill(
            &project,
            ".rapidlm/skills/fmt",
            "fmt",
            &valid_skill("fmt", ""),
        );
        write_skill(
            &user,
            "skills/fmt",
            "fmt",
            "---\nname: fmt\ndescription: User formatter\npaths:\n  - src/**/*.rs\n---\nUser instructions.\n",
        );
        let roots = SkillRoots::new()
            .project_root(&project)
            .user_root(&user)
            .project_trust(ProjectTrust::Trusted);
        let found = discover(&roots, &limits()).expect("discover");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].origin(), SkillOrigin::User);
        assert_eq!(found[0].descriptor().description(), "User formatter");
        cleanup(&project);
        cleanup(&user);
    }

    #[test]
    fn resource_traversal_and_symlink_fail_closed() {
        let err = parse_skill(
            b"---\nname: x\ndescription: d\nresources:\n  - ../secret.md\n---\nbody\n",
            &limits(),
        )
        .expect_err("traversal");
        assert_eq!(err, SkillError::InvalidPath);

        let err = parse_skill(
            b"---\nname: x\ndescription: d\nresources:\n  - /etc/passwd\n---\nbody\n",
            &limits(),
        )
        .expect_err("absolute");
        assert_eq!(err, SkillError::InvalidPath);

        let user = temp_dir();
        write_skill(&user, "skills/fmt", "fmt", &valid_skill("fmt", ""));
        let outside = user.join("outside.txt");
        fs::write(&outside, "secret").expect("outside");
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&outside, user.join("skills/fmt/style.md")).expect("symlink");
            let roots = SkillRoots::new().user_root(&user);
            let records = discover(&roots, &limits()).expect("discover");
            let query = ActivationQuery::new().user_requested(SkillId::parse("fmt").expect("id"));
            let err = activate(&records[0], &limits(), &query).expect_err("symlink");
            assert_eq!(err, SkillError::Symlink);
        }
        cleanup(&user);
    }

    #[test]
    fn unknown_field_and_missing_frontmatter_fail_closed() {
        let err = parse_skill(
            b"---\nname: x\ndescription: d\npriority: 1\n---\nbody\n",
            &limits(),
        )
        .expect_err("unknown");
        assert_eq!(err, SkillError::UnknownField);

        let err = parse_skill(b"just instructions\n", &limits()).expect_err("no fm");
        assert_eq!(err, SkillError::MissingFrontmatter);
        assert!(!err.to_string().contains("just instructions"));
    }

    #[test]
    fn cancellation_and_zero_timeout() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = discover(&SkillRoots::new(), &SkillLimits::new().cancel(cancel))
            .expect_err("cancelled");
        assert_eq!(err, SkillError::Cancelled);

        let err = discover(
            &SkillRoots::new(),
            &SkillLimits::new().timeout(Duration::ZERO),
        )
        .expect_err("timeout");
        assert_eq!(err, SkillError::Timeout);
    }

    #[test]
    fn token_budget_blocks_oversized_instructions() {
        let user = temp_dir();
        write_skill(
            &user,
            "skills/big",
            "big",
            "---\nname: big\ndescription: Huge\nuser_invocable: true\n---\nAAAA BBBB CCCC DDDD EEEE\n",
        );
        let roots = SkillRoots::new().user_root(&user);
        let records = discover(&roots, &limits()).expect("discover");
        let tight = SkillLimits::new().token_budget(1);
        let query = ActivationQuery::new().user_requested(SkillId::parse("big").expect("id"));
        let err = activate(&records[0], &tight, &query).expect_err("budget");
        assert_eq!(err, SkillError::TokenBudget);
        cleanup(&user);
    }

    #[test]
    fn display_never_echoes_attacker_controlled_text() {
        let err = parse_skill(
            b"---\nname: leak-secret-token\ndescription: drop /etc/passwd\ncapability: host.admin\n---\npwn\n",
            &limits(),
        )
        .expect_err("priv");
        let text = err.to_string();
        assert!(!text.contains("leak-secret-token"));
        assert!(!text.contains("/etc/passwd"));
        assert!(!text.contains("host.admin"));
        assert_eq!(err.code(), Some(ErrorCode::PluginCapabilityDenied));
    }

    #[cfg(unix)]
    #[test]
    fn intermediate_directory_symlink_cannot_leak_host_file() {
        const CANARY: &str = "T003-CANARY-host-secret-7f3a91c2";
        let user = temp_dir();
        write_skill(
            &user,
            "skills/fmt",
            "fmt",
            "---\nname: fmt\ndescription: Format Rust sources\nuser_invocable: true\nresources:\n  - leak/secret\n---\nUse rustfmt.\n",
        );
        let outside = user.join("outside-secret");
        fs::create_dir_all(&outside).expect("outside dir");
        fs::write(outside.join("secret"), CANARY).expect("host secret");
        std::os::unix::fs::symlink(&outside, user.join("skills/fmt/leak")).expect("dir symlink");

        let roots = SkillRoots::new().user_root(&user);
        let records = discover(&roots, &limits()).expect("discover");
        let query = ActivationQuery::new().user_requested(SkillId::parse("fmt").expect("id"));
        let err = activate(&records[0], &limits(), &query).expect_err("dir symlink");
        assert_eq!(err, SkillError::Symlink);
        assert!(!err.to_string().contains(CANARY));
        assert!(!err.to_string().contains("/etc"));
        cleanup(&user);
    }

    #[cfg(unix)]
    #[test]
    fn leaked_directory_symlink_cannot_read_id_rsa() {
        const CANARY: &str = "T003-CANARY-id-rsa-b81e44d0";
        let user = temp_dir();
        write_skill(
            &user,
            "skills/fmt",
            "fmt",
            "---\nname: fmt\ndescription: Format Rust sources\nuser_invocable: true\nresources:\n  - leaked/id_rsa\n---\nUse rustfmt.\n",
        );
        let outside = user.join("host-keys");
        fs::create_dir_all(&outside).expect("outside dir");
        fs::write(outside.join("id_rsa"), CANARY).expect("id_rsa");
        std::os::unix::fs::symlink(&outside, user.join("skills/fmt/leaked")).expect("dir symlink");

        let roots = SkillRoots::new().user_root(&user);
        let records = discover(&roots, &limits()).expect("discover");
        let query = ActivationQuery::new().user_requested(SkillId::parse("fmt").expect("id"));
        let err = activate(&records[0], &limits(), &query).expect_err("leaked dir");
        assert_eq!(err, SkillError::Symlink);
        assert!(!err.to_string().contains(CANARY));
        assert!(!err.to_string().contains("id_rsa"));
        cleanup(&user);
    }

    #[cfg(unix)]
    #[test]
    fn resource_toctou_symlink_swap_never_leaks_host_bytes() {
        const CANARY: &str = "T003-CANARY-toctou-swap-c4d19e77";
        let user = temp_dir();
        write_skill(&user, "skills/fmt", "fmt", &valid_skill("fmt", ""));
        let bundle = user.join("skills/fmt");
        let style = bundle.join("style.md");
        fs::write(&style, "ok-style\n").expect("style");
        let canary = user.join("host-canary.txt");
        fs::write(&canary, CANARY).expect("canary");

        let rel = RepoPath::parse("style.md").expect("rel");
        let opened = open_confined(&bundle, &rel).expect("open regular");
        fs::remove_file(&style).expect("unlink");
        std::os::unix::fs::symlink(&canary, &style).expect("swap symlink");
        let bytes = read_file_bounded(opened, DEFAULT_MAX_RESOURCE_BYTES).expect("fd read");
        assert_eq!(bytes, b"ok-style\n");
        assert!(!bytes.windows(CANARY.len()).any(|chunk| chunk == CANARY.as_bytes()));

        let err = read_confined(&bundle, &rel, DEFAULT_MAX_RESOURCE_BYTES).expect_err("open after swap");
        assert_eq!(err, SkillError::Symlink);
        assert!(!err.to_string().contains(CANARY));

        let roots = SkillRoots::new().user_root(&user);
        let records = discover(&roots, &limits()).expect("discover");
        let query = ActivationQuery::new().user_requested(SkillId::parse("fmt").expect("id"));
        let err = activate(&records[0], &limits(), &query).expect_err("activate after swap");
        assert_eq!(err, SkillError::Symlink);
        assert!(!err.to_string().contains(CANARY));
        cleanup(&user);
    }

    #[test]
    fn marketplace_update_frontmatter_parses_into_metadata_seam() {
        let user = temp_dir();
        write_skill(
            &user,
            "skills/fmt",
            "fmt",
            &valid_skill("fmt", "update: stable@1.4.2\n"),
        );
        let roots = SkillRoots::new().user_root(&user);
        let records = discover(&roots, &limits()).expect("discover");
        assert_eq!(records.len(), 1);
        let record = &records[0];
        let update = record.marketplace_update().expect("update pointer");
        assert_eq!(update.channel(), "stable");
        assert_eq!(update.version(), "1.4.2");

        let metadata = record.marketplace_metadata(ProjectTrust::Trusted);
        assert_eq!(metadata.source(), SkillOrigin::User);
        assert_eq!(metadata.trust(), ProjectTrust::Trusted);
        assert_eq!(metadata.update(), Some(update));
        cleanup(&user);
    }

    #[test]
    fn update_pointer_rejects_malformed_and_privileged_shapes() {
        for bad in [
            "",
            "no-separator",
            "@1.0.0",
            "stable@",
            "sta ble@1.0.0",
            "stable@1.0.0 with spaces",
        ] {
            assert!(UpdatePointer::parse(bad).is_err(), "{bad:?} must reject");
        }
        let ok = UpdatePointer::parse("beta_2@0.9.0-rc.1").expect("valid");
        assert_eq!(ok.channel(), "beta_2");
        assert_eq!(ok.version(), "0.9.0-rc.1");
    }

    #[test]
    fn skill_without_update_field_has_no_marketplace_update() {
        let user = temp_dir();
        write_skill(&user, "skills/plain", "plain", &valid_skill("plain", ""));
        let roots = SkillRoots::new().user_root(&user);
        let records = discover(&roots, &limits()).expect("discover");
        assert_eq!(records.len(), 1);
        assert!(records[0].marketplace_update().is_none());
        let metadata = records[0].marketplace_metadata(ProjectTrust::Untrusted);
        assert_eq!(metadata.trust(), ProjectTrust::Untrusted);
        assert!(metadata.update().is_none());
        cleanup(&user);
    }
}
