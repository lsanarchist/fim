# Changelog

## 0.3.0 — lecture coverage and reliability fixes

### File attributes and consistent scanning

- Baseline schema **3** stores byte size, modification time with nanosecond
  precision, and optional filesystem birth time for ordinary regular files.
  Metadata-only changes are reported even when SHA-256 is unchanged. Unix `ctime`
  is not used as a creation-time substitute.
- Preserve the narrow daily-page exception: the root `index.html` retains strict
  normalized content, type, ownership, and permission checks, while raw size and
  timestamps are omitted. Directory size/timestamps are also omitted; recursive
  membership still detects changes in directory contents.
- Reject older or invalid baseline records instead of silently defaulting missing
  metadata. Upgrades require a new verified baseline and deliberate preservation
  of the old database, logs, and state; no automatic migration or overwrite.
- Recheck pathname metadata after hashing to detect same-inode writes as well as
  path replacement. Detect directory changes during/after enumeration and mark
  affected coverage unknown, preserving prior incidents until verification.

### Runtime failure and recovery

- Open runtime databases through SQLite without a separate reservation file
  descriptor. Closing that descriptor could cancel SQLite's POSIX locks and
  break cross-process WAL visibility. Retain path/type checks and use
  `SQLITE_OPEN_NOFOLLOW` when opening the connection.
- Clear stale `MAIL_UNAVAILABLE` when a standalone sender has already drained the
  outbox. Keep the failure active while queued batches remain in retry backoff.
- Surface failed audit replay as `AUDIT_REPLAY_FAILED`, preserve the original
  corrupt record, return a failing check status, and report recovery after repair.
- Keep the continuous monitor's eventual exit status unhealthy after event loss,
  even if a later full filesystem snapshot is clean.
- Exercise actual inotify exchange cookies and unrelated writes, standalone mail
  recovery, audit corruption, and live monitor recovery in runtime regressions.

### Deployment, CLI, and documentation

- Refuse occupied installation roots, systemd unit destinations, and the generator
  destination before any installer mutation, including broken symlinks and partial
  installations without the old configuration/page sentinel files.
- Resolve supervisor `sendmail` paths against the configuration directory, accept
  resolved alternatives symlinks consistently with the application, and limit
  supervisor configuration input to 1 MiB.
- Add `--help`/`-h` and `--version`/`-V` without requiring a config file. Validate
  unknown commands before attempting to load configuration.
- Replace the README with English setup, command/configuration references,
  lecture coverage, incident/recovery examples, deployment, limits, and schema 3
  upgrade guidance. Translate provenance and change history into English.

See `tests/runtime_regression.rs`, `tests/deploy_regression.py`,
`tests/cli_smoke.py`, and the Rust scan/baseline regression scenarios. Actual
executed checks, source provenance, and results are recorded by `scripts/check`
in `VERIFICATION.json`; a list of tests is not a claim that production services
or inbox delivery were verified.

## 0.2.0 — response to the previous review

| Previous concern | Change in v2 | Regression scenarios |
|---|---|---|
| One error aborts the whole scan | Independent `Scan.entries/errors/unknown` results and a raw digest for an invalid page. | `damaged_index_does_not_hide_other_changes`, `symlink_does_not_stop_other_files`, `another_change_in_invalid_index_changes_report` |
| Incomplete traversal produces false deletion/recovery | Unknown prefixes do not become `DELETED` or `RECOVERED`. | `unknown_prefix_not_deleted_or_recovered` |
| Sendmail blocks scanning | Separate worker, regular-file stdin, timeout, process-group termination, and bounded reaping. | `mail_timeout_kills_group_and_preserves_queue`, `blocked_mail_worker_does_not_block_scan` |
| Failed alerts disappear | SQLite outbox with retries/backoff and no automatic deletion of pending messages. | `outbox_and_active_survive_restart`, `repeated_state_is_not_requeued` |
| Recovery depends on `last_sent` | Active incident state is independent of delivery state. | `recovery_recorded_even_when_mail_failed` |
| Read-only SQLite does not protect the reference file | Root-owned trust paths, pinned private snapshots, and repeated guard checks. | `tampered_baseline_not_used_by_loaded_monitor`, `unsafe_trust_file_is_rejected` |
| Calendar is an unverified live input | Load once, guard it, and bind calendar/template/timezone to baseline policy. | `calendar_changes_break_guard_and_baseline_policy` |
| Symlink and TOCTOU risks | Confined `openat2` reads, descriptor hashing, and metadata/identity checks. | `rejects_intermediate_symlink_and_parent_escape`, `concurrent_symlink_swap_never_reads_outside_root` |
| Publisher can silently erase a concurrent replacement | Exchange, verify, and preserve the actual displaced object; no blind rollback. | `publisher_refuses_damaged_page`, `exchange_preserves_actual_old_inode` |
| Traversal crosses midnight | Check time immediately before/after reading the page with an explicit IANA timezone. | `read_crossing_midnight_accepts_new_and_old`, grace/future-date/DST scenarios |
| Ownership, permissions, and directories are missing | Store mode, UID, GID, object type, and directories. | `permission_changes_and_empty_directories` |
| Log failure stops monitoring or concurrent writes corrupt JSONL | Best-effort JSONL, SQLite audit fallback/replay, stderr, and an interprocess write/rotation lock. | `log_failure_has_durable_audit_fallback_and_replay`, `log_rotation_is_bounded`, concurrent log scenarios |
| Partial log write leaves a broken JSONL record | Restore the prior length under the log lock; report rollback failure explicitly. | Partial-write Rust regression |
| Entire ordinary files are loaded into RAM | 64 KiB streaming, separate page limit, file/tree/time budgets. | `stream_hash_matches_known_bytes`, `oversized_file_does_not_hide_smaller_file`, `deadline_expires_without_hanging` |
| Short-lived changes disappear between scans | `FS_ACTIVITY` independent of the next hash; overflow alert and rescan. | `restored_mutation_remains_alertable`, real CLI watcher scenario |
| Authorized midnight publication looks malicious | Protected receipt plus paired rename cookies; do not authorize ordinary data/metadata events. | `publisher_keeps_displaced_page_and_paired_receipt` |
| Concurrent monitors or repeated initialization | `flock` and atomic baseline creation without overwrite. | `cannot_run_two_monitors_with_same_state`, `baseline_overwrite_refused` |
| Bulk changes overflow RAM despite healthy SQLite | Flush audit in batches while forming incidents; reserve RAM buffering for storage failure. | Bulk-incident Rust regression |
| Oversized HTML is published before validation | Validate rendered size before replacement. | Publisher regressions |
| Queued delivery depends on template/calendar health | Separate mail-only configuration loading. | Rust/CLI delivery regressions |
| Long idle scan intervals cause watchdog restart | Main-loop idle heartbeat derived from `WATCHDOG_USEC`; blocked scans remain supervised. | UnixDatagram watchdog regression |
| Timer ignores configured timezone | Validated IANA generator and config path unit, including later edits. | Kyiv/config-edit/invalid-timezone scenarios and offline systemd verification |
| Installer mutates the host before finding a missing asset | Validate required inputs and timezone before user/directory creation. | Missing stylesheet and invalid timezone without mutation |
| Stale reports or dependence on hosted CI | `Cargo.lock`, local `scripts/check`, and timestamped reports of actual commands. | Current `VERIFICATION.json`; production/inbox behavior remains separate |

Remaining limits include a non-atomic tree snapshot, unrecoverable history after
event loss, blocked kernel syscalls, root/kernel/monitor-UID compromise, failure of
all logging channels, and no proof of inbox delivery. See [README.md](README.md).
