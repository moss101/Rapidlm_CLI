let meta = #{
    name: "rapidlm-v2-implement",
    description: "Orchestrate RapidLM V2 implementation from the repository dossier: AGENTS.md governs, Skills.md selects methodology, docs/04-Task-Breakdown.md is the DAG, docs/prompts.md is the exact task prompt. Fresh context per task. No prompt rewriting.",
    when_to_use: "Implement, verify, repair, or audit RapidLM from the existing 320-task dossier without creating a new plan",
    phases: [
        #{ title: "Preflight", detail: "read-only authority, DAG, and prompt mapping audit" },
        #{ title: "Schedule", detail: "select dependency-ready tasks from the dossier DAG" },
        #{ title: "Implement", detail: "fresh isolated implementation workers" },
        #{ title: "Verify", detail: "fresh independent acceptance verifiers" },
        #{ title: "Repair", detail: "fresh repair workers plus a new verifier" },
        #{ title: "Integrate", detail: "apply only verifier-passed worktrees" },
        #{ title: "Phase-gate", detail: "fresh phase integration verifier" },
    ],
};

// Orchestration only. Do not embed the 320 task prompts or a second plan.
// Authority: AGENTS.md > Skills.md > 04-Task-Breakdown.md > prompts.md > listed refs.
// Durable memory: repository state + structured ledger. Conversation is discarded.

let authority_schema = #{
    "type": "object",
    "properties": #{
        "agents_md_exists": #{ "type": "boolean" },
        "agents_md_readable": #{ "type": "boolean" },
        "agents_md_path": #{ "type": "string" },
        "skills_md_exists": #{ "type": "boolean" },
        "skills_md_readable": #{ "type": "boolean" },
        "skills_md_path": #{ "type": "string" },
        "skills_index_status": #{ "type": "string" },
        "discovered_skills": #{ "type": "array", "maxItems": 24, "items": #{ "type": "string" } },
        "readme_status": #{ "type": "string" },
        "nested_agents_md": #{ "type": "array", "maxItems": 12, "items": #{ "type": "string" } },
        "implementation_source_present": #{ "type": "boolean" },
        "files_changed_by_auditor": #{ "type": "array", "maxItems": 8, "items": #{ "type": "string" } },
        "notes": #{ "type": "string" },
    },
};

let dag_schema = #{
    "type": "object",
    "properties": #{
        "breakdown_exists": #{ "type": "boolean" },
        "prompts_exists": #{ "type": "boolean" },
        "task_count_breakdown": #{ "type": "integer" },
        "task_count_prompts": #{ "type": "integer" },
        "expected_task_count": #{ "type": "integer" },
        "ids_unique": #{ "type": "boolean" },
        "mapping_complete": #{ "type": "boolean" },
        "missing_prompt_ids": #{ "type": "array", "maxItems": 16, "items": #{ "type": "string" } },
        "unresolved_dependencies": #{ "type": "array", "maxItems": 16, "items": #{ "type": "string" } },
        "phases": #{
            "type": "array", "maxItems": 20,
            "items": #{
                "type": "object",
                "properties": #{
                    "id": #{ "type": "string" },
                    "title": #{ "type": "string" },
                    "task_count": #{ "type": "integer" },
                },
            },
        },
        "first_task_id": #{ "type": "string" },
        "last_task_id": #{ "type": "string" },
        "sample_ready_task_ids": #{ "type": "array", "maxItems": 8, "items": #{ "type": "string" } },
        "manifest_used_as_crosscheck": #{ "type": "boolean" },
        "manifest_agrees": #{ "type": "boolean" },
        "notes": #{ "type": "string" },
    },
};

let schedule_schema = #{
    "type": "object",
    "properties": #{
        "phase_id": #{ "type": "string" },
        "phase_complete": #{ "type": "boolean" },
        "ready_tasks": #{
            "type": "array", "maxItems": 6,
            "items": #{
                "type": "object",
                "properties": #{
                    "task_id": #{ "type": "string" },
                    "title": #{ "type": "string" },
                    "phase_id": #{ "type": "string" },
                    "dependencies": #{ "type": "array", "maxItems": 12, "items": #{ "type": "string" } },
                    "deliverable": #{ "type": "string" },
                    "write_scope": #{ "type": "array", "maxItems": 12, "items": #{ "type": "string" } },
                    "security_sensitive": #{ "type": "boolean" },
                    "already_implemented": #{ "type": "boolean" },
                    "computer_use": #{ "type": "boolean" },
                },
            },
        },
        "blocked_tasks": #{
            "type": "array", "maxItems": 8,
            "items": #{
                "type": "object",
                "properties": #{
                    "task_id": #{ "type": "string" },
                    "reason": #{ "type": "string" },
                },
            },
        },
        "notes": #{ "type": "string" },
    },
};

let impl_schema = #{
    "type": "object",
    "properties": #{
        "task_id": #{ "type": "string" },
        "status": #{ "type": "string" },
        "skills_used": #{ "type": "array", "maxItems": 12, "items": #{ "type": "string" } },
        "changed_files": #{ "type": "array", "maxItems": 24, "items": #{ "type": "string" } },
        "worktree": #{ "type": "string" },
        "tests_run": #{ "type": "array", "maxItems": 12, "items": #{ "type": "string" } },
        "test_results": #{ "type": "array", "maxItems": 12, "items": #{ "type": "string" } },
        "acceptance_evidence": #{ "type": "array", "maxItems": 16, "items": #{ "type": "string" } },
        "residual_risks": #{ "type": "array", "maxItems": 8, "items": #{ "type": "string" } },
        "blocker": #{ "type": "string" },
    },
};

let verify_schema = #{
    "type": "object",
    "properties": #{
        "task_id": #{ "type": "string" },
        "verification": #{ "type": "string" },
        "criteria": #{
            "type": "array", "maxItems": 16,
            "items": #{
                "type": "object",
                "properties": #{
                    "criterion": #{ "type": "string" },
                    "status": #{ "type": "string" },
                    "evidence": #{ "type": "string" },
                },
            },
        },
        "tests_run": #{ "type": "array", "maxItems": 12, "items": #{ "type": "string" } },
        "security_findings": #{ "type": "array", "maxItems": 12, "items": #{ "type": "string" } },
        "required_repairs": #{ "type": "array", "maxItems": 12, "items": #{ "type": "string" } },
    },
};

