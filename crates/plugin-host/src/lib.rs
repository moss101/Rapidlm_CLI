#![forbid(unsafe_code)]

pub mod hooks;
pub mod install;
pub mod manifest;
pub mod skills;
pub mod trust;
pub mod wasm;

pub use hooks::{
    DEFAULT_HOOK_OUTPUT_BYTES, DEFAULT_HOOK_TIMEOUT, FailurePolicy, HARD_MAX_HOOK_OUTPUT_BYTES,
    HARD_MAX_HOOK_TIMEOUT, HOOK_COMMAND_FAMILY, HOOK_EVENT_SCHEMA, HOOK_EVENT_SCHEMA_VERSION,
    HOOK_SPEC_SCHEMA, HOOK_SPEC_SCHEMA_VERSION, HookCapture, HookDecision, HookDispatchResult,
    HookDisposition, HookError, HookEvent, HookEventInput, HookExecContext, HookId, HookKind,
    HookManager, HookMatcher, HookRecord, HookRequestedCap, HookRunStatus, HookSandboxProfile,
    HookSpec, MAX_HOOK_ARGV, MAX_HOOK_ENV_NAMES, MAX_HOOK_PAYLOAD_BYTES, MAX_HOOK_SPECS, dispatch,
    parse_hook_spec, prepare_command, redact_event_payload, run_command_hook,
};
pub use install::{
    INSTALLED_DIR, InstallError, InstallPolicy, InstallReceipt, InstallRequest, InstalledPlugin,
    MAX_PACKAGE_BYTES, PluginInstaller, ROLLBACK_DIR, RemoveReceipt, STAGING_DIR,
    TRUST_CATALOG_FILE,
};

pub use manifest::{
    CompatibilityRange, ExtensionRef, HOST_PLUGIN_API_VERSION, MAX_ENTRYPOINT_BYTES,
    MAX_EXTENSION_REFS, MAX_IDENT_BYTES, MAX_MANIFEST_BYTES, MAX_REQUESTED_CAPS, ManifestError,
    ManifestTrustBinding, PLUGIN_MANIFEST_SCHEMA, PLUGIN_MANIFEST_SCHEMA_VERSION, PluginEntrypoint,
    PluginId, PluginManifest, PluginVersion, PublisherId, RequestedCapability,
    SUPPORTED_WIT_VERSION, SemVer, load_manifest, parse_manifest, validate_for_install,
    validate_for_load,
};
pub use skills::{
    ActivationQuery, DEFAULT_MAX_ALLOWED_TOOLS, DEFAULT_MAX_DESCRIPTION_BYTES,
    DEFAULT_MAX_DIR_ENTRIES, DEFAULT_MAX_INSTRUCTIONS_BYTES, DEFAULT_MAX_PATH_TRIGGERS,
    DEFAULT_MAX_RESOURCE_BYTES, DEFAULT_MAX_RESOURCES, DEFAULT_MAX_SKILL_BYTES, DEFAULT_MAX_SKILLS,
    DEFAULT_TOKEN_BUDGET, MARKETPLACE_SKILLS_DIR, PROJECT_SKILLS_DIR, ProjectTrust,
    SKILL_DESCRIPTOR_SCHEMA, SKILL_DESCRIPTOR_SCHEMA_VERSION, SKILL_FILE_NAME, SkillDescriptor,
    SkillError, SkillId, SkillLimits, SkillLoader, SkillManifest, SkillOrigin, SkillPromptData,
    SkillRecord, SkillResource, SkillRoots, USER_SKILLS_DIR, activate, activate_matching, discover,
    parse_skill,
};
pub use trust::{
    ExtensionIdentity, ExtensionObservation, ExtensionTrustRecord, ExtensionTrustStatus,
    ExtensionTrustStore, HashTrustPolicy, InstallSource, InstallSourceKind, MAX_GRANTED_CAPS,
    MAX_SOURCE_LOCATOR_BYTES, MAX_TRUST_CATALOG_BYTES, MAX_TRUST_RECORDS, ReviewTimestamp,
    TRUST_SCHEMA, TRUST_SCHEMA_VERSION, TrustError, TrustGrant, TrustInspectView, TrustScope,
    TrustedCapability,
};
pub use wasm::{
    DEFAULT_CALL_EXPORT, DEFAULT_MAX_DURATION, DEFAULT_MAX_FUEL, DEFAULT_MAX_MEMORY_BYTES,
    DEFAULT_MAX_MODULE_BYTES, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_MAX_STACK_FRAMES, DenyAllHost,
    GrantedCapability, HARD_MAX_DURATION, HARD_MAX_FUEL, HARD_MAX_MEMORY_BYTES,
    HARD_MAX_MODULE_BYTES, HARD_MAX_OUTPUT_BYTES, HARD_MAX_STACK_FRAMES, HOST_WIT_MODULE,
    HostCapabilityHandler, PluginInstance, PluginRequest, PluginResponse, ResourceLimits,
    TOOL_WIT_INTERFACE, WASM_PAGE_BYTES, WasmError, WasmPluginHost, instantiate,
};
