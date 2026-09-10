//! Whether this build can actually perform each slash command bare `/help`
//! lists — computed by driving the real parser and dispatcher, never from a
//! hand-maintained table.
//!
//! `/help` printed 28 command families with nothing to say that roughly half
//! of them report "not available" the moment they are run. Every one of them
//! *is* honest when invoked (see `unsupported_command_text` and
//! `unrouted_inspector_text`), so the information existed per command; what
//! was missing was the listing-level answer, which is what a user reads
//! first.
//!
//! The obvious way to add it — a second list of "these ones work" — is the
//! defect this session has removed three times over (`CLI_USAGE` vs. the
//! dispatch table, `CATALOG` vs. `CATALOG_HELP`, `parse_mcp_servers` vs. its
//! own bounds). So instead each catalog entry's **own usage string** is
//! turned into concrete invocations, each is run through
//! [`tui::parse_command`] and [`tui::dispatch`], and the resulting
//! [`FrontendAction`] is classified. There is no availability table: if a
//! command gains a backend, this reports it the same day, and if a usage
//! string changes shape,
//! `every_synthesized_invocation_parses` fails rather than the synthesis
//! silently skipping it.

use tui::{FrontendAction, Inspector, KernelAction, KernelApi, LocalAction};

/// How much of one command family this build can actually perform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Availability {
    /// Every alternative in the usage line runs.
    Full,
    /// Some run and some report "not available".
    Partial,
    /// None run.
    None,
}

impl Availability {
    /// The fixed-width marker each `/help` line is prefixed with.
    ///
    /// Leading, not trailing: several usage lines are already 70+ characters,
    /// so a trailing marker wrapped onto the next row on an ordinary
    /// terminal — exactly where a reader is least likely to see it. A
    /// constant-width prefix keeps the command names aligned and stays
    /// visible however the line wraps. Blank for a fully working command, so
    /// the listing stays quiet about what is fine.
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Full => "    ",
            Self::Partial => " ~  ",
            Self::None => " !  ",
        }
    }
}

/// Whether [`crate::interactive::SessionLoop::apply_kernel_action`] has a
/// real backend for `action`.
///
/// Mirrors that function's own structure deliberately and minimally: the six
/// goal actions have explicit arms, and everything else is decided by
/// `kernel_api()` — the four APIs it implements versus the two it answers
/// with `unsupported_command_text`. Adding a `KernelApi` variant fails to
/// compile there before it can silently become "available" here.
pub(crate) fn kernel_action_is_supported(action: &KernelAction) -> bool {
    match action {
        KernelAction::StartGoal { .. }
        | KernelAction::PauseGoal
        | KernelAction::ResumeGoal
        | KernelAction::CancelGoal
        | KernelAction::RunGoal
        | KernelAction::StopGoal => true,
        // No per-job or per-agent cancellation backend exists at all — see
        // `cancels_a_specific_target`.
        other if cancels_a_specific_target(other) => false,
        other => matches!(
            other.kernel_api(),
            KernelApi::Interrupt
                | KernelApi::SubmitTurn
                | KernelApi::ForkSession
                | KernelApi::Rewind
        ),
    }
}

/// Whether this action asks to cancel or terminate a specific job or agent —
/// something nothing in this build can do.
///
/// All three map to `KernelApi::Interrupt`, whose only implementation is a
/// *session-wide* `interrupt_session` that takes no id. So the id-carrying
/// form cancelled the current turn and reported it as having cancelled the
/// thing named, and the bare form cancelled the current turn under a command
/// whose summary promises job/agent control. **Both** are refused: a first
/// pass here refused only the id-carrying form, which left `/jobs cancel`
/// killing a turn with no explanation and `/help` reporting `/jobs` as
/// fully working.
///
/// `apply_kernel_action` and [`kernel_action_is_supported`] both consult
/// this, so what `/help` reports and what the command does cannot disagree.
pub(crate) fn cancels_a_specific_target(action: &KernelAction) -> bool {
    // `CancelJob` was here until background jobs became session-scoped: the
    // session owns a job table, so `/jobs cancel <id>` now has both a target
    // to name and a mechanism to stop it. The agent actions still have
    // neither — `KernelApi::Interrupt` is session-wide and takes no id.
    matches!(
        action,
        KernelAction::CancelAgent { .. } | KernelAction::TerminateAgent { .. }
    )
}

