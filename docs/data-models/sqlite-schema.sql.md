# SQLite Persistence Schema

The SQL below is the normative logical schema; migrations may add indices/internal columns without changing public contracts.

```sql
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

CREATE TABLE sessions (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  closed_at TEXT
);

CREATE TABLE events (
  session_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  event_id TEXT NOT NULL UNIQUE,
  recorded_at TEXT NOT NULL,
  actor_json TEXT NOT NULL,
  trace_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  redaction TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  PRIMARY KEY(session_id, seq),
  FOREIGN KEY(session_id) REFERENCES sessions(id)
);

CREATE TABLE checkpoints (
  session_id TEXT NOT NULL,
  through_seq INTEGER NOT NULL,
  projection_schema INTEGER NOT NULL,
  artifact_id TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(session_id, through_seq)
);

CREATE TABLE artifacts (
  id TEXT PRIMARY KEY,
  media_type TEXT NOT NULL,
  bytes INTEGER NOT NULL,
  redaction TEXT NOT NULL,
  created_at TEXT NOT NULL,
  expires_at TEXT
);

CREATE TABLE capability_audit (
  lease_id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  agent_id TEXT,
  action_hash BLOB NOT NULL,
  capability TEXT NOT NULL,
  resource_scope_json TEXT NOT NULL,
  decision TEXT NOT NULL,
  policy_revision TEXT NOT NULL,
  issued_at TEXT NOT NULL,
  expires_at TEXT
);

CREATE TABLE memories (
  id TEXT PRIMARY KEY,
  scope TEXT NOT NULL,
  project_id TEXT,
  source_json TEXT NOT NULL,
  confidence REAL NOT NULL,
  content TEXT NOT NULL,
  created_at TEXT NOT NULL,
  expires_at TEXT
);

CREATE TABLE context_chunks (
  id TEXT PRIMARY KEY,
  repo_id TEXT NOT NULL,
  path TEXT NOT NULL,
  start_byte INTEGER NOT NULL,
  end_byte INTEGER NOT NULL,
  language TEXT,
  content_hash TEXT NOT NULL,
  text TEXT NOT NULL,
  symbol_json TEXT,
  indexed_at TEXT NOT NULL
);

CREATE VIRTUAL TABLE context_fts USING fts5(
  chunk_id UNINDEXED,
  path,
  symbols,
  text,
  tokenize='unicode61'
);
```

Vector embeddings MAY live in a separate rebuildable index keyed by `context_chunks.id`; absence/corruption of that index must not make source retrieval unavailable.

## V2 persistence extensions

The following tables are additive V2 projections/stores. The Event Ledger remains the authoritative history for session state; mutable registries (Knowledge/Playbooks/experiments) carry their own revision/provenance fields.

```sql
CREATE TABLE agent_pool (
  session_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  parent_agent_id TEXT,
  class TEXT NOT NULL,                 -- background | managed
  role TEXT NOT NULL,
  state TEXT NOT NULL,
  workspace_view_id TEXT,
  budget_json TEXT NOT NULL,
  last_mail_cursor INTEGER NOT NULL DEFAULT 0,
  private_state_artifact_id TEXT,
  updated_at TEXT NOT NULL,
  PRIMARY KEY(session_id, agent_id)
);

CREATE TABLE agent_mail (
  session_id TEXT NOT NULL,
  cursor INTEGER NOT NULL,
  message_id TEXT NOT NULL UNIQUE,
  from_agent_id TEXT NOT NULL,
  to_agent_id TEXT NOT NULL,
  topic TEXT NOT NULL,
  body_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(session_id, cursor)
);

CREATE TABLE session_execution_leases (
  session_id TEXT PRIMARY KEY,
  generation INTEGER NOT NULL,
  owner_runtime_id TEXT NOT NULL,
  state TEXT NOT NULL,                 -- active | handoff_pending | parked
  lease_digest TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE control_leases (
  session_id TEXT NOT NULL,
  domain TEXT NOT NULL,
  generation INTEGER NOT NULL,
  holder_json TEXT NOT NULL,
  acquired_at TEXT NOT NULL,
  expires_at TEXT,
  state TEXT NOT NULL,
  PRIMARY KEY(session_id, domain)
);

CREATE TABLE knowledge_items (
  id TEXT PRIMARY KEY,
  revision INTEGER NOT NULL,
  scope_json TEXT NOT NULL,
  trigger_json TEXT NOT NULL,
  content TEXT NOT NULL,
  provenance_json TEXT NOT NULL,
  owner_json TEXT NOT NULL,
  confidence REAL,
  freshness_json TEXT,
  status TEXT NOT NULL,                -- candidate | active | superseded | invalid
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE playbooks (
  id TEXT NOT NULL,
  version INTEGER NOT NULL,
  spec_json TEXT NOT NULL,
  digest TEXT NOT NULL,
  status TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(id, version)
);

CREATE TABLE automation_state (
  automation_id TEXT PRIMARY KEY,
  playbook_id TEXT NOT NULL,
  playbook_version INTEGER NOT NULL,
  trigger_json TEXT NOT NULL,
  cursor_json TEXT,
  last_idempotency_key TEXT,
  state TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE trajectories (
  id TEXT PRIMARY KEY,
  source_json TEXT NOT NULL,
  manifest_artifact_id TEXT NOT NULL,
  events_artifact_id TEXT NOT NULL,
  outcome_json TEXT,
  reward_json TEXT,
  data_policy TEXT NOT NULL,
  digest TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE experiments (
  id TEXT PRIMARY KEY,
  spec_json TEXT NOT NULL,
  baseline_version TEXT NOT NULL,
  candidate_version TEXT NOT NULL,
  suite_version TEXT NOT NULL,
  state TEXT NOT NULL,
  result_artifact_id TEXT,
  created_at TEXT NOT NULL,
  completed_at TEXT
);

CREATE TABLE session_insight_reports (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  through_seq INTEGER NOT NULL,
  report_artifact_id TEXT NOT NULL,
  data_policy TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE INDEX idx_agent_mail_recipient ON agent_mail(session_id, to_agent_id, cursor);
CREATE INDEX idx_knowledge_status ON knowledge_items(status, updated_at);
CREATE INDEX idx_trajectory_policy ON trajectories(data_policy, created_at);
```

Security notes: CapabilityLease values and secret plaintext are deliberately absent. Handoff bundles themselves are content-addressed encrypted artifacts and the table stores only execution ownership state/digests. Screen recordings remain Artifact Store objects with redaction/data-policy metadata rather than SQLite blobs.
