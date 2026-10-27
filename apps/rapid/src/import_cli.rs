//! `rapid import` — migrate MCP servers from another tool's config, by
//! shape (SEAM-06's discovery, SEAMS §2.5's rule: nothing maps a product
//! name to a path).
//!
//! `--list` walks the same configuration roots `mcp install --discover`
//! walks and prints every file whose *shape* declares a servers map
//! (`mcpServers`, `mcp_servers`, `servers`, `context_servers`, members
//! naming a `command` or `url`), by path. `--from <path>` imports that
//! file's servers into `.rapidlm/settings.json` through the ordinary
//! loader: every entry is parsed by `mcp_config::parse_entry` — a server
//! the loader would reject is reported and skipped, never written — and a
//! name the project already defines is left alone. Nothing is named after
//! a product; the file is found by its shape and reported by its path.

use std::path::Path;

use crate::mcp_config::parse_entry;

/// Import outcomes for one source file, reported per server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportOutcome {
    /// Written into `.rapidlm/settings.json`.
    Imported,
    /// The project already defines this name; its own entry stands.
    AlreadyDefined,
    /// The loader would reject this entry; the reason names why, and
    /// nothing was written for it.
    Rejected { reason: String },
}

/// List every shape-declared servers file under the discovery roots.
/// Each line is the path plus the dotted servers-map key and count.
pub fn list(env: &[(String, String)]) -> Vec<String> {
    let roots = crate::mcp_install::discover_roots(env);
    crate::mcp_install::discover(&roots)
        .into_iter()
        .map(|found| {
            format!(
                "{} ({}: {} server(s))",
                found.path.display(),
                found.key,
                found.servers
            )
        })
        .collect()
}

/// Import the servers in `source` into the project's
/// `.rapidlm/settings.json` under `root`. Entries are validated by the
/// ordinary loader (`parse_entry`) — a rejected entry is reported, never
/// written — and names the project already defines are skipped. Returns
/// each server's outcome in file order.
pub fn import(root: &Path, source: &Path) -> Result<Vec<(String, ImportOutcome)>, String> {
    let servers = read_servers(source)?;
    let target = root.join(crate::interactive::PROJECT_SETTINGS_FILES[0]);
    let mut settings: serde_json::Value = match std::fs::read_to_string(&target) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .map_err(|_| format!("{} is not valid JSON", target.display()))?,
        _ => serde_json::json!({}),
    };
    let map = settings
        .as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object", target.display()))?;
    let existing = map
        .entry("mcpServers".to_owned())
        .or_insert_with(|| serde_json::json!({}));
    let existing = existing
        .as_object_mut()
        .ok_or_else(|| "mcpServers is not an object".to_owned())?;

    let mut outcomes = Vec::new();
    for (name, spec) in servers {
        if existing.contains_key(&name) {
            outcomes.push((name.clone(), ImportOutcome::AlreadyDefined));
            continue;
        }
        match parse_entry(&name, &spec) {
            Ok(config) => {
                existing.insert(name.clone(), server_json(&config));
                outcomes.push((name, ImportOutcome::Imported));
            }
            Err(issue) => outcomes.push((
                name,
                ImportOutcome::Rejected {
                    reason: issue.to_string(),
                },
            )),
        }
    }
    if outcomes
        .iter()
        .any(|(_, outcome)| matches!(outcome, ImportOutcome::Imported))
    {
        // Write through the same file discipline `mcp add` uses: nothing
        // partial, nothing unparseable — write the whole document, and a
        // failure to write leaves the file as it was.
        let body = serde_json::to_string_pretty(&settings).map_err(|err| err.to_string())?;
        std::fs::write(&target, format!("{body}\n"))
            .map_err(|err| format!("{} could not be written: {err}", target.display()))?;
    }
    Ok(outcomes)
}

/// The servers map of a shape-declared file, in document order —
/// `mcp_install::servers_of`, the same shape machinery discovery uses. A
/// file whose shape is not a servers map is an error naming it, never an
/// empty import that reads as success; a YAML source is named as
/// list-only today (its reader edits by line, it does not read back).
fn read_servers(source: &Path) -> Result<Vec<(String, serde_json::Value)>, String> {
    crate::mcp_install::servers_of(source).ok_or_else(|| {
        format!(
            "{} declares no servers map this can read (YAML is listed but not read)",
            source.display()
        )
    })
}

/// The settings entry for a validated config — the same fields `mcp add`
/// writes, nothing the loader did not already accept.
fn server_json(config: &crate::exec_tools::McpServerConfig) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "command": config.command,
        "args": config.args,
    });
    if !config.env.is_empty() {
        let env: serde_json::Map<String, serde_json::Value> = config
            .env
            .iter()
            .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
            .collect();
        entry["env"] = serde_json::Value::Object(env);
    }
    if let Some(http) = &config.http {
        entry["type"] = serde_json::json!("http");
        entry["url"] = serde_json::json!(http.url);
        if !http.headers.is_empty() {
            let headers: serde_json::Map<String, serde_json::Value> = http
                .headers
                .iter()
                .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
                .collect();
            entry["headers"] = serde_json::Value::Object(headers);
        }
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rapidlm-import-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".rapidlm")).expect("dir");
        dir
    }

    #[test]
    fn import_writes_validated_servers_and_leaves_existing_names_alone() {
        let root = project("writes");
        let source = root.join("source.json");
        std::fs::write(
            &source,
            r#"{
                "mcpServers": {
                    "native": {"command": "npx", "args": ["-y", "pkg"]},
                    "remote": {"type": "http", "url": "https://api.example.test/mcp"},
                    "not-a-server": {"command": null},
                    "bad": {"url": 3}
                }
            }"#,
        )
        .expect("source");

        // The project already defines `native`: its own entry stands.
        std::fs::write(
            root.join(".rapidlm/settings.json"),
            r#"{"mcpServers": {"native": {"command": "mine"}}}"#,
        )
        .expect("settings");

        let outcomes = import(&root, &source).expect("import");
        // The outcomes are per server (map order). What matters: the
        // project's own `native` stands, the valid remote is imported,
        // and every entry the loader rejects is reported, never written.
        let as_map = |name: &str| {
            outcomes
                .iter()
                .find(|(outcome, _)| outcome == name)
                .map(|(_, outcome)| outcome)
                .unwrap_or_else(|| panic!("{name} missing: {outcomes:?}"))
        };
        assert_eq!(as_map("native"), &ImportOutcome::AlreadyDefined);
        assert_eq!(as_map("remote"), &ImportOutcome::Imported);
        assert!(matches!(
            as_map("not-a-server"),
            ImportOutcome::Rejected { .. }
        ));
        assert!(matches!(as_map("bad"), ImportOutcome::Rejected { .. }));

        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".rapidlm/settings.json")).expect("read"),
        )
        .expect("json");
        assert_eq!(
            written["mcpServers"]["native"]["command"], "mine",
            "the project's own entry stands"
        );
        assert_eq!(
            written["mcpServers"]["remote"]["url"],
            "https://api.example.test/mcp"
        );
        assert!(
            written["mcpServers"].get("bad").is_none()
                && written["mcpServers"].get("not-a-server").is_none(),
            "nothing rejected was written: {}",
            written["mcpServers"]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_source_without_a_servers_map_is_an_error_not_an_empty_import() {
        let root = project("empty");
        let source = root.join("empty.json");
        std::fs::write(&source, r#"{"nothing": "here"}"#).expect("source");
        let err = import(&root, &source).expect_err("no servers map");
        assert!(err.contains("declares no servers map"), "{err}");
        // And nothing was written at all.
        assert!(!root.join(".rapidlm/settings.json").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
