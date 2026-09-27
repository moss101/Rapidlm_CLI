//! `rapid worktree list|reclaim [--dry-run]|abandon <view>` (SEAM-08): the
//! worktrees `rapid exec --worktree` and `rapid goal create --worktree`
//! made, and their removal under the reclaim rule in
//! [`crate::agent_views::worktree_entries`].

use crate::agent_views::{Reclaim, WorktreeEntry};

pub const WORKTREE_HELP: &str = "\
usage: rapid worktree list
       rapid worktree reclaim [--dry-run]
       rapid worktree abandon <view-id|name>

list     every worktree of this project, and whether it may be reclaimed
reclaim  remove each reclaimable worktree, journaled as workspace.reclaim;
         --dry-run lists them and removes nothing
abandon  discard a worktree's work — uncommitted, ignored and untracked files,
         and commits made in it — journaled as workspace.abandon, which makes
         it reclaimable

A worktree is reclaimable only when it is clean (no uncommitted, untracked or
ignored files) and either abandoned with nothing committed since, or its HEAD
is already in the project's history; and neither a running session nor the
current goal holds it (where a session's liveness cannot be checked, any
session that has not released it holds it). The primary checkout is never a
candidate.
";

fn label(entry: &WorktreeEntry) -> String {
    match &entry.name {
        Some(name) => format!("{} ({name})", entry.view_id),
        None => entry.view_id.to_string(),
    }
}

fn verdict(entry: &WorktreeEntry) -> String {
    match &entry.verdict {
        Reclaim::Reclaimable(why) => format!("reclaimable: {why}"),
        Reclaim::Kept(why) => format!("kept: {why}"),
    }
}

pub fn run_worktree(args: &[String]) -> Result<i32, crate::interactive::InteractiveError> {
    if args.is_empty() {
        eprint!("{WORKTREE_HELP}");
        return Ok(2);
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{WORKTREE_HELP}");
        return Ok(0);
    }
    let usage = |message: &str| {
        eprintln!("rapid worktree: {message}");
        eprint!("{WORKTREE_HELP}");
        Ok(2)
    };
    let Some((root, trusted)) = crate::interactive::workflow_workspace_root() else {
        eprintln!("rapid worktree: no project here");
        return Ok(2);
    };
    if !trusted {
        eprintln!("rapid worktree: this project is not trusted; run `rapid trust grant` here");
        return Ok(2);
    }
    let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
    match (args[0].as_str(), rest.as_slice()) {
        ("list", []) => match crate::agent_views::worktree_entries(&root) {
            Ok(entries) => {
                if entries.is_empty() {
                    println!("no worktrees");
                }
                for entry in &entries {
                    println!(
                        "{}\t{}\t{}",
                        label(entry),
                        entry.worktree.display(),
                        verdict(entry)
                    );
                }
                Ok(0)
            }
            Err(reason) => {
                eprintln!("rapid worktree list: {reason}");
                Ok(1)
            }
        },
        ("reclaim", flags) if flags.iter().all(|flag| *flag == "--dry-run") => {
            let dry_run = !flags.is_empty();
            match crate::agent_views::reclaim_worktrees(&root, dry_run) {
                Ok(outcomes) => {
                    if outcomes.is_empty() {
                        println!("nothing to reclaim");
                    }
                    let mut failed = false;
                    for (entry, outcome) in &outcomes {
                        match outcome {
                            Ok(()) if dry_run => println!(
                                "would reclaim {}\t{}\t{}",
                                label(entry),
                                entry.worktree.display(),
                                verdict(entry)
                            ),
                            Ok(()) => {
                                println!("reclaimed {}\t{}", label(entry), entry.worktree.display())
                            }
                            Err(reason) => {
                                failed = true;
                                eprintln!("not reclaimed {}: {reason}", label(entry));
                            }
                        }
                    }
                    Ok(i32::from(failed))
                }
                Err(reason) => {
                    eprintln!("rapid worktree reclaim: {reason}");
                    Ok(1)
                }
            }
        }
        ("abandon", [selector]) => match crate::agent_views::abandon_worktree(&root, selector) {
            Ok(entry) => {
                println!(
                    "abandoned {}\t{} (reset to {})",
                    label(&entry),
                    entry.worktree.display(),
                    entry.base_commit
                );
                Ok(0)
            }
            Err(reason) => {
                eprintln!("rapid worktree abandon: {reason}");
                Ok(1)
            }
        },
        _ => usage("unknown arguments"),
    }
}
