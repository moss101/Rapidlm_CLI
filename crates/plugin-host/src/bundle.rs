//! Bounded, content-addressed text contributions inside an installed package.
//!
//! A magic prefix distinguishes this format from existing opaque WASM packages.
//! Bundle data is inert; parsing it never loads a skill or grants authority.

use std::collections::BTreeSet;
use std::fmt;

use capability_broker::CancellationToken;
use serde::Deserialize;

use crate::manifest::PluginManifest;
use crate::skills::{SkillId, SkillLimits, parse_skill};

pub const BUNDLE_MAGIC: &[u8] = b"RAPIDLM-PLUGIN-BUNDLE/1\n";
pub const BUNDLE_SCHEMA: &str = "rapidlm.plugin_bundle";
const MAX_SKILLS: usize = 32;
const MAX_RESOURCES: usize = 16;
const MAX_RESOURCE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginBundle {
    schema: String,
    schema_version: u16,
    skills: Vec<BundleSkill>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleSkill {
    id: String,
    skill_md: String,
    resources: Vec<BundleResource>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleResource {
    path: String,
    text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BundleError {
    Cancelled,
    TooLarge,
    InvalidJson,
    UnsupportedSchema,
    InvalidSkill,
    Duplicate,
    Undeclared,
    ResourceMismatch,
    UnsupportedContribution,
}

impl fmt::Display for BundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "plugin bundle validation cancelled",
            Self::TooLarge => "plugin bundle exceeds a size or item limit",
            Self::InvalidJson => "plugin bundle is not valid closed-schema JSON",
            Self::UnsupportedSchema => "plugin bundle schema is unsupported",
            Self::InvalidSkill => "plugin bundle contains an invalid skill",
            Self::Duplicate => "plugin bundle contains duplicate entries",
            Self::Undeclared => "plugin bundle skills do not match the plugin manifest",
            Self::ResourceMismatch => "plugin skill resources do not match its declarations",
            Self::UnsupportedContribution => {
                "plugin bundle declares unsupported hook or MCP content"
            }
        })
    }
}

impl std::error::Error for BundleError {}

