//! A bounded line diff for the `/diff` panel.
//!
//! In-tree rather than a dependency, following the precedent set when the
//! keychain backend chose the OS `security` CLI over a crypto crate and the
//! scheduler hand-rolled Kahn's algorithm: a small, well-understood
//! algorithm that this tree fully controls. This is the classic Myers O(ND)
//! forward search with a recorded trace for backtracking, which is exact —
//! it finds a shortest edit script — and whose cost is bounded here twice:
//! by the number of lines on either side and by the edit distance it will
//! search before giving up. A diff that exceeds either bound reports
//! [`DiffOutcome::TooLarge`] and the caller falls back to what it can say
//! truthfully without one (line counts).
//!
//! The output is unified-diff text, which is what a reader expects to see
//! and what every other tool renders. It is deliberately a *display* aid:
//! nothing here applies a patch, and the `workspace` crate's semantic patch
//! model (byte-range ops) is untouched.

use std::fmt::Write as _;

/// Most lines on either side a diff will be attempted for.
///
/// The Myers search is O((N+M)·D); with both bounds this is at most a few
/// million steps and a few megabytes of trace, a cost a write can absorb.
pub const MAX_DIFF_LINES: usize = 2_000;

/// Most edits (insertions + deletions) the search will pursue before giving
/// up. A rewrite that replaces most of a file is not usefully shown as a
/// diff anyway.
pub const MAX_DIFF_EDITS: usize = 1_000;

/// Unchanged lines shown around each change, as `git diff` does.
pub const CONTEXT_LINES: usize = 3;

/// Hard cap on the unified text produced, so it fits the ledger payload's
/// display bound with room to spare. Cut at a line boundary and marked.
pub const MAX_UNIFIED_BYTES: usize = 8 * 1024;

// The reducer reads `hunks` through `optional_display`, which errors — and
// freezes the session — on a payload field over the display bound. The cap
// plus the truncation marker must fit under it, and a build error is the
// right place to learn otherwise.
const _: () = assert!(
    MAX_UNIFIED_BYTES + 64 <= tui::state::MAX_DISPLAY_TEXT_BYTES,
    "hunk text plus its truncation marker must fit the reducer's display bound"
);

const TRUNCATION_MARKER: &str = "... (diff truncated)";

/// What a diff attempt produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffOutcome {
    /// Nothing changed line-for-line.
    Identical,
    /// Unified-diff text, possibly truncated at [`MAX_UNIFIED_BYTES`].
    Unified(String),
    /// One side exceeded [`MAX_DIFF_LINES`] or the edit distance exceeded
    /// [`MAX_DIFF_EDITS`]; nothing truthful can be rendered as hunks.
    TooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Edit {
    Keep,
    Delete,
    Insert,
}

/// Diff `before` against `after` line by line, producing unified hunks.
pub fn unified(before: &str, after: &str) -> DiffOutcome {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    if a.len() > MAX_DIFF_LINES || b.len() > MAX_DIFF_LINES {
        return DiffOutcome::TooLarge;
    }
    let Some(script) = myers(&a, &b) else {
        return DiffOutcome::TooLarge;
    };
    if script.iter().all(|edit| *edit == Edit::Keep) {
        return DiffOutcome::Identical;
    }
    DiffOutcome::Unified(render(&a, &b, &script))
}

/// Shortest edit script from `a` to `b`, or `None` past [`MAX_DIFF_EDITS`].
///
/// Standard forward Myers: `v[k]` holds the furthest x reached on diagonal
/// k = x − y after d edits. Each d's `v` is kept so the path can be walked
/// back once the end is reached.
fn myers(a: &[&str], b: &[&str]) -> Option<Vec<Edit>> {
    let n = a.len();
    let m = b.len();
    let max = n + m;
    if max == 0 {
        return Some(Vec::new());
    }
    let limit = max.min(MAX_DIFF_EDITS);
    // Diagonal k ranges over [-limit, limit]; index it by k + limit.
    let width = 2 * limit + 1;
    let offset = limit;
    let mut v: Vec<usize> = vec![0; width];
    let mut trace: Vec<Vec<usize>> = Vec::new();

    for d in 0..=limit {
        trace.push(v.clone());
        let mut k = -(d as isize);
        while k <= d as isize {
            let idx = (k + offset as isize) as usize;
            let mut x = if k == -(d as isize) || (k != d as isize && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y = (x as isize - k) as usize;
            while x < n && y < m && a[x] == b[y] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                return Some(backtrack(a, b, &trace, d, offset));
            }
            k += 2;
        }
    }
    None
}