let integrate_schema = #{
    "type": "object",
    "properties": #{
        "integrated_tasks": #{ "type": "array", "maxItems": 6, "items": #{ "type": "string" } },
        "skipped_tasks": #{ "type": "array", "maxItems": 6, "items": #{ "type": "string" } },
        "changed_files": #{ "type": "array", "maxItems": 40, "items": #{ "type": "string" } },
        "ledger_path": #{ "type": "string" },
        "notes": #{ "type": "string" },
    },
};

let phase_gate_schema = #{
    "type": "object",
    "properties": #{
        "phase_id": #{ "type": "string" },
        "verification": #{ "type": "string" },
        "checks": #{
            "type": "array", "maxItems": 16,
            "items": #{
                "type": "object",
                "properties": #{
                    "check": #{ "type": "string" },
                    "status": #{ "type": "string" },
                    "evidence": #{ "type": "string" },
                },
            },
        },
        "notes": #{ "type": "string" },
    },
};

fn trimmed(s) {
    if type_of(s) == "string" {
        s.trim();
        s
    } else {
        ""
    }
}

fn as_str(x) {
    if type_of(x) == "string" { x } else { "" }
}

fn as_num(x) {
    let t = type_of(x);
    if t == "i64" { x } else if t == "f64" { x } else { 0 }
}

fn as_bool(x) {
    x == true
}

fn yn(b) {
    if b == true { "PASS" } else { "FAIL" }
}

fn str_list(x) {
    let out = [];
    if type_of(x) == "array" {
        for item in x {
            if type_of(item) == "string" {
                let s = item;
                s.trim();
                if s != "" { out.push(s); }
            }
        }
    }
    out
}

fn valid_task_id(id) {
    let s = as_str(id);
    if s.len() < 8 { false } else if s.sub_string(0, 4) == "RLM-" { true } else { false }
}

fn security_from_id(task_id) {
    let s = as_str(task_id);
    if s.contains("sandbox") { true }
    else if s.contains("policy") { true }
    else if s.contains("computer") { true }
    else if s.contains("handoff") { true }
    else if s.contains("protocol") { true }
    else if s.contains("daemon") { true }
    else if s.contains("remote") { true }
    else if s.contains("trajectory") { true }
    else if s.contains("foundation-010") { true }
    else if s.contains("foundation-011") { true }
    else if s.contains("foundation-008") { true }
    else if s.contains("foundation-009") { true }
    else { false }
}

fn clamp_parallel(n) {
    let v = as_num(n);
    if v < 1 { 1 } else if v > 6 { 6 } else { v }
}

fn mode_ok(mode) {
    if mode == "audit" { true }
    else if mode == "build" { true }
    else if mode == "verify" { true }
    else if mode == "repair" { true }
    else { false }
}

fn authority_prompt() {
    let p = "";
    p += "You are a FRESH read-only RapidLM preflight auditor. This is a new context.\n";
    p += "DO NOT implement RapidLM. DO NOT edit, create, delete, or patch any file.\n";
    p += "Use only read_file, list_dir, and grep. If a write tool is available, do not use it.\n";
    p += "Do not create crates, apps, tests, or product source. Do not create a worktree.\n\n";
    p += "Read these paths with tools and report only what you actually observed:\n";
    p += "1. AGENTS.md at the repository root. Confirm it exists and is readable. Quote its mission sentence.\n";
    p += "2. Skills.md at the repository root. Confirm it exists and is readable.\n";
    p += "   If it redirects to another file, follow that redirect once and inspect the target.\n";
    p += "   List skill names only if the index actually names them. Do not invent skills.\n";
    p += "   Set skills_index_status to one of: index, redirect-only, missing, incomplete.\n";
    p += "3. README.md at the repository root. If missing, report what orientation file exists instead (for example 00-README.md).\n";
    p += "4. Search for nested AGENTS.md files. Report paths. Do not read every design doc.\n";
    p += "5. Inspect whether product implementation source exists (crates/, apps/, Cargo.toml). Report presence only.\n";
    p += "6. Confirm you changed no files. files_changed_by_auditor must be empty if you made no edits.\n\n";
    p += "Do not read docs/04-Task-Breakdown.md or docs/prompts.md in depth; another auditor owns the DAG.\n";
    p += "A valid empty discovered_skills list requires that you actually read Skills.md and found no skill entries.\n";
    p
}

fn dag_prompt() {
    let p = "";
    p += "You are a FRESH read-only RapidLM DAG auditor. This is a new context.\n";
    p += "DO NOT implement RapidLM. DO NOT edit any file. Use only read_file, list_dir, and grep.\n\n";
    p += "Canonical sources:\n";
    p += "- docs/04-Task-Breakdown.md is the task inventory and dependency DAG.\n";
    p += "- docs/prompts.md is the exact implementation prompt for each task ID.\n";
    p += "- docs/task-manifest.json may be used ONLY as a machine-readable cross-check.\n";
    p += "  If the manifest disagrees with 04-Task-Breakdown.md, the markdown breakdown wins.\n";
    p += "- docs/05-prompts.md is a pointer, not a second prompt library.\n\n";
    p += "Required checks:\n";
    p += "1. Confirm docs/04-Task-Breakdown.md exists. Count unique task IDs matching RLM-*.\n";
    p += "2. Confirm docs/prompts.md exists. Count unique headings matching ## RLM-*.\n";
    p += "3. Expected task count is 320. Report actual counts.\n";
    p += "4. Prove mapping: every breakdown task ID has a prompts.md heading. Do not read all 320 prompts.\n";
    p += "   Use grep for headings. Sample the first task, one middle task, and the last task by reading only those sections.\n";
    p += "5. Resolve dependencies from 04-Task-Breakdown.md (or the manifest if it agrees). Report unresolved IDs only.\n";
    p += "6. Extract the phase list from 04-Task-Breakdown.md headings (## 01-foundation through ## 15-...).\n";
    p += "7. List up to 8 task IDs that have no dependencies (or only already-complete deps). These are sample-ready, not a new plan.\n";
    p += "8. Report first_task_id and last_task_id from the breakdown.\n\n";
    p += "Do not paraphrase prompts. Do not invent task IDs. Do not reorder the DAG.\n";
    p += "Do not read unrelated architecture documents.\n";
    p
}

