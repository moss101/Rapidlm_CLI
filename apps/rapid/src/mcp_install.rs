//! `rapid mcp install`: put one of this project's MCP servers into another
//! program's configuration file, and find such files (SEAM-06 AC-04/05).
//!
//! Edits are format-preserving: the one entry is injected or replaced and
//! every other byte — comments, ordering, unrelated keys — is left as it
//! was. JSONC and YAML have no editor in the workspace, so each has a small
//! one here, deliberately narrow (a JSONC object path; a YAML block-mapping
//! path) and refusing, never guessing, on anything else. TOML goes through
//! `toml_edit`.
//!
//! Discovery is by shape, never by name (rule 2.5): a file is a candidate
//! when it declares a servers map under one of [`SERVER_KEYS`], wherever it
//! lives in the configuration roots and whatever it is called.

use std::path::{Path, PathBuf};

use crate::exec_tools::McpServerConfig;

/// The key names a servers map is declared under.
pub const SERVER_KEYS: &[&str] = &["mcpServers", "mcp_servers", "servers", "context_servers"];

/// The environment variable naming the configuration roots `--discover`
/// scans (a path list), in place of the home and XDG roots.
pub const CONFIG_ROOTS_ENV: &str = "RAPIDLM_CONFIG_ROOTS";

/// A file larger than this is neither edited nor inspected.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// How deep below a root `--discover` looks, and how many entries it visits
/// in all before it stops.
const DISCOVER_DEPTH: usize = 3;
const DISCOVER_MAX_ENTRIES: usize = 20_000;
const DISCOVER_MAX_FILE_BYTES: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Jsonc,
    Toml,
    Yaml,
}

impl Format {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "jsonc" | "json" => Some(Self::Jsonc),
            "toml" => Some(Self::Toml),
            "yaml" | "yml" => Some(Self::Yaml),
            _ => None,
        }
    }

    /// The format a file's extension names.
    pub fn of_path(path: &Path) -> Option<Self> {
        Self::parse(&path.extension()?.to_str()?.to_ascii_lowercase())
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jsonc => "jsonc",
            Self::Toml => "toml",
            Self::Yaml => "yaml",
        }
    }

    /// Where a servers map goes when `--key` names none.
    pub fn default_key(self) -> &'static str {
        match self {
            Self::Toml => "mcp_servers",
            Self::Jsonc | Self::Yaml => "mcpServers",
        }
    }
}

/// What an edit came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Edit {
    /// The entry is already there, as it would be written.
    Unchanged,
    Changed(String),
}

/// The entry a server is written as: `command`/`args`/`env`, or
/// `url`/`headers`.
pub fn entry_value(server: &McpServerConfig) -> serde_json::Value {
    let mut entry = serde_json::Map::new();
    match &server.http {
        Some(http) => {
            entry.insert("url".to_owned(), http.url.clone().into());
            if !http.headers.is_empty() {
                entry.insert("headers".to_owned(), pairs(&http.headers));
            }
        }
        None => {
            entry.insert("command".to_owned(), server.command.clone().into());
            if !server.args.is_empty() {
                entry.insert("args".to_owned(), server.args.clone().into());
            }
            if !server.env.is_empty() {
                entry.insert("env".to_owned(), pairs(&server.env));
            }
        }
    }
    serde_json::Value::Object(entry)
}

fn pairs(pairs: &[(String, String)]) -> serde_json::Value {
    serde_json::Value::Object(
        pairs
            .iter()
            .map(|(key, value)| (key.clone(), value.clone().into()))
            .collect(),
    )
}

/// Put `name: entry` under the servers map at `key_path` in `text`.
pub fn edit(
    text: &str,
    format: Format,
    key_path: &[String],
    name: &str,
    entry: &serde_json::Value,
) -> Result<Edit, String> {
    if key_path.is_empty() || key_path.iter().any(String::is_empty) {
        return Err("the servers key path is empty".to_owned());
    }
    match format {
        Format::Jsonc => jsonc::edit(text, key_path, name, entry),
        Format::Toml => toml_format::edit(text, key_path, name, entry),
        Format::Yaml => yaml::edit(text, key_path, name, entry),
    }
}

