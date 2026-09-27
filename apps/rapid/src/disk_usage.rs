//! `rapid du [--reclaim-plan] [--output text|json]` (SEAM-07): what
//! RapidLM keeps on disk, by the real layout — the project's `.rapidlm`
//! tree, the user's home (`RAPIDLM_HOME`: config, trust and permission
//! files) and the worktree store under the repository's `.git/rapidlm`.
//! Read only: `--reclaim-plan` lists what `rapid worktree reclaim` would
//! remove (the SEAM-08 rule, as a dry run) and writes nothing — the plan
//! opens no journal and creates no ledger.

use std::path::{Path, PathBuf};

pub const DU_HELP: &str = "\
usage: rapid du [--reclaim-plan] [--output text|json]

Bytes (file lengths; symlinks are not followed) RapidLM keeps for this
project, in areas that never overlap: `project` (its .rapidlm tree), `home`
(the user's RapidLM home), `worktrees` (each worktree under .git/rapidlm) and
`worktree-store` (the rest of .git/rapidlm: records and leases).

--reclaim-plan  also list the worktrees `rapid worktree reclaim` would remove
                (the same list as `rapid worktree reclaim --dry-run`);
                nothing is deleted
";

/// One measured area and its top-level entries.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Area {
    pub area: &'static str,
    pub path: PathBuf,
    pub bytes: u64,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Entry {
    pub name: String,
    pub bytes: u64,
}

/// Total file length under `path`, not following symlinks (a link counts
/// as its own length). A missing path is 0; an unreadable one counts what
/// could be read.
pub fn tree_bytes(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in read.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(item.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(item.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// `path` measured with each top-level entry, largest first.
pub fn area(area: &'static str, path: &Path) -> Area {
    area_without(area, path, None)
}

/// [`area`] leaving out the entry named `skip` — measured as an area of
/// its own, so no byte is counted in two areas.
pub fn area_without(area: &'static str, path: &Path, skip: Option<&str>) -> Area {
    let mut entries: Vec<Entry> = std::fs::read_dir(path)
        .map(|read| {
            read.flatten()
                .filter(|item| skip != Some(item.file_name().to_string_lossy().as_ref()))
                .map(|item| Entry {
                    name: item.file_name().to_string_lossy().into_owned(),
                    bytes: tree_bytes(&item.path()),
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    Area {
        area,
        path: path.to_path_buf(),
        bytes: entries.iter().map(|entry| entry.bytes).sum(),
        entries,
    }
}

/// `bytes` for a person: B, KiB, MiB, GiB with one decimal.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `rapid du [--reclaim-plan] [--output text|json]`.
pub fn run_du(args: &[String]) -> Result<i32, crate::interactive::InteractiveError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{DU_HELP}");
        return Ok(0);
    }
    let mut plan = false;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--reclaim-plan" => plan = true,
            "--output" => match args.get(index + 1).map(String::as_str) {
                Some("json") => {
                    json = true;
                    index += 1;
                }
                Some("text") => index += 1,
                _ => {
                    eprintln!("rapid du: --output takes text or json");
                    return Ok(2);
                }
            },
            other => {
                eprintln!("rapid du: unknown argument '{other}'");
                eprint!("{DU_HELP}");
                return Ok(2);
            }
        }
        index += 1;
    }
    let Some((root, trusted)) = crate::interactive::workflow_workspace_root() else {
        eprintln!("rapid du: no project here");
        return Ok(2);
    };
    let mut areas = vec![area(
        "project",
        &root.join(crate::interactive::PROJECT_MARKER),
    )];
    if let Some(home) = crate::interactive::exec_user_home() {
        areas.push(area("home", &home));
    }
    // The worktree store: only a git project has one. Its worktrees (one
    // entry each) and the rest of it (records, leases) are separate areas;
    // the areas never overlap.
    let cancel = workspace::view::CancellationToken::new();
    let store_dir = workspace::backends::git_worktree::GitWorktreeStore::open(&root, &cancel)
        .ok()
        .map(|store| store.git_common_dir().join("rapidlm"));
    if let Some(dir) = &store_dir {
        areas.push(area("worktrees", &dir.join("worktrees")));
        areas.push(area_without("worktree-store", dir, Some("worktrees")));
    }
    let plan = if plan {
        if !trusted {
            eprintln!(
                "rapid du: --reclaim-plan needs a trusted project; run `rapid trust grant` here"
            );
            return Ok(2);
        }
        match crate::agent_views::reclaim_worktrees(&root, true) {
            Ok(plan) => Some(plan),
            Err(reason) => {
                eprintln!("rapid du: the reclaim plan could not be made: {reason}");
                return Ok(1);
            }
        }
    } else {
        None
    };
    if json {
        let plan_json: Option<Vec<serde_json::Value>> = plan.as_ref().map(|plan| {
            plan.iter()
                .map(|(entry, _)| {
                    serde_json::json!({
                        "view_id": entry.view_id.to_string(),
                        "name": entry.name,
                        "worktree": entry.worktree,
                        "bytes": tree_bytes(&entry.worktree),
                    })
                })
                .collect()
        });
        let doc = serde_json::json!({"areas": areas, "reclaim_plan": plan_json});
        println!("{doc}");
        return Ok(0);
    }
    for area in &areas {
        println!(
            "{}\t{}\t{} ({})",
            area.area,
            area.path.display(),
            area.bytes,
            human(area.bytes)
        );
        for entry in &area.entries {
            println!("  {}\t{} ({})", entry.name, entry.bytes, human(entry.bytes));
        }
    }
    if let Some(plan) = plan {
        if plan.is_empty() {
            println!("reclaim plan: nothing to reclaim");
        } else {
            let bytes: u64 = plan
                .iter()
                .map(|(entry, _)| tree_bytes(&entry.worktree))
                .sum();
            println!(
                "reclaim plan: {} worktree(s), {} ({})",
                plan.len(),
                bytes,
                human(bytes)
            );
            for (entry, _) in &plan {
                println!("{}", crate::worktree_cmd::plan_line(entry));
            }
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_match_the_files_and_symlinks_are_not_followed() {
        let root = std::env::temp_dir().join(format!(
            "rapid-du-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("sessions/deep")).unwrap();
        std::fs::write(root.join("sessions/a.jsonl"), vec![b'x'; 1000]).unwrap();
        std::fs::write(root.join("sessions/deep/b.jsonl"), vec![b'x'; 24]).unwrap();
        std::fs::write(root.join("goal.json"), vec![b'x'; 7]).unwrap();
        // A link to a large tree elsewhere counts as the link, not the tree.
        let elsewhere = root.with_extension("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("big"), vec![b'x'; 1 << 20]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, root.join("sessions/link")).unwrap();
        let measured = area("project", &root);
        let link =
            std::fs::symlink_metadata(root.join("sessions/link")).map_or(0, |meta| meta.len());
        assert_eq!(measured.bytes, 1000 + 24 + 7 + link);
        assert!(link < 4096);
        assert_eq!(measured.entries[0].name, "sessions");
        assert_eq!(measured.entries[0].bytes, 1024 + link);
        assert_eq!(tree_bytes(&root.join("missing")), 0);
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(7), "7 B");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }
}