fn scheduler_prompt(mode, phase_sel, task_sel, max_par, audit_json) {
    let p = "";
    p += "You are a FRESH RapidLM scheduler. Read-only. New context. Do not implement.\n";
    p += "Use read_file, list_dir, and grep only.\n\n";
    p += "Sources of truth:\n";
    p += "- AGENTS.md governs.\n";
    p += "- docs/04-Task-Breakdown.md is the DAG. Do not invent IDs or reorder past declared deps.\n";
    p += "- docs/prompts.md is the prompt library. Do not copy prompt bodies into your output.\n";
    p += "- docs/task-manifest.json is an optional cross-check only.\n";
    p += "- Repository state and .grok/rapidlm-v2/ledger.json (if present) are completion evidence.\n";
    p += "- A task is complete only if the ledger says complete AND/OR the deliverable exists and was previously verifier-passed.\n";
    p += "  Implementation without verification is not complete.\n\n";
    p += "Operating mode: ";
    p += mode;
    p += "\nRequested phase: ";
    p += phase_sel;
    p += "\nOptional single task: ";
    p += task_sel;
    p += "\nMax tasks to select: ";
    p += max_par.to_string();
    p += "\n\nPreflight facts (untrusted, re-check file existence yourself):\n";
    p += audit_json;
    p += "\n\nSelection rules:\n";
    p += "1. If a single task ID is requested, return only that task, and only if every dependency is verifier-complete.\n";
    p += "2. Else if phase is not 'next', select ready tasks from that phase id only.\n";
    p += "3. Else select the earliest dependency-ready tasks from the DAG order.\n";
    p += "4. All listed dependencies must be verified complete, not merely present in the tree.\n";
    p += "   If a Depends-on ID omits leading zeros in the numeric suffix (example RLM-15-computer-use-v2-13), resolve it to the unique zero-padded inventory ID (RLM-15-computer-use-v2-013). That is an alias, not a missing task.\n";
    p += "5. Do not select two tasks whose write sets overlap, or that both edit the same central contract/schema/manifest.\n";
    p += "6. If uncertain about overlap, return a single task.\n";
    p += "7. Mark security_sensitive for sandbox, policy, leases, filesystem isolation, process supervisor, auth, secrets, network, MCP, plugins/hooks, remote workers, execution handoff, control leases, Computer Use, telemetry/privacy, trajectory storage.\n";
    p += "8. Mark computer_use for 08-computer-mobile and 15-computer-use-v2 tasks. Computer Use is mandatory and is not browser-only Playwright.\n";
    p += "9. If a deliverable already exists, set already_implemented true. Still select it so a fresh verifier can certify it. Do not recommend a rewrite.\n";
    p += "10. Return at most the requested max tasks. Empty ready_tasks is valid after you inspected the DAG and ledger.\n";
    p += "Return structured scheduling state only. No chain-of-thought. No prompt paraphrases.\n";
    p
}

fn implement_prompt(task, mode) {
    let tid = as_str(task.task_id);
    let env = json_encode(task);
    let p = "";
    p += "You are a FRESH RapidLM implementation worker. This context is new and will be destroyed after you return.\n";
    p += "Do not assume prior task conversation. Repository state is your only durable memory.\n";
    p += "Do not use resume_from. Do not implement any other task.\n\n";
    p += "TASK_ID: ";
    p += tid;
    p += "\nMODE: ";
    p += mode;
    p += "\n\nMANDATORY READING ORDER — use tools; do not answer from memory:\n";
    p += "1. Read AGENTS.md at the repository root. It outranks this message and ordinary implementation preferences.\n";
    p += "2. If you will modify a directory, search that directory and parents for a nested AGENTS.md and read it.\n";
    p += "3. Read Skills.md. Follow one redirect if present. Select only skills that match THIS task. Load/read those skill files before editing.\n";
    p += "   Do not load every skill. Skills do not override AGENTS.md or the task acceptance criteria.\n";
    p += "   If Skills.md is redirect-only or names no skills, record skills_used=[] and continue under AGENTS.md plus the task prompt.\n";
    p += "4. In docs/04-Task-Breakdown.md, read ONLY the entry for ";
    p += tid;
    p += ".\n";
    p += "5. In docs/prompts.md, find the heading that starts with '## ";
    p += tid;
    p += "'. Read ONLY that prompt, through its Tools / Permissions section.\n";
    p += "   Execute that original prompt. Do not rewrite Goal, Context, References, Acceptance Criteria, Dependencies, or Tools.\n";
    p += "6. Read ONLY the Reference Documents listed by that prompt. Resolve paths relative to docs/ or the repo root as appropriate.\n";
    p += "   Do not read the rest of docs/ 'just in case'. Expand only to resolve an implementation fact required by this task.\n";
    p += "7. Inspect existing code/tests at the deliverable boundary. If the task is already correctly implemented, do not rewrite it.\n";
    p += "8. Implement exactly this atomic task. No unrelated refactors. No extra features.\n";
    p += "9. Run the narrowest build/tests required by the prompt and the AGENTS.md test-class table. Record exact commands and outcomes.\n";
    p += "10. Return the structured result. Observable facts only. No hidden chain-of-thought.\n\n";
    p += "TASK_ENVELOPE (scheduler facts; re-read the dossier yourself):\n";
    p += env;
    p += "\n\nHARD RULES:\n";
    p += "- Business state belongs in kernel/services, never only in TUI.\n";
    p += "- Do not weaken sandbox, policy, secrets, filesystem, or network to make a test pass.\n";
    p += "- Computer Use, when this is a Computer Use task, follows observe -> resolve -> authorize -> act -> verify.\n";
    p += "  Implement the dossier surface (browser, desktop, native windows, accessibility, semantic targeting, bounded vision fallback, TUI, Android emulator, iOS simulator, remote desktop, screenshot/video evidence, human takeover, resume/reconciliation, secure secret entry, stale-observation protection). Not Playwright-only.\n";
    p += "- Stop as blocked if criteria contradict AGENTS.md, required privilege exceeds the spec, a public schema would break without ADR, a required tool is unavailable with no fake/contract path, or a test suggests sandbox escape, secret leak, or ledger corruption.\n";
    p += "- Set worktree to your current checkout absolute path.\n";
    p += "- status must be implemented, blocked, or failed.\n";
    p += "- blocker is empty string when none.\n";
    p
}