/// A line diff of one contiguous change: the lines before and after it are
/// shared.
pub fn diff(path: &Path, old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let prefix = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = old_lines[prefix..]
        .iter()
        .rev()
        .zip(new_lines[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = format!(
        "--- {0}\n+++ {0}\n@@ line {1} @@\n",
        path.display(),
        prefix + 1
    );
    for line in &old_lines[prefix..old_lines.len() - suffix] {
        out.push_str(&format!("-{line}\n"));
    }
    for line in &new_lines[prefix..new_lines.len() - suffix] {
        out.push_str(&format!("+{line}\n"));
    }
    out
}

/// A per-user lock on installing: held while it lives, refused while
/// another holder has it.
pub struct InstallLock {
    path: PathBuf,
}

impl InstallLock {
    pub fn acquire(home: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(home)
            .map_err(|err| format!("{} could not be created: {err}", home.display()))?;
        let path = home.join("mcp-install.lock");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                let _ = std::io::Write::write_all(
                    &mut file,
                    format!("{}\n", std::process::id()).as_bytes(),
                );
                Ok(Self { path })
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Err(format!(
                "another `rapid mcp install` holds the lock {}; if none is running, remove it",
                path.display()
            )),
            Err(err) => Err(format!("{}: {err}", path.display())),
        }
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Copy `target` to a timestamped `.bak` beside it; the backup's path.
pub fn backup(target: &Path) -> Result<PathBuf, String> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    for attempt in 0..100u32 {
        let suffix = if attempt == 0 {
            format!("{secs}.bak")
        } else {
            format!("{secs}-{attempt}.bak")
        };
        let bak = target.with_file_name(format!("{name}.{suffix}"));
        let bytes = std::fs::read(target).map_err(|err| format!("{}: {err}", target.display()))?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&bak)
        {
            Ok(mut file) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
                }
                std::io::Write::write_all(&mut file, &bytes)
                    .map_err(|err| format!("{}: {err}", bak.display()))?;
                return Ok(bak);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format!("{}: {err}", bak.display())),
        }
    }
    Err(format!("no free backup name beside {}", target.display()))
}

/// One file `--discover` found.
#[derive(Debug, PartialEq, Eq)]
pub struct Discovered {
    pub path: PathBuf,
    pub format: Format,
    /// The servers map's key path, dotted.
    pub key: String,
    pub servers: usize,
}

/// The roots `--discover` scans: [`CONFIG_ROOTS_ENV`] when set, else the
/// home directory and the XDG configuration directory.
pub fn discover_roots(env: &[(String, String)]) -> Vec<PathBuf> {
    let var = |name: &str| {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .filter(|value| !value.is_empty())
    };
    if let Some(roots) = var(CONFIG_ROOTS_ENV) {
        return std::env::split_paths(&roots).collect();
    }
    let mut roots = Vec::new();
    if let Some(home) = var("HOME").or_else(|| var("USERPROFILE")) {
        roots.push(PathBuf::from(&home));
        match var("XDG_CONFIG_HOME") {
            Some(xdg) => roots.push(PathBuf::from(xdg)),
            None => roots.push(PathBuf::from(home).join(".config")),
        }
    } else if let Some(xdg) = var("XDG_CONFIG_HOME") {
        roots.push(PathBuf::from(xdg));
    }
    if let Some(appdata) = var("APPDATA") {
        roots.push(PathBuf::from(appdata));
    }
    roots.dedup();
    roots
}