/// Whether the frontend renders something real for `inspector`, rather than
/// the honest "not available" [`crate::interactive::unrouted_inspector_text`]
/// produces.
pub(crate) fn inspector_is_supported(inspector: &Inspector) -> bool {
    // `route().is_some()` is **not** the question, and using it was this
    // module's own worst bug: nine of the twelve `UiRoute`s resolve to a
    // panel that paints nothing, so `/diff`, `/memory` and `/jobs` were
    // reported as fully working while opening an empty sidebar — and on a
    // narrow terminal an empty route takes the whole transcript rect, so the
    // most visible effect was blanking the screen. The authority is
    // `tui::route_renders_content`, which lives beside the match that
    // actually decides it.
    //
    // `Mcp` and `Permissions` have no route at all and are answered inline
    // with a real report (see `open_unrouted_inspector`).
    inspector
        .route()
        .is_some_and(tui::route_renders_content)
        || matches!(inspector, Inspector::Mcp | Inspector::Permissions)
}

/// Classify one already-parsed command.
fn action_is_supported(action: &FrontendAction) -> bool {
    match action {
        // `/quit` and `/help` are always real.
        FrontendAction::Quit | FrontendAction::InlineHelp(_) => true,
        FrontendAction::Local(LocalAction::Open(inspector)) => inspector_is_supported(inspector),
        // Recording a grant is always available: it is the user's own act,
        // not a backend that might be missing.
        FrontendAction::Local(LocalAction::Permissions(_)) => true,
        FrontendAction::Kernel(action) => kernel_action_is_supported(action),
    }
}

/// Concrete invocations covering every alternative one usage line offers.
///
/// `/model [list|select <name>|doctor]` becomes `/model list`,
/// `/model select x`, `/model doctor`; `/quit` becomes just `/quit`.
/// Optional trailing operands (`[id]`, `[target]`) are omitted, and an
/// optional *flag* group (`[--agent <id>]`) is treated as no alternatives at
/// all, since the bare command is the thing being classified.
pub(crate) fn synthesize_invocations(usage: &str) -> Vec<String> {
    let mut parts = usage.splitn(2, ' ');
    let name = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or("").trim();
    let alternatives = match alternative_group(rest) {
        Operands::None => Vec::new(),
        // Deliberately produces an invocation that cannot parse, so
        // `every_synthesized_invocation_parses` fails loudly instead of the
        // command being silently classified from a form nobody checked.
        Operands::Unrecognized(raw) => {
            return vec![format!("{name} !unrecognized-usage-shape! {raw}")];
        }
        Operands::Alternatives(group) => group
            .split('|')
            .map(strip_operands)
            // `[--agent <id>]`-style flags are not alternatives; checked per
            // alternative rather than on the whole group, so a real list
            // whose first entry happens to be a flag is not dropped.
            .filter(|alt| !alt.is_empty() && !alt.starts_with("--"))
            .collect::<Vec<_>>(),
    };
    if alternatives.is_empty() {
        // No alternatives, but the line may still carry a *required*
        // operand (`/rewind <seq>`). Dropping it produced an invocation the
        // parser rejects, which the tripwire then blamed on the usage line.
        let operand = strip_operands(rest);
        return vec![if operand.is_empty() {
            name.to_owned()
        } else {
            format!("{name} {operand}")
        }];
    }
    alternatives
        .into_iter()
        .map(|alt| format!("{name} {alt}"))
        .collect()
}