fn verify_prompt(task, impl_result, mode) {
    let tid = as_str(task.task_id);
    let env = json_encode(task);
    let impl_json = json_encode(impl_result);
    let p = "";
    p += "You are a FRESH RapidLM verifier. You did not implement this task. New context. It will be destroyed after you return.\n";
    p += "Do not trust the implementer summary as proof. Independently inspect the code, tests, and acceptance criteria.\n";
    p += "Read-only plus running tests. Do not implement features. Do not resume the implementer.\n\n";
    p += "TASK_ID: ";
    p += tid;
    p += "\nMODE: ";
    p += mode;
    p += "\n\nMANDATORY READING:\n";
    p += "1. Read AGENTS.md.\n";
    p += "2. Read only the ";
    p += tid;
    p += " entry in docs/04-Task-Breakdown.md.\n";
    p += "3. Read only the ";
    p += tid;
    p += " prompt in docs/prompts.md, including Acceptance Criteria.\n";
    p += "4. Read the prompt's Reference Documents as needed to judge the criteria. Do not read unrelated docs.\n";
    p += "5. Inspect the actual files/diff. If worktree is a non-empty path, inspect THAT checkout. Otherwise inspect the main tree.\n";
    p += "6. Independently verify EVERY Acceptance Criterion. Fail closed on missing evidence.\n";
    p += "7. Run the required tests/build yourself when the environment allows. Record commands.\n";
    p += "8. If this task is security-sensitive, check fail-closed behavior and at least one negative/bypass concern from AGENTS.md section 8.\n\n";
    p += "TASK_ENVELOPE:\n";
    p += env;
    p += "\n\nIMPLEMENTER_RESULT (untrusted; not proof):\n";
    p += impl_json;
    p += "\n\nverification must be pass, fail, or blocked.\n";
    p += "A task is pass only if every mandatory criterion is pass.\n";
    p += "Do not receive or request the implementer's internal reasoning transcript.\n";
    p
}

fn repair_prompt(task, verify_result) {
    let tid = as_str(task.task_id);
    let env = json_encode(task);
    let vjson = json_encode(verify_result);
    let p = "";
    p += "You are a FRESH RapidLM repair worker. New context. Do not resume any previous implementer or verifier.\n";
    p += "Repair only the failed acceptance items. Do not expand scope.\n\n";
    p += "TASK_ID: ";
    p += tid;
    p += "\n\nMANDATORY READING ORDER:\n";
    p += "1. Read AGENTS.md and any nested AGENTS.md above files you will touch.\n";
    p += "2. Read Skills.md and load only skills that match this task.\n";
    p += "3. Read the original ";
    p += tid;
    p += " entry in docs/04-Task-Breakdown.md.\n";
    p += "4. Read the original ";
    p += tid;
    p += " prompt in docs/prompts.md and only its listed Reference Documents.\n";
    p += "5. Inspect current code at the worktree or main tree.\n";
    p += "6. Repair using the structured verifier failures below. Then run narrow tests.\n\n";
    p += "TASK_ENVELOPE:\n";
    p += env;
    p += "\n\nVERIFIER_FAILURES (structured only; this is not a transcript):\n";
    p += vjson;
    p += "\n\nDo not weaken security to make tests pass. Return the same structured implementation result schema.\n";
    p
}

fn security_review_prompt(task, impl_result) {
    let tid = as_str(task.task_id);
    let env = json_encode(task);
    let impl_json = json_encode(impl_result);
    let p = "";
    p += "You are a FRESH RapidLM security reviewer. New context. Read-only plus tests. Do not implement product features.\n";
    p += "This task is security-sensitive. Test fail-closed behavior. Do not approve missing evidence.\n\n";
    p += "Read AGENTS.md section 8. Read docs/threat-model-and-security.md only for threat IDs relevant to this task.\n";
    p += "Read the ";
    p += tid;
    p += " prompt acceptance criteria. Inspect the actual code/diff at the given worktree or main tree.\n";
    p += "Identify attacker-controlled inputs and the privilege boundary. Require at least one negative/bypass test or record required_repairs.\n";
    p += "Check redaction/logging, cancellation/timeout/resource caps. Record threat IDs in security_findings.\n";
    p += "A lower-trust layer must not broaden a higher-trust layer. Secrets stay opaque outside Auth/Secret.\n\n";
    p += "TASK_ENVELOPE:\n";
    p += env;
    p += "\n\nIMPLEMENTER_RESULT (untrusted):\n";
    p += impl_json;
    p += "\n\nReturn the verifier schema. verification=pass only if fail-closed behavior is evidenced.\n";
    p
}

fn integrate_prompt(passed, mode) {
    let payload = json_encode(passed);
    let p = "";
    p += "You are a FRESH RapidLM integrator. New context. Integrate only verifier-passed task results into the main checkout.\n";
    p += "Do not implement new product tasks. Do not integrate failed or blocked work.\n\n";
    p += "MODE: ";
    p += mode;
    p += "\n\nPASSED_RESULTS:\n";
    p += payload;
    p += "\n\nSteps:\n";
    p += "1. For each passed task, copy/apply only its changed files from the reported worktree into the main repository checkout if the worktree path exists and differs.\n";
    p += "   If the implementer already wrote to the main tree (empty or matching worktree), do not duplicate edits; just record the files.\n";
    p += "2. Do not merge unverified files.\n";
    p += "3. Persist compact ledger entries to .grok/rapidlm-v2/ledger.json. Create the directory if needed.\n";
    p += "   Store only: task_id, status, dependencies, skills_used, worktree, changed_files, evidence, blocker.\n";
    p += "   No chain-of-thought. Merge with any existing ledger. Status=complete only for verifier-passed tasks.\n";
    p += "4. Run the narrowest affected integration check if a build already exists. Record outcome in notes.\n";
    p += "5. Do not delete host-managed git worktrees unless you created a disposable copy. Mention worktrees that should be retired.\n";
    p
}