fn backtrack(
    a: &[&str],
    b: &[&str],
    trace: &[Vec<usize>],
    d_end: usize,
    offset: usize,
) -> Vec<Edit> {
    let mut edits = Vec::new();
    let mut x = a.len();
    let mut y = b.len();
    for d in (0..=d_end).rev() {
        let v = &trace[d];
        let k = x as isize - y as isize;
        let idx = (k + offset as isize) as usize;
        let prev_k = if d == 0 {
            k
        } else if k == -(d as isize) || (k != d as isize && v[idx - 1] < v[idx + 1]) {
            k + 1
        } else {
            k - 1
        };
        let prev_idx = (prev_k + offset as isize) as usize;
        let prev_x = if d == 0 { 0 } else { v[prev_idx] };
        let prev_y = (prev_x as isize - prev_k) as usize;
        // The snake: matching lines walked forward from (prev_x, prev_y).
        while x > prev_x && y > prev_y {
            edits.push(Edit::Keep);
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                edits.push(Edit::Insert);
                y -= 1;
            } else {
                edits.push(Edit::Delete);
                x -= 1;
            }
        }
    }
    edits.reverse();
    edits
}

/// Render an edit script as unified hunks with [`CONTEXT_LINES`] of context.
fn render(a: &[&str], b: &[&str], script: &[Edit]) -> String {
    // Walk the script into (kind, a_index, b_index) rows so hunks can be
    // grouped by proximity.
    struct Row {
        edit: Edit,
        ai: usize,
        bi: usize,
    }
    let mut rows = Vec::with_capacity(script.len());
    let (mut ai, mut bi) = (0usize, 0usize);
    for edit in script {
        rows.push(Row {
            edit: *edit,
            ai,
            bi,
        });
        match edit {
            Edit::Keep => {
                ai += 1;
                bi += 1;
            }
            Edit::Delete => ai += 1,
            Edit::Insert => bi += 1,
        }
    }

    let mut out = String::new();
    let mut i = 0;
    while i < rows.len() {
        if rows[i].edit == Edit::Keep {
            i += 1;
            continue;
        }
        // A hunk runs from CONTEXT_LINES before this change to
        // CONTEXT_LINES after the last change within 2*CONTEXT_LINES of it.
        let start = i.saturating_sub(CONTEXT_LINES);
        let mut end = i;
        let mut j = i;
        while j < rows.len() {
            if rows[j].edit != Edit::Keep {
                end = j;
                j += 1;
                continue;
            }
            // A run of unchanged rows: stop if it is longer than two
            // contexts, since the next change would start its own hunk.
            let run_start = j;
            while j < rows.len() && rows[j].edit == Edit::Keep {
                j += 1;
            }
            if j >= rows.len() || j - run_start > 2 * CONTEXT_LINES {
                break;
            }
        }
        let stop = (end + 1 + CONTEXT_LINES).min(rows.len());

        let a_start = rows[start].ai;
        let b_start = rows[start].bi;
        let a_count = rows[start..stop]
            .iter()
            .filter(|row| row.edit != Edit::Insert)
            .count();
        let b_count = rows[start..stop]
            .iter()
            .filter(|row| row.edit != Edit::Delete)
            .count();
        let _ = writeln!(
            out,
            "@@ -{},{} +{},{} @@",
            a_start + 1,
            a_count,
            b_start + 1,
            b_count
        );
        for row in &rows[start..stop] {
            let (sigil, text) = match row.edit {
                Edit::Keep => (' ', a[row.ai]),
                Edit::Delete => ('-', a[row.ai]),
                Edit::Insert => ('+', b[row.bi]),
            };
            let _ = writeln!(out, "{sigil}{text}");
            if out.len() > MAX_UNIFIED_BYTES {
                return truncate(out);
            }
        }
        i = stop;
    }
    out
}