/// Every file under `roots` whose shape declares a servers map, sorted by
/// path. Bounded: [`DISCOVER_DEPTH`] levels, [`DISCOVER_MAX_ENTRIES`]
/// entries, small files only; symlinked directories are not followed.
pub fn discover(roots: &[PathBuf]) -> Vec<Discovered> {
    let mut found = Vec::new();
    let mut visited = 0usize;
    let mut seen = std::collections::BTreeSet::new();
    for root in roots {
        walk(root, 0, &mut visited, &mut |path| {
            if seen.insert(path.to_path_buf())
                && let Some(entry) = inspect(path)
            {
                found.push(entry);
            }
        });
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

fn walk(dir: &Path, depth: usize, visited: &mut usize, visit: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        *visited += 1;
        if *visited > DISCOVER_MAX_ENTRIES {
            return;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            let name = entry.file_name();
            let skip = matches!(
                name.to_str(),
                Some(".git" | "node_modules" | "target" | ".cache" | "Library" | ".Trash")
            );
            if depth < DISCOVER_DEPTH && !skip {
                walk(&path, depth + 1, visited, visit);
            }
        } else if kind.is_file() {
            visit(&path);
        }
    }
}

/// A file's servers map, if its shape declares one.
fn inspect(path: &Path) -> Option<Discovered> {
    let format = Format::of_path(path)?;
    let size = std::fs::metadata(path).ok()?.len();
    if size > DISCOVER_MAX_FILE_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let (key, servers) = match format {
        Format::Jsonc => json_shape(&serde_json::from_str(&jsonc::strip(&text)).ok()?)?,
        Format::Toml => {
            let value: toml::Value = toml::from_str(&text).ok()?;
            json_shape(&serde_json::to_value(value).ok()?)?
        }
        Format::Yaml => yaml::shape(&text)?,
    };
    Some(Discovered {
        path: path.to_path_buf(),
        format,
        key,
        servers,
    })
}

/// A servers map at the top level or one level down: an object under a
/// known key whose members are objects naming a `command` or a `url`.
fn json_shape(value: &serde_json::Value) -> Option<(String, usize)> {
    let is_servers = |value: &serde_json::Value| {
        let map = value.as_object()?;
        let servers = map
            .values()
            .filter(|server| {
                server.as_object().is_some_and(|server| {
                    server.contains_key("command") || server.contains_key("url")
                })
            })
            .count();
        (servers > 0).then_some(servers)
    };
    let top = value.as_object()?;
    for key in SERVER_KEYS {
        if let Some(servers) = top.get(*key).and_then(is_servers) {
            return Some(((*key).to_owned(), servers));
        }
    }
    for (parent, child) in top {
        let Some(child) = child.as_object() else {
            continue;
        };
        for key in SERVER_KEYS {
            if let Some(servers) = child.get(*key).and_then(is_servers) {
                return Some((format!("{parent}.{key}"), servers));
            }
        }
    }
    None
}

/// Pretty JSON for `value`, every line after the first indented by
/// `indent`.
fn pretty(value: &serde_json::Value, indent: &str) -> String {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_owned());
    text.lines()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                line.to_owned()
            } else {
                format!("{indent}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `value` nested under the rest of a key path.
fn nest(rest: &[String], name: &str, entry: &serde_json::Value) -> serde_json::Value {
    let mut value = serde_json::json!({ name: entry });
    for key in rest.iter().rev() {
        value = serde_json::json!({ key: value });
    }
    value
}

mod jsonc {
    use super::{Edit, nest, pretty};

    /// `text` without its comments (strings untouched).
    pub fn strip(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    let end = string_end(bytes, i).unwrap_or(bytes.len());
                    out.extend_from_slice(&bytes[i..end]);
                    i = end;
                }
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    i = find(bytes, i + 2, b"*/").map_or(bytes.len(), |end| end + 2);
                    out.push(b' ');
                }
                byte => {
                    out.push(byte);
                    i += 1;
                }
            }
        }
        String::from_utf8(out).unwrap_or_default()
    }

    fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
        bytes
            .get(from..)?
            .windows(needle.len())
            .position(|window| window == needle)
            .map(|at| from + at)
    }

    /// Past whitespace and comments.
    fn skip(bytes: &[u8], mut i: usize) -> usize {
        loop {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if bytes[i..].starts_with(b"//") {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            } else if bytes[i..].starts_with(b"/*") {
                i = find(bytes, i + 2, b"*/").map_or(bytes.len(), |end| end + 2);
            } else {
                return i;
            }
        }
    }

    /// Past the string starting at `i` (its opening quote).
    fn string_end(bytes: &[u8], i: usize) -> Result<usize, String> {
        let mut j = i + 1;
        while j < bytes.len() {
            match bytes[j] {
                b'\\' => j += 2,
                b'"' => return Ok(j + 1),
                _ => j += 1,
            }
        }
        Err("an unterminated string".to_owned())
    }

    /// Past the value starting at `i`.
    fn value_end(bytes: &[u8], i: usize) -> Result<usize, String> {
        match bytes.get(i) {
            Some(b'"') => string_end(bytes, i),
            Some(b'{' | b'[') => {
                let mut depth = 0usize;
                let mut j = i;
                while j < bytes.len() {
                    match bytes[j] {
                        b'"' => {
                            j = string_end(bytes, j)?;
                            continue;
                        }
                        b'/' if matches!(bytes.get(j + 1), Some(b'/' | b'*')) => {
                            j = skip(bytes, j);
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                return Ok(j + 1);
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                Err("an unterminated object or array".to_owned())
            }
            Some(_) => {
                let mut j = i;
                while j < bytes.len()
                    && !matches!(bytes[j], b',' | b'}' | b']' | b'/')
                    && !bytes[j].is_ascii_whitespace()
                {
                    j += 1;
                }
                if j == i {
                    Err("a missing value".to_owned())
                } else {
                    Ok(j)
                }
            }
            None => Err("a missing value".to_owned()),
        }
    }

    struct Member {
        key: String,
        key_start: usize,
        value_start: usize,
        value_end: usize,
    }

    /// The members of the object opening at `open`, and its closing brace.
    fn members(text: &str, open: usize) -> Result<(Vec<Member>, usize), String> {
        let bytes = text.as_bytes();
        let mut members = Vec::new();
        let mut i = skip(bytes, open + 1);
        loop {
            match bytes.get(i) {
                Some(b'}') => return Ok((members, i)),
                Some(b'"') => {}
                _ => return Err("an object member that is not a quoted key".to_owned()),
            }
            let key_end = string_end(bytes, i)?;
            let key: String = serde_json::from_str(&text[i..key_end])
                .map_err(|_| "an unreadable object key".to_owned())?;
            let colon = skip(bytes, key_end);
            if bytes.get(colon) != Some(&b':') {
                return Err(format!("no ':' after the key {key:?}"));
            }
            let value_start = skip(bytes, colon + 1);
            let end = value_end(bytes, value_start)?;
            members.push(Member {
                key,
                key_start: i,
                value_start,
                value_end: end,
            });
            let next = skip(bytes, end);
            match bytes.get(next) {
                Some(b',') => i = skip(bytes, next + 1),
                Some(b'}') => return Ok((members, next)),
                _ => return Err("an object member not followed by ',' or '}'".to_owned()),
            }
        }
    }

    /// The whitespace a line starts with.
    fn line_indent(text: &str, at: usize) -> &str {
        let start = text[..at].rfind('\n').map_or(0, |newline| newline + 1);
        let line = &text[start..];
        &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
    }

    /// The indentation unit a document uses: its first indented line's.
    fn indent_unit(text: &str) -> String {
        text.lines()
            .map(|line| &line[..line.len() - line.trim_start_matches([' ', '\t']).len()])
            .find(|indent| !indent.is_empty())
            .unwrap_or("  ")
            .to_owned()
    }

    pub fn edit(
        text: &str,
        key_path: &[String],
        name: &str,
        entry: &serde_json::Value,
    ) -> Result<Edit, String> {
        let source = if text.trim().is_empty() { "{}\n" } else { text };
        let bytes = source.as_bytes();
        let mut open = skip(bytes, 0);
        if bytes.get(open) != Some(&b'{') {
            return Err("the document is not a JSON object".to_owned());
        }
        let unit = indent_unit(source);
        let mut path = key_path;
        let mut current = name;
        let mut value = entry.clone();
        // Walk the path; where it stops, the rest is inserted nested.
        loop {
            let (members, close) = members(source, open).map_err(|err| format!("JSONC: {err}"))?;
            let Some((segment, rest)) = path.split_first() else {
                break;
            };
            match members.iter().find(|member| member.key == *segment) {
                Some(member) if bytes.get(member.value_start) == Some(&b'{') => {
                    open = member.value_start;
                    path = rest;
                }
                Some(_) => return Err(format!("'{segment}' is not an object")),
                None => {
                    value = nest(rest, name, entry);
                    current = segment;
                    return Ok(Edit::Changed(insert(
                        source, &unit, open, close, &members, current, &value,
                    )));
                }
            }
        }
        let (members, close) = members(source, open).map_err(|err| format!("JSONC: {err}"))?;
        if let Some(member) = members.iter().find(|member| member.key == current) {
            let existing: Option<serde_json::Value> =
                serde_json::from_str(&strip(&source[member.value_start..member.value_end])).ok();
            if existing.as_ref() == Some(&value) {
                return Ok(Edit::Unchanged);
            }
            let indent = line_indent(source, member.key_start);
            let mut out = String::with_capacity(source.len() + 256);
            out.push_str(&source[..member.value_start]);
            out.push_str(&pretty(&value, indent));
            out.push_str(&source[member.value_end..]);
            return Ok(Edit::Changed(out));
        }
        Ok(Edit::Changed(insert(
            source, &unit, open, close, &members, current, &value,
        )))
    }

    /// `"key": value` added as the last member of the object at
    /// `open..=close`.
    fn insert(
        text: &str,
        unit: &str,
        open: usize,
        close: usize,
        members: &[Member],
        key: &str,
        value: &serde_json::Value,
    ) -> String {
        let key = serde_json::to_string(key).unwrap_or_default();
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len() + 256);
        match members.last() {
            Some(last) => {
                let indent = line_indent(text, members[0].key_start).to_owned();
                let member = format!("{key}: {}", pretty(value, &indent));
                let after = skip(bytes, last.value_end);
                if bytes.get(after) == Some(&b',') {
                    // A trailing comma already there: keep the style.
                    out.push_str(&text[..after + 1]);
                    out.push_str(&format!("\n{indent}{member},"));
                    out.push_str(&text[after + 1..]);
                    return out;
                }
                let line_end = text[last.value_end..]
                    .find('\n')
                    .map_or(text.len(), |at| last.value_end + at);
                let rest = text[last.value_end..line_end].trim();
                out.push_str(&text[..last.value_end]);
                out.push(',');
                if (rest.is_empty() || rest.starts_with("//")) && line_end < close {
                    out.push_str(&text[last.value_end..line_end]);
                    out.push_str(&format!("\n{indent}{member}"));
                    out.push_str(&text[line_end..]);
                } else {
                    out.push_str(&format!("\n{indent}{member}"));
                    out.push_str(&text[last.value_end..]);
                }
            }
            None => {
                let outer = line_indent(text, open).to_owned();
                let indent = format!("{outer}{unit}");
                let member = format!("{key}: {}", pretty(value, &indent));
                let inner = &text[open + 1..close];
                out.push_str(&text[..open + 1]);
                if inner.trim().is_empty() {
                    out.push_str(&format!("\n{indent}{member}\n{outer}"));
                } else {
                    // Only comments inside: they stay, the member follows.
                    out.push_str(inner.trim_end());
                    out.push_str(&format!("\n{indent}{member}\n{outer}"));
                }
                out.push_str(&text[close..]);
            }
        }
        out
    }
}

mod toml_format {
    use super::Edit;

    fn to_item(value: &serde_json::Value) -> toml_edit::Value {
        match value {
            serde_json::Value::String(text) => text.as_str().into(),
            serde_json::Value::Array(items) => {
                let mut array = toml_edit::Array::new();
                for item in items {
                    array.push(to_item(item));
                }
                toml_edit::Value::Array(array)
            }
            serde_json::Value::Object(map) => {
                let mut table = toml_edit::InlineTable::new();
                for (key, value) in map {
                    table.insert(key, to_item(value));
                }
                toml_edit::Value::InlineTable(table)
            }
            serde_json::Value::Bool(flag) => (*flag).into(),
            serde_json::Value::Number(number) => number
                .as_i64()
                .map_or_else(|| number.to_string().into(), Into::into),
            serde_json::Value::Null => "".into(),
        }
    }

    pub fn edit(
        text: &str,
        key_path: &[String],
        name: &str,
        entry: &serde_json::Value,
    ) -> Result<Edit, String> {
        let current: toml::Value = toml::from_str(text).map_err(|err| format!("TOML: {err}"))?;
        let existing = key_path
            .iter()
            .try_fold(&current, |value, key| value.get(key))
            .and_then(|servers| servers.get(name))
            .and_then(|value| serde_json::to_value(value).ok());
        if existing.as_ref() == Some(entry) {
            return Ok(Edit::Unchanged);
        }
        let mut document: toml_edit::DocumentMut =
            text.parse().map_err(|err| format!("TOML: {err}"))?;
        let mut table = document.as_table_mut();
        for key in key_path {
            if !table.contains_key(key) {
                let mut child = toml_edit::Table::new();
                child.set_implicit(true);
                table.insert(key, toml_edit::Item::Table(child));
            }
            table = table
                .get_mut(key)
                .and_then(toml_edit::Item::as_table_mut)
                .ok_or_else(|| format!("'{key}' is not a table"))?;
        }
        let mut server = toml_edit::Table::new();
        if let Some(fields) = entry.as_object() {
            for (key, value) in fields {
                server.insert(key, toml_edit::Item::Value(to_item(value)));
            }
        }
        table.insert(name, toml_edit::Item::Table(server));
        Ok(Edit::Changed(document.to_string()))
    }
}

mod yaml {
    use super::{Edit, SERVER_KEYS};

    struct Line<'a> {
        text: &'a str,
        indent: usize,
        /// Blank or a comment only.
        blank: bool,
    }

    fn lines(text: &str) -> Vec<Line<'_>> {
        text.split_inclusive('\n')
            .map(|text| {
                let body = text.trim_end_matches(['\n', '\r']);
                let trimmed = body.trim_start_matches(' ');
                Line {
                    text,
                    indent: body.len() - trimmed.len(),
                    blank: trimmed.is_empty() || trimmed.starts_with('#'),
                }
            })
            .collect()
    }

    /// A mapping line's key and what follows its colon.
    fn key_of(line: &str) -> Option<(String, &str)> {
        let body = line.trim_end_matches(['\n', '\r']).trim_start_matches(' ');
        if body.starts_with('-') {
            return None;
        }
        let (key, rest) = if let Some(quoted) = body.strip_prefix('"') {
            let end = quoted.find('"')?;
            (
                quoted[..end].to_owned(),
                quoted[end + 1..].strip_prefix(':')?,
            )
        } else if let Some(quoted) = body.strip_prefix('\'') {
            let end = quoted.find('\'')?;
            (
                quoted[..end].to_owned(),
                quoted[end + 1..].strip_prefix(':')?,
            )
        } else {
            let colon = body
                .char_indices()
                .find(|(at, c)| {
                    *c == ':'
                        && body[at + 1..]
                            .chars()
                            .next()
                            .is_none_or(|next| next == ' ' || next == '\t')
                })?
                .0;
            (body[..colon].trim_end().to_owned(), &body[colon + 1..])
        };
        Some((key, rest))
    }

    /// Whether what follows a key's colon opens a nested block (nothing, or
    /// only a comment).
    fn opens_block(rest: &str) -> bool {
        let rest = rest.trim();
        rest.is_empty() || rest.starts_with('#')
    }

    /// The line after the block belonging to the key on line `at`, trailing
    /// blank lines left outside.
    fn block_end(lines: &[Line<'_>], at: usize, end: usize) -> usize {
        let mut next = at + 1;
        while next < end && (lines[next].blank || lines[next].indent > lines[at].indent) {
            next += 1;
        }
        while next > at + 1 && lines[next - 1].blank {
            next -= 1;
        }
        next
    }

    fn find(
        lines: &[Line<'_>],
        start: usize,
        end: usize,
        indent: usize,
        key: &str,
    ) -> Option<usize> {
        (start..end).find(|&at| {
            !lines[at].blank
                && lines[at].indent == indent
                && key_of(lines[at].text).is_some_and(|(found, _)| found == key)
        })
    }

    fn quote_key(key: &str) -> String {
        if key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            key.to_owned()
        } else {
            serde_json::to_string(key).unwrap_or_default()
        }
    }

    /// `value` as block YAML at `indent`, under `key`.
    fn render(out: &mut String, indent: usize, key: &str, value: &serde_json::Value) {
        let pad = " ".repeat(indent);
        match value {
            serde_json::Value::Object(map) => {
                out.push_str(&format!("{pad}{}:\n", quote_key(key)));
                for (child, value) in map {
                    render(out, indent + 2, child, value);
                }
            }
            serde_json::Value::Array(items) => {
                out.push_str(&format!("{pad}{}:\n", quote_key(key)));
                for item in items {
                    out.push_str(&format!("{pad}  - {item}\n"));
                }
            }
            scalar => out.push_str(&format!("{pad}{}: {scalar}\n", quote_key(key))),
        }
    }

    pub fn edit(
        text: &str,
        key_path: &[String],
        name: &str,
        entry: &serde_json::Value,
    ) -> Result<Edit, String> {
        let all = lines(text);
        let (mut start, mut end) = (0, all.len());
        let mut indent = (0..end)
            .find(|&at| !all[at].blank)
            .map_or(0, |at| all[at].indent);
        let mut replace_line: Option<(usize, String)> = None;
        let mut rest_path: &[String] = key_path;
        while let Some((segment, rest)) = rest_path.split_first() {
            let Some(at) = find(&all, start, end, indent, segment) else {
                break;
            };
            let (_, after) = key_of(all[at].text).unwrap_or_default();
            if after.trim() == "{}" {
                // An empty flow map is rewritten as a block key.
                replace_line = Some((
                    at,
                    format!("{}{}:\n", " ".repeat(indent), quote_key(segment)),
                ));
            } else if !opens_block(after) {
                return Err(format!(
                    "YAML: '{segment}' is not a block mapping; edit it by hand"
                ));
            }
            let block = block_end(&all, at, end);
            let child = (at + 1..block)
                .find(|&line| !all[line].blank)
                .map_or(indent + 2, |line| all[line].indent);
            if child <= indent && block > at + 1 {
                return Err(format!("YAML: '{segment}' has no nested block"));
            }
            (start, end, indent) = (at + 1, block, child);
            rest_path = rest;
        }
        let mut rendered = String::new();
        let (from, to) = if rest_path.is_empty() {
            match find(&all, start, end, indent, name) {
                Some(at) => {
                    let block = block_end(&all, at, end);
                    render(&mut rendered, indent, name, entry);
                    let existing: String = all[at..block].iter().map(|line| line.text).collect();
                    if existing == rendered {
                        return Ok(Edit::Unchanged);
                    }
                    (at, block)
                }
                None => {
                    render(&mut rendered, indent, name, entry);
                    (end, end)
                }
            }
        } else {
            let mut nested = serde_json::json!({ name: entry });
            for key in rest_path[1..].iter().rev() {
                nested = serde_json::json!({ key: nested });
            }
            render(&mut rendered, indent, &rest_path[0], &nested);
            (end, end)
        };
        let mut out = String::with_capacity(text.len() + rendered.len());
        for (at, line) in all.iter().enumerate().take(from) {
            match &replace_line {
                Some((replaced, text)) if *replaced == at => out.push_str(text),
                _ => out.push_str(line.text),
            }
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&rendered);
        for line in &all[to..] {
            out.push_str(line.text);
        }
        Ok(Edit::Changed(out))
    }

    /// A servers map declared at the top level or one level down: a known
    /// key whose block names a `command` or a `url`.
    pub fn shape(text: &str) -> Option<(String, usize)> {
        let all = lines(text);
        for at in 0..all.len() {
            if all[at].blank || all[at].indent > 2 {
                continue;
            }
            let Some((key, after)) = key_of(all[at].text) else {
                continue;
            };
            if !SERVER_KEYS.contains(&key.as_str()) || !opens_block(after) {
                continue;
            }
            let block = block_end(&all, at, all.len());
            let child = (at + 1..block).find(|&line| !all[line].blank)?;
            let child_indent = all[child].indent;
            let servers = (at + 1..block)
                .filter(|&line| !all[line].blank && all[line].indent == child_indent)
                .filter(|&line| {
                    let inner = block_end(&all, line, block);
                    (line + 1..inner).any(|field| {
                        key_of(all[field].text)
                            .is_some_and(|(name, _)| name == "command" || name == "url")
                    })
                })
                .count();
            if servers > 0 {
                let parent = (0..at)
                    .rev()
                    .find(|&line| !all[line].blank && all[line].indent < all[at].indent)
                    .and_then(|line| key_of(all[line].text))
                    .map(|(parent, _)| format!("{parent}."))
                    .unwrap_or_default();
                return Some((format!("{parent}{key}"), servers));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> McpServerConfig {
        McpServerConfig {
            name: "tools".to_owned(),
            command: "npx".to_owned(),
            args: vec!["-y".to_owned(), "pkg".to_owned()],
            env: vec![("TOKEN".to_owned(), "t".to_owned())],
            http: None,
        }
    }

    fn key(path: &str) -> Vec<String> {
        path.split('.').map(str::to_owned).collect()
    }

    fn changed(edit: Edit) -> String {
        match edit {
            Edit::Changed(text) => text,
            Edit::Unchanged => panic!("expected a change"),
        }
    }

    /// Nothing of the old text was changed or dropped — every old byte is
    /// still there, in order — and what was added holds the new entry.
    fn only_additions(old: &str, new: &str, block_marker: &str) {
        let mut added = String::new();
        let mut old_bytes = old.chars().peekable();
        for c in new.chars() {
            if old_bytes.peek() == Some(&c) {
                old_bytes.next();
            } else {
                added.push(c);
            }
        }
        assert!(
            old_bytes.peek().is_none(),
            "old text was changed, not just added to:\n{new}"
        );
        assert!(added.contains(block_marker), "{added:?}");
        // The additions are the entry and the separator it needs, no more.
        let stripped: String = added.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(stripped.len() < 120, "{added:?}");
    }

    #[test]
    fn jsonc_keeps_comments_order_and_other_keys_and_is_unchanged_on_repeat() {
        let old = r#"{
  // the editor's own settings
  "theme": "dark", /* inline */
  "mcpServers": {
    // existing
    "other": { "command": "x" } // keep me
  },
  "zeta": [1, 2]
}
"#;
        let entry = entry_value(&server());
        let new =
            changed(edit(old, Format::Jsonc, &key("mcpServers"), "tools", &entry).expect("edit"));
        only_additions(old, &new, "\"tools\"");
        assert!(new.contains("// keep me") && new.contains("/* inline */"));
        let parsed: serde_json::Value = serde_json::from_str(&jsonc::strip(&new)).expect("json");
        assert_eq!(parsed["mcpServers"]["tools"], entry);
        assert_eq!(parsed["mcpServers"]["other"]["command"], "x");
        assert_eq!(parsed["zeta"], serde_json::json!([1, 2]));
        assert_eq!(
            edit(&new, Format::Jsonc, &key("mcpServers"), "tools", &entry).expect("again"),
            Edit::Unchanged
        );
    }

    #[test]
    fn jsonc_creates_a_missing_map_and_replaces_a_stale_entry_in_place() {
        let old = "{\n  \"a\": 1\n}\n";
        let entry = entry_value(&server());
        let new =
            changed(edit(old, Format::Jsonc, &key("x.servers"), "tools", &entry).expect("edit"));
        let parsed: serde_json::Value = serde_json::from_str(&new).expect("json");
        assert_eq!(parsed["x"]["servers"]["tools"], entry);
        assert_eq!(parsed["a"], 1);
        // A stale entry is replaced where it stands.
        let stale = "{\n  \"mcpServers\": {\n    \"tools\": {\"command\": \"old\"},\n    \"z\": {}\n  }\n}\n";
        let new =
            changed(edit(stale, Format::Jsonc, &key("mcpServers"), "tools", &entry).expect("edit"));
        let parsed: serde_json::Value = serde_json::from_str(&new).expect("json");
        assert_eq!(parsed["mcpServers"]["tools"], entry);
        assert!(new.find("\"tools\"") < new.find("\"z\""), "{new}");
        // Not an object where the path needs one: refused, not guessed.
        assert!(
            edit(
                "{\"mcpServers\": []}",
                Format::Jsonc,
                &key("mcpServers"),
                "t",
                &entry
            )
            .is_err()
        );
        assert!(edit("[1]", Format::Jsonc, &key("mcpServers"), "t", &entry).is_err());
    }

    #[test]
    fn toml_keeps_comments_and_other_tables_and_is_unchanged_on_repeat() {
        let old = "# top comment\ntitle = \"x\"\n\n[mcp_servers.other]\ncommand = \"y\" # keep\n\n[z]\nk = 1\n";
        let entry = entry_value(&server());
        let new =
            changed(edit(old, Format::Toml, &key("mcp_servers"), "tools", &entry).expect("edit"));
        assert!(new.starts_with("# top comment\ntitle = \"x\"\n"), "{new}");
        assert!(new.contains("command = \"y\" # keep"), "{new}");
        let parsed: toml::Value = toml::from_str(&new).expect("toml");
        assert_eq!(
            serde_json::to_value(&parsed["mcp_servers"]["tools"]).expect("json"),
            entry
        );
        assert_eq!(parsed["z"]["k"].as_integer(), Some(1));
        assert_eq!(
            edit(&new, Format::Toml, &key("mcp_servers"), "tools", &entry).expect("again"),
            Edit::Unchanged
        );
    }

    #[test]
    fn yaml_keeps_comments_and_other_keys_and_is_unchanged_on_repeat() {
        let old = "# settings\ntheme: dark\nmcpServers:\n  # existing\n  other:\n    command: x # keep\nzeta:\n  - 1\n";
        let entry = entry_value(&server());
        let new =
            changed(edit(old, Format::Yaml, &key("mcpServers"), "tools", &entry).expect("edit"));
        only_additions(old, &new, "tools:");
        assert!(
            new.contains("command: x # keep") && new.ends_with("zeta:\n  - 1\n"),
            "{new}"
        );
        assert!(
            new.contains(
                "  tools:\n    args:\n      - \"-y\"\n      - \"pkg\"\n    command: \"npx\"\n"
            ),
            "{new}"
        );
        assert_eq!(
            edit(&new, Format::Yaml, &key("mcpServers"), "tools", &entry).expect("again"),
            Edit::Unchanged
        );
        // A missing map is appended; a flow-style one is refused.
        let new = changed(
            edit("a: 1\n", Format::Yaml, &key("mcpServers"), "tools", &entry).expect("edit"),
        );
        assert!(new.starts_with("a: 1\nmcpServers:\n  tools:\n"), "{new}");
        assert!(
            edit(
                "mcpServers: {x: 1}\n",
                Format::Yaml,
                &key("mcpServers"),
                "t",
                &entry
            )
            .is_err()
        );
        let new = changed(
            edit(
                "mcpServers: {}\n",
                Format::Yaml,
                &key("mcpServers"),
                "t",
                &entry,
            )
            .expect("edit"),
        );
        assert!(new.starts_with("mcpServers:\n  t:\n"), "{new}");
    }

    #[test]
    fn discovery_finds_files_by_shape_not_name() {
        let root = std::env::temp_dir().join(format!(
            "rapidlm-discover-{}-{}",
            std::process::id(),
            protocol::TraceId::new()
        ));
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("dirs");
        std::fs::write(
            nested.join("one.json"),
            "{\n // c\n \"mcpServers\": {\"s\": {\"command\": \"x\"}}\n}",
        )
        .expect("json");
        std::fs::write(
            root.join("two.toml"),
            "[tool.mcp_servers.s]\nurl = \"https://x\"\n",
        )
        .expect("toml");
        std::fs::write(root.join("three.yaml"), "servers:\n  s:\n    command: x\n").expect("yaml");
        // Shapes that are not a servers map, whatever they are called.
        std::fs::write(root.join("mcp.json"), "{\"mcpServers\": {}}").expect("empty");
        std::fs::write(root.join("servers.yaml"), "servers:\n  - a\n").expect("list");
        std::fs::write(root.join("notes.txt"), "mcpServers: {s: {command: x}}").expect("txt");
        let found = discover(std::slice::from_ref(&root));
        let names: Vec<(String, &str)> = found
            .iter()
            .map(|entry| {
                (
                    entry
                        .path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    entry.key.as_str(),
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                ("one.json".to_owned(), "mcpServers"),
                ("three.yaml".to_owned(), "servers"),
                ("two.toml".to_owned(), "tool.mcp_servers"),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_roots_come_from_the_environment_first() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        let roots = discover_roots(&env(&[(CONFIG_ROOTS_ENV, "/r1"), ("HOME", "/h")]));
        assert_eq!(roots, [PathBuf::from("/r1")]);
        let roots = discover_roots(&env(&[("HOME", "/h"), ("XDG_CONFIG_HOME", "/x")]));
        assert_eq!(roots, [PathBuf::from("/h"), PathBuf::from("/x")]);
    }

    #[test]
    fn a_held_lock_refuses_a_second_install() {
        let home = std::env::temp_dir().join(format!(
            "rapidlm-install-lock-{}-{}",
            std::process::id(),
            protocol::TraceId::new()
        ));
        let held = InstallLock::acquire(&home).expect("first");
        let err = InstallLock::acquire(&home).err().expect("refused");
        assert!(err.contains("holds the lock"), "{err}");
        drop(held);
        InstallLock::acquire(&home).expect("free again");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_diff_names_only_the_changed_lines() {
        let diff = diff(Path::new("f"), "a\nb\nc\n", "a\nb\nx\ny\nc\n");
        assert_eq!(diff, "--- f\n+++ f\n@@ line 3 @@\n+x\n+y\n");
    }
}