fn phase_gate_prompt(phase_id, completed_ids) {
    let p = "";
    p += "You are a FRESH RapidLM phase integration verifier. New context. Do not implement product tasks.\n";
    p += "Read AGENTS.md. Read the matching phase section in docs/04-Task-Breakdown.md. Read docs/reference/definition-of-done.md.\n";
    p += "Read docs/03-Development-Plan-and-Roadmap.md only for this phase's exit gates.\n\n";
    p += "PHASE_ID: ";
    p += as_str(phase_id);
    p += "\nTASKS_CLAIMED_COMPLETE: ";
    p += json_encode(completed_ids);
    p += "\n\nCheck: phase acceptance, integrated build if a workspace exists, relevant tests, lint/typecheck if configured, API/contract consistency, security invariants, task completeness, dependency correctness.\n";
    p += "If no implementation source exists yet, fail the build checks with evidence rather than inventing a pass.\n";
    p += "Return the phase-gate schema. verification=pass only if the phase is actually done.\n";
    p
}

fn join_lines(items) {
    let s = "";
    let i = 0;
    for item in items {
        if i > 0 { s += ", "; }
        s += as_str(item);
        i += 1;
    }
    s
}

fn audit_markdown(auth, dag, mode, max_par, fresh_ok) {
    let md = "";
    md += "# RapidLM V2 workflow preflight\n\n";
    md += "Workflow: `rapidlm-v2-implement`\n\n";
    md += "Mode: ";
    md += mode;
    md += "\n\nThis run is orchestration preflight. Product implementation was not started.\n\n";
    md += "## Absolute invariant\n\n";
    md += "AGENTS.md governs. Skills.md selects methodology. Task-Breakdown defines what work exists and what is ready. ";
    md += "prompts.md defines exactly how each task is executed. Referenced documents provide task-specific design authority. ";
    md += "Repository state and structured verification evidence provide durable memory. ";
    md += "Every implementation, repair, and verification task begins with a fresh context.\n\n";
    md += "## Authority files\n\n";
    md += "| Check | Result |\n|---|---|\n";
    md += "| Root AGENTS.md exists | ";
    md += yn(as_bool(auth.agents_md_exists));
    md += " |\n| Root AGENTS.md readable | ";
    md += yn(as_bool(auth.agents_md_readable));
    md += " |\n| Root Skills.md exists | ";
    md += yn(as_bool(auth.skills_md_exists));
    md += " |\n| Root Skills.md readable | ";
    md += yn(as_bool(auth.skills_md_readable));
    md += " |\n| Skills index status | ";
    md += as_str(auth.skills_index_status);
    md += " |\n| README orientation | ";
    md += as_str(auth.readme_status);
    md += " |\n| Implementation source present | ";
    if as_bool(auth.implementation_source_present) { md += "YES"; } else { md += "NO"; }
    md += " |\n\n";
    md += "AGENTS.md path: `";
    md += as_str(auth.agents_md_path);
    md += "`\n\nSkills.md path: `";
    md += as_str(auth.skills_md_path);
    md += "`\n\nDiscovered skills: ";
    let skills = str_list(auth.discovered_skills);
    if skills.len() == 0 { md += "(none named by Skills.md)\n"; } else {
        md += join_lines(skills);
        md += "\n";
    }
    md += "\nNested AGENTS.md: ";
    let nested = str_list(auth.nested_agents_md);
    if nested.len() == 0 { md += "(none found)\n"; } else {
        md += join_lines(nested);
        md += "\n";
    }
    md += "\nAuditor notes: ";
    md += as_str(auth.notes);
    md += "\n\n## DAG and prompts\n\n";
    md += "| Check | Result |\n|---|---|\n";
    md += "| docs/04-Task-Breakdown.md exists | ";
    md += yn(as_bool(dag.breakdown_exists));
    md += " |\n| docs/prompts.md exists | ";
    md += yn(as_bool(dag.prompts_exists));
    md += " |\n| Breakdown task count | ";
    md += as_num(dag.task_count_breakdown).to_string();
    md += " |\n| Prompt heading count | ";
    md += as_num(dag.task_count_prompts).to_string();
    md += " |\n| Expected task count | ";
    md += as_num(dag.expected_task_count).to_string();
    md += " |\n| IDs unique | ";
    md += yn(as_bool(dag.ids_unique));
    md += " |\n| Every task maps to a prompt | ";
    md += yn(as_bool(dag.mapping_complete));
    md += " |\n| Manifest used as cross-check | ";
    md += yn(as_bool(dag.manifest_used_as_crosscheck));
    md += " |\n| Manifest agrees with breakdown | ";
    md += yn(as_bool(dag.manifest_agrees));
    md += " |\n| First task | ";
    md += as_str(dag.first_task_id);
    md += " |\n| Last task | ";
    md += as_str(dag.last_task_id);
    md += " |\n\nMissing prompt IDs: ";
    let missing = str_list(dag.missing_prompt_ids);
    if missing.len() == 0 { md += "(none reported)\n"; } else {
        md += join_lines(missing);
        md += "\n";
    }
    md += "\nUnresolved dependencies: ";
    let unresolved = str_list(dag.unresolved_dependencies);
    if unresolved.len() == 0 { md += "(none reported)\n"; } else {
        md += join_lines(unresolved);
        md += "\n";
    }
    md += "\nSample dependency-ready task IDs: ";
    let ready = str_list(dag.sample_ready_task_ids);
    if ready.len() == 0 { md += "(none reported)\n"; } else {
        md += join_lines(ready);
        md += "\n";
    }
    md += "\n### Phases parsed from 04-Task-Breakdown.md\n\n";
    if type_of(dag.phases) == "array" {
        for ph in dag.phases {
            if ph != () {
                md += "- `";
                md += as_str(ph.id);
                md += "` ";
                md += as_str(ph.title);
                md += " (";
                md += as_num(ph.task_count).to_string();
                md += " tasks)\n";
            }
        }
    } else {
        md += "(phase list unavailable in this result)\n";
    }
    md += "\nDAG notes: ";
    md += as_str(dag.notes);
    md += "\n\n## Orchestration capability\n\n";
    md += "| Check | Result |\n|---|---|\n";
    md += "| Fresh subagent contexts used (no resume_from) | ";
    md += yn(fresh_ok);
    md += " |\n| Worktree isolation available for parallel writers | PASS |\n";
    md += "| Default max_parallel | ";
    md += max_par.to_string();
    md += " (absolute max 6) |\n| Implementation source changed by this preflight | ";
    let changed = str_list(auth.files_changed_by_auditor);
    if changed.len() == 0 { md += "PASS (none) |\n"; } else {
        md += "FAIL (";
        md += join_lines(changed);
        md += ") |\n";
    }
    md += "\n## How later runs execute a task\n\n";
    md += "1. Parse the DAG from docs/04-Task-Breakdown.md.\n";
    md += "2. Select dependency-ready tasks with non-overlapping write sets.\n";
    md += "3. Create a FRESH implementer (worktree isolation when parallel). Worker reads AGENTS.md, Skills.md, matching task, matching prompt, listed refs only.\n";
    md += "4. Destroy implementer context after the structured result.\n";
    md += "5. Create a FRESH verifier. Independent acceptance check. Destroy afterward.\n";
    md += "6. On fail: FRESH repair agent, then another FRESH verifier. Never resume the original implementer.\n";
    md += "7. Integrate only verifier-passed results. Persist compact ledger facts.\n";
    md += "8. At phase end, a FRESH integration verifier checks the phase gate.\n";
    md += "\nTo implement after this audit, re-run with `mode: \"build\"`.\n";
    md
}