/// Cut at the last line boundary under the cap and say so.
///
/// The cap is a byte count and the text is UTF-8, so the cap can land in
/// the middle of a multi-byte character; slicing there would panic — in
/// the write path, on a source file with a non-ASCII comment. Back off to
/// a character boundary first, then to the line.
fn truncate(mut out: String) -> String {
    let mut limit = MAX_UNIFIED_BYTES.min(out.len());
    while !out.is_char_boundary(limit) {
        limit -= 1;
    }
    let cut = out[..limit].rfind('\n').map(|at| at + 1).unwrap_or(0);
    out.truncate(cut);
    out.push_str(TRUNCATION_MARKER);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_inputs_are_identical() {
        assert_eq!(unified("a\nb\nc\n", "a\nb\nc\n"), DiffOutcome::Identical);
        assert_eq!(unified("", ""), DiffOutcome::Identical);
    }

    #[test]
    fn a_single_changed_line_yields_one_hunk_with_context() {
        let before = "one\ntwo\nthree\nfour\nfive\nsix\nseven\n";
        let after = "one\ntwo\nthree\nFOUR\nfive\nsix\nseven\n";
        let DiffOutcome::Unified(text) = unified(before, after) else {
            panic!("expected hunks");
        };
        assert_eq!(
            text,
            "@@ -1,7 +1,7 @@\n one\n two\n three\n-four\n+FOUR\n five\n six\n seven\n"
        );
    }

    #[test]
    fn distant_changes_become_separate_hunks() {
        let before: String = (1..=30).map(|n| format!("l{n}\n")).collect();
        let after = before.replace("l2\n", "L2\n").replace("l28\n", "L28\n");
        let DiffOutcome::Unified(text) = unified(&before, &after) else {
            panic!("expected hunks");
        };
        assert_eq!(text.matches("@@ ").count(), 2, "{text}");
        assert!(
            text.starts_with("@@ -1,5 +1,5 @@\n l1\n-l2\n+L2\n l3\n"),
            "{text}"
        );
        assert!(text.contains("@@ -25,6 +25,6 @@\n"), "{text}");
    }

    #[test]
    fn insertions_and_deletions_are_exact() {
        let DiffOutcome::Unified(text) = unified("a\nb\n", "a\nx\nb\n") else {
            panic!("expected hunks");
        };
        assert_eq!(text, "@@ -1,2 +1,3 @@\n a\n+x\n b\n");
        let DiffOutcome::Unified(text) = unified("a\nx\nb\n", "a\nb\n") else {
            panic!("expected hunks");
        };
        assert_eq!(text, "@@ -1,3 +1,2 @@\n a\n-x\n b\n");
        // A file that did not exist, and a file emptied.
        let DiffOutcome::Unified(text) = unified("", "new\n") else {
            panic!("expected hunks");
        };
        assert_eq!(text, "@@ -1,0 +1,1 @@\n+new\n");
    }

    #[test]
    fn the_script_is_a_shortest_edit_and_reconstructs_both_sides() {
        // Property: applying the script to `a` yields `b`, and the edit
        // count equals the known distance for these inputs.
        let a = ["a", "b", "c", "a", "b", "b", "a"];
        let b = ["c", "b", "a", "b", "a", "c"];
        let script = myers(&a, &b).expect("within bounds");
        let edits = script.iter().filter(|e| **e != Edit::Keep).count();
        assert_eq!(edits, 5, "the textbook example has distance 5");
        let (mut ai, mut bi) = (0, 0);
        let mut rebuilt = Vec::new();
        for edit in &script {
            match edit {
                Edit::Keep => {
                    assert_eq!(a[ai], b[bi]);
                    rebuilt.push(b[bi]);
                    ai += 1;
                    bi += 1;
                }
                Edit::Delete => ai += 1,
                Edit::Insert => {
                    rebuilt.push(b[bi]);
                    bi += 1;
                }
            }
        }
        assert_eq!(rebuilt, b);
        assert_eq!((ai, bi), (a.len(), b.len()));
    }

    #[test]
    fn oversized_inputs_and_rewrites_are_refused_not_rendered() {
        let big: String = (0..MAX_DIFF_LINES + 1).map(|n| format!("{n}\n")).collect();
        assert_eq!(unified(&big, "x\n"), DiffOutcome::TooLarge);
        // Every line different, more edits than the search will pursue.
        let a: String = (0..MAX_DIFF_LINES).map(|n| format!("a{n}\n")).collect();
        let b: String = (0..MAX_DIFF_LINES).map(|n| format!("b{n}\n")).collect();
        assert_eq!(unified(&a, &b), DiffOutcome::TooLarge);
    }

    #[test]
    fn the_cut_backs_off_to_a_character_boundary() {
        // Built so that byte `MAX_UNIFIED_BYTES` is provably *inside* a
        // three-byte character: a newline early on for the cut to land on,
        // ASCII up to one byte before the cap, then `日`. A slice at the cap
        // panics; the first version did exactly that. (The end-to-end
        // fixture below passed with the naive slice — its cap happened to
        // fall on a boundary — which is why this test exists.)
        let mut out = String::new();
        out.push_str(&"x".repeat(100));
        out.push('\n');
        out.push_str(&"x".repeat(MAX_UNIFIED_BYTES - 102));
        assert_eq!(out.len(), MAX_UNIFIED_BYTES - 1);
        out.push('日');
        out.push('\n');
        assert!(
            !out.is_char_boundary(MAX_UNIFIED_BYTES),
            "the cap must land mid-character"
        );

        let cut = truncate(out);
        assert!(cut.starts_with(&format!("{}\n{TRUNCATION_MARKER}\n", "x".repeat(100))));
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        // Lines of multi-byte text long enough that the byte cap lands
        // inside a character on some line. The first version sliced the
        // String at the cap and would have panicked here — in the write
        // path, on any source file with a non-ASCII comment.
        let before: String = (0..MAX_DIFF_LINES)
            .map(|n| format!("зміна {n}\n"))
            .collect();
        let after: String = (0..MAX_DIFF_LINES)
            .map(|n| {
                if n % 5 == 0 {
                    format!("ЗМІНА {n}\n")
                } else {
                    format!("зміна {n}\n")
                }
            })
            .collect();
        let DiffOutcome::Unified(text) = unified(&before, &after) else {
            panic!("expected hunks");
        };
        assert!(text.ends_with(&format!("{TRUNCATION_MARKER}\n")));
        assert!(text.is_char_boundary(text.len()));
        // Every line is intact: the marker follows a complete line.
        let body = &text[..text.len() - TRUNCATION_MARKER.len() - 1];
        assert!(body.ends_with('\n'));
        assert!(
            body.lines()
                .all(|line| line.is_empty() || line.starts_with(['@', ' ', '-', '+']))
        );
    }

    #[test]
    fn long_output_is_cut_at_a_line_and_marked() {
        // Many small, spread-out changes: each hunk is short but there are
        // enough of them to pass the byte cap.
        let before: String = (0..MAX_DIFF_LINES).map(|n| format!("line {n}\n")).collect();
        let after: String = (0..MAX_DIFF_LINES)
            .map(|n| {
                if n % 10 == 0 {
                    format!("LINE {n}\n")
                } else {
                    format!("line {n}\n")
                }
            })
            .collect();
        let DiffOutcome::Unified(text) = unified(&before, &after) else {
            panic!("expected hunks");
        };
        assert!(
            text.len() <= MAX_UNIFIED_BYTES + TRUNCATION_MARKER.len() + 1,
            "{}",
            text.len()
        );
        assert!(text.ends_with(&format!("{TRUNCATION_MARKER}\n")));
        // Cut on a line boundary: the line before the marker is complete.
        let body = &text[..text.len() - TRUNCATION_MARKER.len() - 1];
        assert!(body.ends_with('\n'));
    }
}