/// What a usage line's operand section is.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Operands<'a> {
    /// No alternatives — the command is classified by its bare form.
    None,
    /// One `a|b|c` group.
    Alternatives(&'a str),
    /// A shape this synthesis does not understand. **Never** silently
    /// treated as "no alternatives": three of the four ways that used to
    /// happen (an unmatched `[`, a group after another token, a second
    /// group) produced an invocation that *parsed*, so the tripwire could
    /// not see them and the command was classified from a form nobody
    /// checked.
    Unrecognized(&'a str),
}

/// The `a|b|c` group a usage line offers, if any.
fn alternative_group(rest: &str) -> Operands<'_> {
    if rest.is_empty() {
        return Operands::None;
    }
    if let Some(stripped) = rest.strip_prefix('[') {
        // The *matching* bracket, not the first one: `/goal [show|budget
        // [turns N] [tokens N]|run]` nests, and taking `find(']')` truncated
        // the group after `budget [turns N`, silently losing `run` and
        // `stop`.
        let mut depth = 1usize;
        let mut end = None;
        for (index, ch) in stripped.char_indices() {
            match ch {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            // An unmatched `[`.
            return Operands::Unrecognized(rest);
        };
        let inner = &stripped[..end];
        let after = stripped[end + 1..].trim();
        // A second alternatives group would go uncovered entirely.
        if after.starts_with('[') && after.contains('|') {
            return Operands::Unrecognized(rest);
        }
        if !inner.contains('|') {
            // A single optional operand or flag — `[seq]`, `[--agent <id>]`.
            return Operands::None;
        }
        return Operands::Alternatives(inner);
    }
    // Bare alternatives: `/takeover terminal|browser|desktop|mobile`.
    if rest.contains('|') {
        // A group that starts after some other token is a shape this does
        // not cover; saying so beats inventing one invocation for it.
        let head = rest.split_whitespace().next().unwrap_or_default();
        if !head.contains('|') {
            return Operands::Unrecognized(rest);
        }
        let end = rest.find(" [").unwrap_or(rest.len());
        return Operands::Alternatives(&rest[..end]);
    }
    Operands::None
}

/// A syntactically valid typed id, for usage lines whose operand is one.
/// Only ever parsed, never resolved against anything.
const PLACEHOLDER_ID: &str = "01234567-89ab-7cde-89ab-0123456789ab";

/// One alternative reduced to something the parser accepts: nested optional
/// groups dropped, `<placeholders>` replaced by a plain word.
fn strip_operands(alt: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    let mut chars = alt.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            '<' => {
                // Substitute a value the operand's own parser accepts. Most
                // are free identifiers; `<id>` is a typed id
                // (`parse_typed_id`), so a bare word would be rejected and
                // the command would read as unavailable for the wrong
                // reason.
                let mut placeholder = String::new();
                for inner in chars.by_ref() {
                    if inner == '>' {
                        break;
                    }
                    placeholder.push(inner);
                }
                if depth == 0 {
                    out.push_str(match placeholder.as_str() {
                        "id" => PLACEHOLDER_ID,
                        "seq" => "1",
                        _ => "x",
                    });
                }
            }
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Availability of the command whose usage line is `usage`.
pub(crate) fn availability_of(usage: &str) -> Availability {
    let mut supported = 0usize;
    let mut total = 0usize;
    for invocation in synthesize_invocations(usage) {
        let Ok(parsed) = tui::parse_command(&invocation) else {
            // A synthesized invocation the parser rejects is a synthesis or
            // usage-string defect, not evidence about availability. It is
            // *not* silently skipped: `every_synthesized_invocation_parses`
            // fails on it. Counted as unsupported here so a broken usage
            // line can never read as "works".
            total += 1;
            continue;
        };
        total += 1;
        if action_is_supported(&tui::dispatch(parsed)) {
            supported += 1;
        }
    }
    match (supported, total) {
        (0, _) => Availability::None,
        (s, t) if s == t => Availability::Full,
        _ => Availability::Partial,
    }
}

/// Bare `/help`, with each line marked when this build cannot perform some
/// or all of it.
pub(crate) fn annotated_catalog_help() -> String {
    let mut text = String::new();
    let mut any_missing = false;
    for entry in tui::catalog() {
        let availability = availability_of(entry.usage);
        any_missing |= availability != Availability::Full;
        text.push_str(availability.prefix());
        text.push_str(entry.usage);
        text.push('\n');
    }
    if any_missing {
        text.push_str(&format!(
            "\n{} unavailable in this build    {} partly unavailable\nrun a marked command to see exactly what is missing; each one names its own gap\n",
            Availability::None.prefix().trim(),
            Availability::Partial.prefix().trim(),
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not an assertion — prints the real rendered help so a reviewer can
    /// see what a user sees. Run with `-- --nocapture --ignored`.
    #[test]
    #[ignore = "prints the rendered help for inspection"]
    fn show_the_rendered_help() {
        println!("{}", annotated_catalog_help());
    }

    #[test]
    fn every_synthesized_invocation_parses() {
        // The tripwire that keeps the synthesis honest. If a usage line
        // grows a shape this does not understand, availability for that
        // command would silently become "unavailable" — so it fails here
        // instead, naming the exact invocation.
        let mut checked = 0usize;
        for entry in tui::catalog() {
            for invocation in synthesize_invocations(entry.usage) {
                assert!(
                    tui::parse_command(&invocation).is_ok(),
                    "`{invocation}`, synthesized from `{}`, does not parse — either the \
synthesis does not understand that usage shape, or the usage line disagrees with the \
parser about which operands are required",
                    entry.usage
                );
                checked += 1;
            }
        }
        assert!(checked > 40, "expected many invocations, got {checked}");
    }

    #[test]
    fn a_usage_line_never_presents_a_required_operand_as_optional() {
        // `[...]` means optional everywhere else in this catalog
        // (`/rewind [seq]`, `/resume [session]`, `/help [command]`), so a
        // line whose *entire* operand section is bracketed is promising that
        // the bare command works. `/handoff [local|daemon|remote <target>]`
        // and `/rewind [seq]` both made that promise and both rejected the
        // bare form — and `/handoff`'s own error message then printed the
        // usage back, telling the user the thing they just typed is legal.
        for entry in tui::catalog() {
            let name = entry.usage.split(' ').next().unwrap_or_default();
            let rest = entry.usage[name.len()..].trim();
            let bare_parses = tui::parse_command(name).is_ok();
            let fully_optional = rest.is_empty()
                || (rest.starts_with('[') && rest.ends_with(']'));
            assert!(
                bare_parses || !fully_optional,
                "`{}` presents every operand as optional, but `{name}` alone does not parse",
                entry.usage
            );
        }
    }

    #[test]
    fn a_usage_shape_the_synthesis_cannot_read_is_loud_not_silently_ignored() {
        // Three of the four ways this used to degrade produced an
        // invocation that *parsed*, so `availability_of` classified the
        // command from a form nobody checked and the tripwire never fired.
        for malformed in [
            // Unmatched `[`.
            "/mcp [list|add <target>",
            // A group after another token.
            "/foo <name> [a|b]",
            // A second alternatives group.
            "/foo [a|b] [c|d]",
        ] {
            let synthesized = synthesize_invocations(malformed);
            assert_eq!(synthesized.len(), 1, "{malformed}: {synthesized:?}");
            assert!(
                tui::parse_command(&synthesized[0]).is_err(),
                "`{malformed}` must synthesize into something that fails loudly, got {:?}",
                synthesized[0]
            );
        }
        // A real alternatives list whose first entry is a flag must not be
        // dropped wholesale — the `--` check is per alternative.
        assert_eq!(
            synthesize_invocations("/foo [--flag <id>|list]"),
            vec!["/foo list".to_owned()]
        );
    }

    #[test]
    fn synthesis_covers_each_alternative_and_drops_optional_operands() {
        assert_eq!(synthesize_invocations("/quit"), vec!["/quit".to_owned()]);
        assert_eq!(
            synthesize_invocations("/model [list|select <name>|doctor]"),
            vec![
                "/model list".to_owned(),
                "/model select x".to_owned(),
                "/model doctor".to_owned()
            ]
        );
        // A trailing optional operand is not an alternative.
        assert_eq!(
            synthesize_invocations("/jobs [list|show|cancel|logs] [id]"),
            vec![
                "/jobs list".to_owned(),
                "/jobs show".to_owned(),
                "/jobs cancel".to_owned(),
                "/jobs logs".to_owned()
            ]
        );
        // An optional *flag* group is not a set of alternatives either.
        assert_eq!(
            synthesize_invocations("/diff [--agent <id>]"),
            vec!["/diff".to_owned()]
        );
        // A single optional operand is not an alternative.
        assert_eq!(
            synthesize_invocations("/rewind [seq]"),
            vec!["/rewind".to_owned()]
        );
        // Bare alternatives, with a trailing optional operand.
        assert_eq!(
            synthesize_invocations("/takeover terminal|browser|desktop|mobile"),
            vec![
                "/takeover terminal".to_owned(),
                "/takeover browser".to_owned(),
                "/takeover desktop".to_owned(),
                "/takeover mobile".to_owned()
            ]
        );
        // A nested optional group inside one alternative is dropped.
        assert_eq!(
            synthesize_invocations("/goal [show|budget [turns N] [tokens N]|run]"),
            vec![
                "/goal show".to_owned(),
                "/goal budget".to_owned(),
                "/goal run".to_owned()
            ]
        );
    }

    #[test]
    fn cancelling_a_job_or_agent_is_never_treated_as_a_turn_interrupt() {
        // `CancelJob`/`CancelAgent`/`TerminateAgent` map to
        // `KernelApi::Interrupt`, whose only implementation is a
        // *session-wide* interrupt taking no id. Honouring the id-carrying
        // form meant killing the current turn and reporting it as having
        // cancelled the thing named.
        use protocol::{AgentId, JobId};

        let job: JobId = "01234567-89ab-7cde-89ab-0123456789ab".parse().expect("job id");
        let agent: AgentId = "01234567-89ab-7cde-89ab-0123456789ab"
            .parse()
            .expect("agent id");

        // The agent forms, both with and without an id: there is still no
        // running-agent registry, so neither can name a target. A first pass
        // refused only the id-carrying form, which left `/agents cancel`
        // killing a turn with no explanation while `/help` reported it as
        // fully working.
        for action in [
            KernelAction::CancelAgent { id: Some(agent) },
            KernelAction::TerminateAgent { id: Some(agent) },
            KernelAction::CancelAgent { id: None },
            KernelAction::TerminateAgent { id: None },
        ] {
            assert!(cancels_a_specific_target(&action), "{action:?}");
            assert!(
                !kernel_action_is_supported(&action),
                "cancelling an agent must not read as supported: {action:?}"
            );
        }
        // `/jobs cancel` is the one that grew a backend: the session owns a
        // job table, so it has a target to name *and* a way to stop it. It
        // must read as supported — and, crucially, it must not have become
        // supported by quietly falling back to the session-wide interrupt,
        // which is what the rest of this test guards.
        for action in [
            KernelAction::CancelJob { id: Some(job) },
            KernelAction::CancelJob { id: None },
        ] {
            assert!(
                !cancels_a_specific_target(&action),
                "a job cancel now names a real target: {action:?}"
            );
            assert!(kernel_action_is_supported(&action), "{action:?}");
        }
        // The session-wide interrupt itself is still real — it is just not
        // what these commands claim to do.
        assert!(kernel_action_is_supported(&KernelAction::ForkSession));
    }

    #[test]
    fn a_route_is_only_called_available_if_it_actually_paints_something() {
        // The bug this module shipped with, and the reason it is worth a
        // dedicated test: `inspector_is_supported` asked
        // `Inspector::route().is_some()`, which is a *different fact* from
        // "the compositor paints anything for that route". Nine of the
        // twelve routes render nothing, so `/diff`, `/memory` and `/jobs`
        // were reported as fully working while opening an empty sidebar.
        //
        // Cross-checked against `tui::sidebar_lines` itself rather than
        // against a belief about it.
        use tui::state::{AppState, CancellationToken, UiRoute};
        let state = AppState::new();
        let cancel = CancellationToken::new();
        for route in [
            UiRoute::Transcript,
            UiRoute::Agents,
            UiRoute::Diff,
            UiRoute::Context,
            UiRoute::Memory,
            UiRoute::Jobs,
            UiRoute::Approvals,
            UiRoute::Goals,
            UiRoute::Graph,
            UiRoute::Computer,
            UiRoute::Resources,
            UiRoute::Models,
        ] {
            let paints_now = !tui::sidebar_lines(route, &state, 80, 24, &cancel).is_empty();
            let claims = tui::route_renders_content(route);
            // Every live panel renders a placeholder for an empty projection
            // (`/agents` a header, `/goals` "no goal", `/jobs` "no jobs",
            // `/approvals` "no approvals") and every dead route renders
            // nothing at all, so the two facts are exactly equivalent on an
            // empty state and the check runs in both directions. It used to
            // be one-directional, with a hand-listed set of dead routes
            // beside it — the second list this module exists to avoid, and
            // one that went stale the moment `/jobs` and `/approvals` were
            // given renderers.
            assert_eq!(
                paints_now, claims,
                "{route:?}: the compositor paints {paints_now} for an empty state \
while `route_renders_content` claims {claims}"
            );
        }
    }

    #[test]
    fn availability_reflects_what_the_dispatcher_really_does() {
        // Anchored on commands whose status is established elsewhere in this
        // crate's own tests, so this cannot drift into asserting itself.
        assert_eq!(availability_of("/quit"), Availability::Full);
        // Anchored on the catalog's *real* strings, not on copies: an
        // earlier version asserted against pre-correction usage lines that
        // no longer exist, so `/mcp`'s verdict came partly from those
        // strings failing to parse rather than from the backend gap the
        // comment describes.
        let usage = |name: &str| {
            tui::catalog()
                .find(|entry| entry.name == name)
                .unwrap_or_else(|| panic!("`{name}` is in the catalog"))
                .usage
        };
        // `/mcp list|doctor` render a real report; `add`/`remove`/`auth` are
        // approval-gated or have no backend and report why.
        assert_eq!(availability_of(usage("mcp")), Availability::Partial);
        // Every alternative reads or writes the real grant store.
        assert_eq!(availability_of(usage("permissions")), Availability::Full);
        // No knowledge-candidate store exists at all.
        assert_eq!(availability_of(usage("knowledge")), Availability::None);
        // `/memory` opens a panel that renders the project memory index, so
        // its single alternative is real. This said `None` until that panel
        // was given a renderer — the derivation working: availability follows
        // the compositor, and a hardcoded answer is what goes stale.
        assert_eq!(availability_of(usage("memory")), Availability::Full);
        // Coverage of the `None` case for a panel-only command, *derived*
        // rather than named. `/memory` and then `/diff` each held this spot
        // and each had to be edited when its panel became real — three edits
        // to keep asserting one unchanged property. Asking the compositor
        // which route still paints nothing keeps the coverage without the
        // churn, and when the last empty panel is filled this simply finds
        // nothing to check and says so.
        let dead_panel_command = tui::catalog().find(|entry| {
            let Ok(command) = tui::parse_command(&format!("/{}", entry.name)) else {
                return false;
            };
            match tui::dispatch(command) {
                tui::FrontendAction::Local(tui::LocalAction::Open(inspector)) => inspector
                    .route()
                    .is_some_and(|route| !tui::route_renders_content(route)),
                _ => false,
            }
        });
        match dead_panel_command {
            Some(entry) => assert_eq!(
                availability_of(entry.usage),
                Availability::None,
                "`/{}` opens a panel that paints nothing and must read as unavailable",
                entry.name
            ),
            None => {
                // Every panel-only command now renders something. Nothing to
                // assert, and nothing stale left behind.
            }
        }
        // Goal lifecycle is real; only `budget` has no backend.
        assert_eq!(availability_of(usage("goal")), Availability::Partial);
    }

    #[test]
    fn the_rendered_help_marks_only_what_is_missing() {
        let help = annotated_catalog_help();
        for entry in tui::catalog() {
            let availability = availability_of(entry.usage);
            let expected = format!("{}{}", availability.prefix(), entry.usage);
            assert!(
                help.lines().any(|line| line == expected),
                "`{}` should be listed as `{expected:?}`",
                entry.usage
            );
        }
        assert!(
            help.contains("run a marked command to see exactly what is missing"),
            "the footer must tell the user where the specific reason lives"
        );
    }

    #[test]
    fn every_marker_is_the_same_width_so_the_command_names_stay_aligned() {
        let widths: std::collections::BTreeSet<usize> =
            [Availability::Full, Availability::Partial, Availability::None]
                .into_iter()
                .map(|availability| availability.prefix().len())
                .collect();
        assert_eq!(
            widths.len(),
            1,
            "markers must be one fixed width or the listing stops aligning: {widths:?}"
        );
        assert_eq!(*widths.iter().next().expect("one width"), 4);
    }

    #[test]
    fn the_annotated_help_still_lists_exactly_the_catalog() {
        // The annotation must not become a *third* list. Every line before
        // the footer is one catalog usage plus at most a marker.
        let help = annotated_catalog_help();
        let listed: Vec<&str> = help
            .lines()
            .take_while(|line| !line.trim().is_empty())
            .collect();
        let expected: Vec<&str> = tui::catalog().map(|entry| entry.usage).collect();
        assert_eq!(listed.len(), expected.len());
        // The marker is a fixed-width leading prefix, so the remainder of
        // every line must be exactly the catalog's own usage string.
        const PREFIX_WIDTH: usize = 4;
        for (line, usage) in listed.iter().zip(expected) {
            assert!(
                line.len() > PREFIX_WIDTH,
                "a help line is a marker prefix plus a usage: {line:?}"
            );
            assert_eq!(
                &line[PREFIX_WIDTH..],
                usage,
                "a help line must be exactly one catalog usage plus its marker"
            );
        }
    }
}