fn empty_impl(task_id) {
    #{
        task_id: as_str(task_id),
        status: "failed",
        skills_used: [],
        changed_files: [],
        worktree: "",
        tests_run: [],
        test_results: [],
        acceptance_evidence: [],
        residual_risks: ["implementer returned no usable structured result"],
        blocker: "implementer_unusable",
    }
}

fn empty_verify(task_id) {
    #{
        task_id: as_str(task_id),
        verification: "fail",
        criteria: [],
        tests_run: [],
        security_findings: [],
        required_repairs: ["verifier returned no usable structured result; fail closed"],
    }
}

fn impl_from(out, fallback_id) {
    if out == () { empty_impl(fallback_id) }
    else if out.task_id == () { empty_impl(fallback_id) }
    else { out }
}

fn verify_from(out, fallback_id) {
    if out == () { empty_verify(fallback_id) }
    else if out.verification == () { empty_verify(fallback_id) }
    else { out }
}

// --- args ---
let mode = "audit";
let phase_sel = "next";
let task_sel = "";
let max_par = 4;
let verification = "task";

if args != () {
    if args.mode != () {
        let m = trimmed(as_str(args.mode));
        if m != "" { mode = m; }
    }
    if args.phase != () {
        let ph = trimmed(as_str(args.phase));
        if ph != "" { phase_sel = ph; }
    }
    if args.task != () {
        let tk = trimmed(as_str(args.task));
        if tk != "" { task_sel = tk; }
    }
    if args.max_parallel != () { max_par = clamp_parallel(args.max_parallel); }
    if args.verification != () {
        let vf = trimmed(as_str(args.verification));
        if vf != "" { verification = vf; }
    }
}
max_par = clamp_parallel(max_par);

if !mode_ok(mode) {
    pause("verification", "args.mode must be audit, build, verify, or repair");
}

log("rapidlm-v2-implement mode=" + mode + " phase=" + phase_sel + " max_parallel=" + max_par.to_string());

// --- Preflight: always read-only, always fresh contexts, never implements ---
phase("Preflight");
log("Launching two fresh read-only preflight auditors");

let preflight_jobs = [];
preflight_jobs.push(#{
    prompt: authority_prompt(),
    label: "preflight-authority",
    capability_mode: "read-only",
    output_schema: authority_schema,
});
preflight_jobs.push(#{
    prompt: dag_prompt(),
    label: "preflight-dag",
    capability_mode: "read-only",
    output_schema: dag_schema,
});
let preflight = parallel(preflight_jobs);

let auth = #{};
let dag = #{};
if preflight.len() > 0 && preflight[0] != () && preflight[0].success && preflight[0].output != () {
    auth = preflight[0].output;
}
if preflight.len() > 1 && preflight[1] != () && preflight[1].success && preflight[1].output != () {
    dag = preflight[1].output;
}

let fresh_ok = preflight.len() == 2;
let agents_ok = as_bool(auth.agents_md_exists) && as_bool(auth.agents_md_readable);
let skills_ok = as_bool(auth.skills_md_exists) && as_bool(auth.skills_md_readable);
let breakdown_ok = as_bool(dag.breakdown_exists);
let prompts_ok = as_bool(dag.prompts_exists);
let mapping_ok = as_bool(dag.mapping_complete);
let ids_ok = as_bool(dag.ids_unique);
let no_impl_edits = str_list(auth.files_changed_by_auditor).len() == 0;
let count_ok = as_num(dag.task_count_breakdown) > 0 && as_num(dag.task_count_prompts) > 0;
let deps_ok = str_list(dag.unresolved_dependencies).len() == 0;

let audit_pass = agents_ok && skills_ok && breakdown_ok && prompts_ok && mapping_ok && ids_ok && no_impl_edits && fresh_ok && count_ok;

let report = audit_markdown(auth, dag, mode, max_par, fresh_ok);
let report_path = write_scratch_file("audit-report.md", report);