impl PluginBundle {
    pub fn skills(&self) -> &[BundleSkill] {
        &self.skills
    }
}
impl BundleSkill {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn skill_md(&self) -> &str {
        &self.skill_md
    }
    pub fn resources(&self) -> &[BundleResource] {
        &self.resources
    }
}
impl BundleResource {
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Opaque legacy packages return `None`; the magic prefix commits to strict
/// validation and malformed bundles cannot silently fall back to opaque bytes.
pub fn parse_bundle(
    bytes: &[u8],
    manifest: &PluginManifest,
    cancel: &CancellationToken,
) -> Result<Option<PluginBundle>, BundleError> {
    cancel.check().map_err(|_| BundleError::Cancelled)?;
    let Some(json) = bytes.strip_prefix(BUNDLE_MAGIC) else {
        return if bytes.starts_with(b"RAPIDLM-PLUGIN-BUNDLE/") {
            Err(BundleError::UnsupportedSchema)
        } else {
            Ok(None)
        };
    };
    if bytes.len() > crate::install::MAX_PACKAGE_BYTES {
        return Err(BundleError::TooLarge);
    }
    let bundle: PluginBundle =
        serde_json::from_slice(json).map_err(|_| BundleError::InvalidJson)?;
    if bundle.schema != BUNDLE_SCHEMA || bundle.schema_version != 1 {
        return Err(BundleError::UnsupportedSchema);
    }
    if bundle.skills.len() > MAX_SKILLS {
        return Err(BundleError::TooLarge);
    }
    if !manifest.hooks().is_empty() || !manifest.mcp_servers().is_empty() {
        return Err(BundleError::UnsupportedContribution);
    }
    let declared: BTreeSet<_> = manifest
        .skills()
        .iter()
        .map(|skill| skill.as_str())
        .collect();
    let mut seen = BTreeSet::new();
    for skill in &bundle.skills {
        cancel.check().map_err(|_| BundleError::Cancelled)?;
        let id = SkillId::parse(&skill.id).map_err(|_| BundleError::InvalidSkill)?;
        if !seen.insert(skill.id.as_str()) {
            return Err(BundleError::Duplicate);
        }
        if !declared.contains(skill.id.as_str()) {
            return Err(BundleError::Undeclared);
        }
        let (parsed, _) = parse_skill(skill.skill_md.as_bytes(), &SkillLimits::new())
            .map_err(|_| BundleError::InvalidSkill)?;
        if parsed.name() != &id {
            return Err(BundleError::InvalidSkill);
        }
        if skill.resources.len() > MAX_RESOURCES {
            return Err(BundleError::TooLarge);
        }
        let expected: BTreeSet<_> = parsed
            .resources()
            .iter()
            .map(|path| path.as_str())
            .collect();
        let mut resources = BTreeSet::new();
        for resource in &skill.resources {
            cancel.check().map_err(|_| BundleError::Cancelled)?;
            let path = protocol::RepoPath::parse(&resource.path)
                .map_err(|_| BundleError::ResourceMismatch)?;
            if resource.text.len() > MAX_RESOURCE_BYTES {
                return Err(BundleError::TooLarge);
            }
            if !resources.insert(resource.path.as_str()) {
                return Err(BundleError::Duplicate);
            }
            if !expected.contains(path.as_str()) {
                return Err(BundleError::ResourceMismatch);
            }
        }
        if resources != expected {
            return Err(BundleError::ResourceMismatch);
        }
    }
    if seen != declared {
        return Err(BundleError::Undeclared);
    }
    Ok(Some(bundle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn manifest(skills: &[&str]) -> PluginManifest {
        let value = json!({"schema":"rapidlm.plugin_manifest","schema_version":1,"id":"acme.fmt","version":"1.0.0","publisher":"acme","digest":protocol::ArtifactId::from_bytes(b"package").to_string(),"entrypoint":"plugin.wasm","wit_version":"1.0.0","compatibility":{"min":"1.0.0","max":"2.0.0"},"requested_caps":[],"skills":skills});
        crate::parse_manifest(
            &serde_json::to_vec(&value).unwrap(),
            &CancellationToken::new(),
        )
        .unwrap()
    }
    fn encoded(value: &Value) -> Vec<u8> {
        let mut bytes = BUNDLE_MAGIC.to_vec();
        bytes.extend(serde_json::to_vec(value).unwrap());
        bytes
    }
    fn valid() -> Value {
        json!({"schema":BUNDLE_SCHEMA,"schema_version":1,"skills":[{"id":"fmt","skill_md":"---\nname: fmt\ndescription: Format sources\nresources:\n  - guide.md\n---\nFormat safely.\n","resources":[{"path":"guide.md","text":"Prefer the project formatter."}]}]})
    }

    #[test]
    fn bundle_round_trips_declared_skills_and_resources() {
        let value = valid();
        let parsed = parse_bundle(
            &encoded(&value),
            &manifest(&["fmt"]),
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.skills()[0].id(), "fmt");
        assert_eq!(
            parsed.skills()[0].resources()[0].text(),
            "Prefer the project formatter."
        );
        assert_eq!(
            parse_bundle(
                b"opaque wasm",
                &manifest(&["fmt"]),
                &CancellationToken::new()
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn bundle_rejects_undeclared_duplicate_and_unconfined_contributions() {
        let manifest = manifest(&["fmt"]);
        let cancel = CancellationToken::new();
        let mut value = valid();
        let duplicate = value["skills"][0].clone();
        value["skills"].as_array_mut().unwrap().push(duplicate);
        assert_eq!(
            parse_bundle(&encoded(&value), &manifest, &cancel),
            Err(BundleError::Duplicate)
        );
        let mut value = valid();
        value["skills"][0]["id"] = json!("other");
        assert_eq!(
            parse_bundle(&encoded(&value), &manifest, &cancel),
            Err(BundleError::Undeclared)
        );
        let mut value = valid();
        value["skills"][0]["resources"][0]["path"] = json!("../outside");
        assert_eq!(
            parse_bundle(&encoded(&value), &manifest, &cancel),
            Err(BundleError::ResourceMismatch)
        );
        let mut value = valid();
        value["skills"][0]["resources"] = json!([]);
        assert_eq!(
            parse_bundle(&encoded(&value), &manifest, &cancel),
            Err(BundleError::ResourceMismatch)
        );
        let mut value = valid();
        value["skills"][0]["skill_md"] =
            json!("---\nname: fmt\ndescription: x\ncapabilities: [all]\n---\nbody\n");
        assert_eq!(
            parse_bundle(&encoded(&value), &manifest, &cancel),
            Err(BundleError::InvalidSkill)
        );
    }

    #[test]
    fn bundle_magic_rejects_malformed_unknown_and_oversized_contents() {
        let manifest = manifest(&["fmt"]);
        let cancel = CancellationToken::new();
        let mut malformed = BUNDLE_MAGIC.to_vec();
        malformed.extend(b"{");
        assert_eq!(
            parse_bundle(&malformed, &manifest, &cancel),
            Err(BundleError::InvalidJson)
        );
        assert_eq!(
            parse_bundle(b"RAPIDLM-PLUGIN-BUNDLE/2\n{}", &manifest, &cancel),
            Err(BundleError::UnsupportedSchema)
        );
        let mut value = valid();
        value["unexpected"] = json!(true);
        assert_eq!(
            parse_bundle(&encoded(&value), &manifest, &cancel),
            Err(BundleError::InvalidJson)
        );
        let mut value = valid();
        value["schema_version"] = json!(2);
        assert_eq!(
            parse_bundle(&encoded(&value), &manifest, &cancel),
            Err(BundleError::UnsupportedSchema)
        );
        let mut value = valid();
        value["skills"][0]["resources"][0]["text"] = json!("x".repeat(MAX_RESOURCE_BYTES + 1));
        assert_eq!(
            parse_bundle(&encoded(&value), &manifest, &cancel),
            Err(BundleError::TooLarge)
        );
        cancel.cancel();
        assert_eq!(
            parse_bundle(&encoded(&valid()), &manifest, &cancel),
            Err(BundleError::Cancelled)
        );
    }

    #[test]
    fn unsupported_contribution_types_are_not_silently_accepted() {
        let raw = json!({"schema":"rapidlm.plugin_manifest","schema_version":1,"id":"acme.fmt","version":"1.0.0","publisher":"acme","digest":protocol::ArtifactId::from_bytes(b"package").to_string(),"entrypoint":"plugin.wasm","wit_version":"1.0.0","compatibility":{"min":"1.0.0","max":"2.0.0"},"requested_caps":[],"skills":["fmt"],"hooks":["before"]});
        let manifest = crate::parse_manifest(
            &serde_json::to_vec(&raw).unwrap(),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(
            parse_bundle(&encoded(&valid()), &manifest, &CancellationToken::new()),
            Err(BundleError::UnsupportedContribution)
        );
    }
}
