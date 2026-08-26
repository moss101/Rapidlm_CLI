#![forbid(unsafe_code)]

pub mod hooks;
pub mod install;
pub mod manifest;
pub mod skills;
pub mod trust;
pub mod wasm;

pub use hooks::{
    dispatch, parse_hook_spec, prepare_command, redact_event_payload, run_command_hook,
    FailurePolicy, HookCapture, HookDecision, HookDispatchResult, HookDisposition, HookError,
    HookEvent, HookEventInput, HookExecContext, HookId, HookKind, HookManager, HookMatcher,
    HookRecord, HookRequestedCap, HookRunStatus, HookSandboxProfile, HookSpec,
    DEFAULT_HOOK_OUTPUT_BYTES, DEFAULT_HOOK_TIMEOUT, HARD_MAX_HOOK_OUTPUT_BYTES,
    HARD_MAX_HOOK_TIMEOUT, HOOK_COMMAND_FAMILY, HOOK_EVENT_SCHEMA, HOOK_EVENT_SCHEMA_VERSION,
    HOOK_SPEC_SCHEMA, HOOK_SPEC_SCHEMA_VERSION, MAX_HOOK_ARGV, MAX_HOOK_ENV_NAMES,
    MAX_HOOK_PAYLOAD_BYTES, MAX_HOOK_SPECS,
};
pub use install::{
    InstallError, InstallPolicy, InstallReceipt, InstallRequest, InstalledPlugin, PluginInstaller,
    RemoveReceipt, INSTALLED_DIR, MAX_PACKAGE_BYTES, ROLLBACK_DIR, STAGING_DIR, TRUST_CATALOG_FILE,
};

pub use manifest::{
    load_manifest, parse_manifest, validate_for_install, validate_for_load, CompatibilityRange,
    ExtensionRef, ManifestError, ManifestTrustBinding, PluginEntrypoint, PluginId, PluginManifest,
    PluginVersion, PublisherId, RequestedCapability, SemVer, HOST_PLUGIN_API_VERSION,
    MAX_ENTRYPOINT_BYTES, MAX_EXTENSION_REFS, MAX_IDENT_BYTES, MAX_MANIFEST_BYTES,
    MAX_REQUESTED_CAPS, PLUGIN_MANIFEST_SCHEMA, PLUGIN_MANIFEST_SCHEMA_VERSION,
    SUPPORTED_WIT_VERSION,
};
pub use skills::{
    activate, activate_matching, discover, parse_skill, ActivationQuery, ProjectTrust,
    SkillDescriptor, SkillError, SkillId, SkillLimits, SkillLoader, SkillManifest, SkillOrigin,
    SkillPromptData, SkillRecord, SkillResource, SkillRoots, DEFAULT_MAX_ALLOWED_TOOLS,
    DEFAULT_MAX_DESCRIPTION_BYTES, DEFAULT_MAX_DIR_ENTRIES, DEFAULT_MAX_INSTRUCTIONS_BYTES,
    DEFAULT_MAX_PATH_TRIGGERS, DEFAULT_MAX_RESOURCES, DEFAULT_MAX_RESOURCE_BYTES,
    DEFAULT_MAX_SKILLS, DEFAULT_MAX_SKILL_BYTES, DEFAULT_TOKEN_BUDGET, MARKETPLACE_SKILLS_DIR,
    PROJECT_SKILLS_DIR, SKILL_DESCRIPTOR_SCHEMA, SKILL_DESCRIPTOR_SCHEMA_VERSION, SKILL_FILE_NAME,
    USER_SKILLS_DIR,
};
pub use trust::{
    ExtensionIdentity, ExtensionObservation, ExtensionTrustRecord, ExtensionTrustStatus,
    ExtensionTrustStore, HashTrustPolicy, InstallSource, InstallSourceKind, ReviewTimestamp,
    TrustError, TrustGrant, TrustInspectView, TrustScope, TrustedCapability, MAX_GRANTED_CAPS,
    MAX_SOURCE_LOCATOR_BYTES, MAX_TRUST_CATALOG_BYTES, MAX_TRUST_RECORDS, TRUST_SCHEMA,
    TRUST_SCHEMA_VERSION,
};
pub use wasm::{
    instantiate, DenyAllHost, GrantedCapability, HostCapabilityHandler, PluginInstance,
    PluginRequest, PluginResponse, ResourceLimits, WasmError, WasmPluginHost, DEFAULT_CALL_EXPORT,
    DEFAULT_MAX_DURATION, DEFAULT_MAX_FUEL, DEFAULT_MAX_MEMORY_BYTES, DEFAULT_MAX_MODULE_BYTES,
    DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_MAX_STACK_FRAMES, HARD_MAX_DURATION, HARD_MAX_FUEL,
    HARD_MAX_MEMORY_BYTES, HARD_MAX_MODULE_BYTES, HARD_MAX_OUTPUT_BYTES, HARD_MAX_STACK_FRAMES,
    HOST_WIT_MODULE, TOOL_WIT_INTERFACE, WASM_PAGE_BYTES,
};
