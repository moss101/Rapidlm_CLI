# SQLite Persistence Model

Logical tables: `sessions`, `events`, `projection_checkpoints`, `runs`, `graph_revisions`, `graph_nodes`, `graph_edges`, `node_attempts`, `operation_journal`, `artifacts`, `artifact_refs`, `goals`, `criteria`, `claims`, `evidence`, `verification_records`, `workspace_views`, `workspace_transactions`, `read_observations`, `context_packets`, `approvals`, `leases`, `processes`, `monitors`, `triggers`, `job_leases`, `resource_leases`, `handoffs`, `control_leases`, `knowledge_items`, `preferences`, `trajectories`, `experiments`.

SQLite is the local durability baseline with WAL and explicit transactions. FTS tables/projections are rebuildable caches. Large bodies/logs/screenshots are CAS files. Migrations are forward-only with pre-migration backup/compatibility fixture; downgrade is restore-from-backup, not reverse mutation.