let audit_result = #{
    workflow: "rapidlm-v2-implement",
    mode: mode,
    path: report_path,
    implementation_started: false,
    product_source_changed: !no_impl_edits,
    audit_pass: audit_pass,
    checks: #{
        agents_md: agents_ok,
        skills_md: skills_ok,
        task_breakdown: breakdown_ok,
        prompts: prompts_ok,
        task_ids_parsed: count_ok,
        prompt_mapping: mapping_ok,
        dependencies_resolved: deps_ok,
        skills_discoverable: skills_ok,
        fresh_subagent_contexts: fresh_ok,
        worktree_isolation_available: true,
        no_implementation_source_changed: no_impl_edits,
    },
    task_count_breakdown: as_num(dag.task_count_breakdown),
    task_count_prompts: as_num(dag.task_count_prompts),
    first_task_id: as_str(dag.first_task_id),
    last_task_id: as_str(dag.last_task_id),
    sample_ready_task_ids: str_list(dag.sample_ready_task_ids),
    skills_index_status: as_str(auth.skills_index_status),
    readme_status: as_str(auth.readme_status),
};

if mode == "audit" {
    log("Audit-only run complete. implementation_started=false");
    complete(audit_result);
}

if !audit_pass {
    pause("verification", "Preflight failed. See scratch/audit-report.md. Refusing to implement until AGENTS.md, Skills.md, the task DAG, and prompt mapping are usable.");
}

// --- Schedule: fresh read-only scheduler, compact state only ---
phase("Schedule");
let audit_json = json_encode(#{
    audit_pass: audit_pass,
    first_task_id: as_str(dag.first_task_id),
    last_task_id: as_str(dag.last_task_id),
    sample_ready_task_ids: str_list(dag.sample_ready_task_ids),
    skills_index_status: as_str(auth.skills_index_status),
});

let b0 = budget();
if b0.remaining < 6 {
    pause("infra", "Insufficient agent budget remaining after preflight for a build/verify/repair batch.");
}

log("Launching fresh scheduler");
let sched_res = agent(
    scheduler_prompt(mode, phase_sel, task_sel, max_par, audit_json),
    #{ label: "schedule", capability_mode: "read-only", output_schema: schedule_schema },
);

let ready = [];
let phase_id = phase_sel;
let phase_complete = false;
if sched_res != () && sched_res.success && sched_res.output != () {
    if sched_res.output.phase_id != () { phase_id = as_str(sched_res.output.phase_id); }
    phase_complete = as_bool(sched_res.output.phase_complete);
    if type_of(sched_res.output.ready_tasks) == "array" {
        for t in sched_res.output.ready_tasks {
            if t != () && valid_task_id(t.task_id) { ready.push(t); }
        }
    }
}

if ready.len() > max_par {
    log("Scheduler returned extra tasks; truncating to max_parallel");
    let clipped = [];
    let ci = 0;
    for t in ready {
        if ci < max_par { clipped.push(t); }
        ci += 1;
    }
    ready = clipped;
}

// Reserve worst-case: impl + verify + optional security + repair + re-verify, plus integrate/gate.
let b1 = budget();
let per_task = 5;
let reserve = 2;
let room = b1.remaining - reserve;
let max_by_budget = 0;
if room >= per_task { max_by_budget = room / per_task; }
if max_by_budget < ready.len() {
    let clipped2 = [];
    let cj = 0;
    for t in ready {
        if cj < max_by_budget { clipped2.push(t); }
        cj += 1;
    }
    ready = clipped2;
}

if ready.len() == 0 {
    log("No dependency-ready tasks selected");
    complete(#{
        workflow: "rapidlm-v2-implement",
        mode: mode,
        path: report_path,
        implementation_started: false,
        reason: "no_ready_tasks",
        phase_id: phase_id,
        phase_complete: phase_complete,
        ledger: [],
    });
}

// --- Implement: one FRESH worker per task. Never resume_from. Isolate parallel writers. ---
phase("Implement");
let isolate = ready.len() > 1;
let impl_jobs = [];
for t in ready {
    let cap = "all";
    if mode == "verify" { cap = "read-only"; }
    impl_jobs.push(#{
        prompt: implement_prompt(t, mode),
        label: "impl:" + as_str(t.task_id),
        capability_mode: cap,
        isolation_worktree: isolate,
        output_schema: impl_schema,
    });
}
log("Launching " + impl_jobs.len().to_string() + " fresh implementers; isolation_worktree=" + isolate.to_string());
let impl_panel = parallel(impl_jobs);

let impls = [];
let ii = 0;
for t in ready {
    let slot = ();
    if ii < impl_panel.len() { slot = impl_panel[ii]; }
    if slot != () && slot.success && slot.output != () {
        impls.push(impl_from(slot.output, t.task_id));
    } else {
        impls.push(empty_impl(t.task_id));
    }
    ii += 1;
}

// --- Verify: FRESH verifier per task. Security reviewer when required. ---
phase("Verify");
let verify_jobs = [];
let vmap = [];
let vi = 0;
for t in ready {
    // execute: run cargo/tests. Do not use read-only; cargo writes target/ and Shell is denied.
    verify_jobs.push(#{
        prompt: verify_prompt(t, impls[vi], mode),
        label: "verify:" + as_str(t.task_id),
        capability_mode: "execute",
        output_schema: verify_schema,
    });
    vmap.push(#{ kind: "verify", index: vi });
    let sec = as_bool(t.security_sensitive) || security_from_id(t.task_id);
    if sec {
        verify_jobs.push(#{
            prompt: security_review_prompt(t, impls[vi]),
            label: "security:" + as_str(t.task_id),
            capability_mode: "execute",
            output_schema: verify_schema,
        });
        vmap.push(#{ kind: "security", index: vi });
    }
    vi += 1;
}
log("Launching " + verify_jobs.len().to_string() + " fresh verifiers");
let verify_panel = parallel(verify_jobs);

let verdicts = [];
for t in ready { verdicts.push(empty_verify(t.task_id)); }
let sec_verdicts = [];
for t in ready { sec_verdicts.push(()); }

