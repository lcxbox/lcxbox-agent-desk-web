# Personal agent storage

This crate owns the common SeaORM entities and bounded storage operations used by
Manager and standalone Signal. It uses the caller's connection or transaction;
it does not open databases, select a SQL driver, create runtime tasks, or acquire
native account/device/session authority.

- `entity`, `entity_table`: shared table shapes, explicit schema registration
  and single-statement table creation that keeps an entity's unique keys.
- `projection`, `retrieval_cache`, `citations`: immutable read snapshots, frozen
  query pages and display-only navigation references.
- `keyword_search`, `memory_keyword`, `history_query`, `history_native`: bounded
  SQL candidates, sparse ranking and exact native metadata verification. Host
  source readers retain all original source authorization.
- `memory_control`, `suppression`, `evidence`: forgetting, source suppression and
  authenticated evidence-link decoding.
- `memory_write`, `memory_history`: synchronous storage of model-authored
  memories and bounded revision selection.
- `memory_quota`, `memory_index`: exact resource deltas and search index writes.
- `source_lifecycle`, `source_rows`: generation revocation after the host
  authenticates deletion, and row keys of model-visible source ids.
- `output_store`, `output_gc`: accepted-output proofs and output GC discovery,
  scan hints and final quota cursor writes.
- `main_request`, `worker_request`, `worker_report`, `delegation_group`,
  `workbench`, `device_roster`: main request intake, the Worker queue and its
  immutable reports, delegation groups, workbench identity and session device
  configuration.
- `proposal_review`, `decision_resume`: proposals waiting for the AI reviewer or
  the owner, and resume records for turns paused on a decision.
- `schedule`: scheduled-task storage rules.
- `skill_package`, `skill_store`: Agent Skills package validation and platform
  and personal skill storage.
- `issue_authority`, `issue_events`, `issue_reply`: issue source policy and
  accepted facts, source polling, and durable reply publication facts.

A snapshot establishes immutable content identity. It does not authorize model
use, revive a deleted source, extend expiry, or replace the original host's
SELECT/version/lease/raw-JSON fences. Native writes finish before the original
transaction's final agent quota mutation. Errors require rollback of the
whole caller transaction.

Storage modules do not construct model provider adapters; each host's
application layer owns them.

Manager keeps PostgreSQL actor locking, platform configuration, personal account
and device checks, and billing. Signal keeps SQLite writer admission, file-based
configuration and local instance binding. User-visible APIs are unchanged.

Manager's business schema registers these tables through `desk_database::schema`;
Signal uses its existing explicit SQLite schema initialization. The optional
`schema-sync` feature is enabled by Manager; Signal does not pull in schema-sync
as a production dependency. Re-exported
entities in both hosts refer to these same Rust types. Signal retains its quota
entity extension for native cleanup cursors; common accounting accesses only the
shared counter fields and preserves those host-only columns. No migration or backfill
is provided for this unreleased feature.

Run the crate's unit tests from the `web` workspace (`cargo test -p
desk-agent-data-store --lib`). Manager is in the parent workspace and tests
the same operations through its own SQLite and PostgreSQL host integration tests.
Both build contexts collect independent frame reports for this crate.