let vk = 0;
for slot in verify_panel {
    if vk < vmap.len() {
        let meta_v = vmap[vk];
        let idx = meta_v.index;
        if slot != () && slot.success && slot.output != () {
            if meta_v.kind == "security" {
                sec_verdicts[idx] = verify_from(slot.output, ready[idx].task_id);
            } else {
                verdicts[idx] = verify_from(slot.output, ready[idx].task_id);
            }
        }
    }
    vk += 1;
}

// Fail closed if a required security reviewer failed.
let vs = 0;
for t in ready {
    let need_sec = as_bool(t.security_sensitive) || security_from_id(t.task_id);
    if need_sec {
        let sv = sec_verdicts[vs];
        if sv == () {
            verdicts[vs].verification = "fail";
            verdicts[vs].required_repairs.push("security reviewer produced no usable result; fail closed");
        } else if as_str(sv.verification) != "pass" {
            verdicts[vs].verification = "fail";
            let repairs = str_list(sv.required_repairs);
            for r in repairs { verdicts[vs].required_repairs.push(r); }
        }
    }
    vs += 1;
}

// --- Repair: FRESH repair agent + FRESH verifier. Never resume the implementer. ---
phase("Repair");
let repair_idx = [];
let repair_jobs = [];
if mode != "verify" {
    let ri = 0;
    for t in ready {
        let vstat = as_str(verdicts[ri].verification);
        let istat = as_str(impls[ri].status);
        if vstat == "fail" && istat != "blocked" {
            repair_idx.push(ri);
            repair_jobs.push(#{
                prompt: repair_prompt(t, verdicts[ri]),
                label: "repair:" + as_str(t.task_id),
                capability_mode: "all",
                isolation_worktree: isolate,
                output_schema: impl_schema,
            });
        }
        ri += 1;
    }
}

if repair_jobs.len() > 0 {
    log("Launching " + repair_jobs.len().to_string() + " fresh repair workers");
    let repair_panel = parallel(repair_jobs);
    let rk = 0;
    for slot in repair_panel {
        let idx = repair_idx[rk];
        if slot != () && slot.success && slot.output != () {
            impls[idx] = impl_from(slot.output, ready[idx].task_id);
        }
        rk += 1;
    }

    let re_jobs = [];
    for idx in repair_idx {
        re_jobs.push(#{
            prompt: verify_prompt(ready[idx], impls[idx], mode),
            label: "reverify:" + as_str(ready[idx].task_id),
            capability_mode: "execute",
            output_schema: verify_schema,
        });
    }
    log("Launching " + re_jobs.len().to_string() + " fresh post-repair verifiers");
    let re_panel = parallel(re_jobs);
    let rk2 = 0;
    for slot in re_panel {
        let idx = repair_idx[rk2];
        if slot != () && slot.success && slot.output != () {
            verdicts[idx] = verify_from(slot.output, ready[idx].task_id);
        } else {
            verdicts[idx] = empty_verify(ready[idx].task_id);
        }
        rk2 += 1;
    }
}

// --- Integrate only verifier-passed work ---
phase("Integrate");
let passed = [];
let ledger = [];
let li = 0;
for t in ready {
    let vstat = as_str(verdicts[li].verification);
    let status = "failed";
    if vstat == "pass" { status = "complete"; }
    else if vstat == "blocked" { status = "blocked"; }
    else if as_str(impls[li].status) == "blocked" { status = "blocked"; }

    let entry = #{
        task_id: as_str(t.task_id),
        status: status,
        dependencies: str_list(t.dependencies),
        skills_used: str_list(impls[li].skills_used),
        worktree: as_str(impls[li].worktree),
        changed_files: str_list(impls[li].changed_files),
        evidence: str_list(impls[li].acceptance_evidence),
        blocker: as_str(impls[li].blocker),
        verification: vstat,
    };
    ledger.push(entry);
    if status == "complete" {
        passed.push(#{
            task_id: as_str(t.task_id),
            worktree: as_str(impls[li].worktree),
            changed_files: str_list(impls[li].changed_files),
            dependencies: str_list(t.dependencies),
            skills_used: str_list(impls[li].skills_used),
            evidence: str_list(impls[li].acceptance_evidence),
        });
    }
    li += 1;
}

let ledger_path = write_scratch_file("ledger.json", json_encode(ledger));

if passed.len() > 0 {
    log("Integrating " + passed.len().to_string() + " verifier-passed tasks");
    let integ = agent(
        integrate_prompt(passed, mode),
        #{ label: "integrate", capability_mode: "all", output_schema: integrate_schema },
    );
    if integ != () && integ.success && integ.output != () {
        if as_str(integ.output.ledger_path) != "" { ledger_path = as_str(integ.output.ledger_path); }
    }
} else {
    log("No verifier-passed tasks to integrate");
}

// --- Phase gate when requested or when scheduler says the phase is complete ---
phase("Phase-gate");
let run_gate = verification == "phase" || phase_complete;
if run_gate {
    let completed_ids = [];
    for e in ledger {
        if as_str(e.status) == "complete" { completed_ids.push(as_str(e.task_id)); }
    }
    log("Launching fresh phase-gate verifier for " + phase_id);
    let gate = agent(
        phase_gate_prompt(phase_id, completed_ids),
        #{ label: "phase-gate:" + phase_id, capability_mode: "execute", output_schema: phase_gate_schema },
    );
    let gate_status = "unavailable";
    if gate != () && gate.success && gate.output != () { gate_status = as_str(gate.output.verification); }
    complete(#{
        workflow: "rapidlm-v2-implement",
        mode: mode,
        path: report_path,
        ledger_path: ledger_path,
        implementation_started: mode == "build" || mode == "repair",
        phase_id: phase_id,
        phase_gate: gate_status,
        ledger: ledger,
    });
}

complete(#{
    workflow: "rapidlm-v2-implement",
    mode: mode,
    path: report_path,
    ledger_path: ledger_path,
    implementation_started: mode == "build" || mode == "repair",
    phase_id: phase_id,
    ledger: ledger,
});
