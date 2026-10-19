# Rust File Integrity Monitor

A Linux command-line File Integrity Monitoring (FIM) project for a static website.
It records a known-good baseline in SQLite, compares file contents and metadata,
logs changes, and queues email alerts. Continuous inotify monitoring complements
periodic SHA-256 scans. A separate publisher updates the website's date and Slovak
name-day block without exempting the rest of the page from integrity checks.

**Version:** 0.3.0. **Baseline schema:** 3; older baselines require an explicit,
verified replacement. One Rust executable, JSON configuration, bundled SQLite,
and a local sendmail-compatible mail transport. No external database server,
Tokio runtime, web administration interface, or online name-day API is required.

## Scope and lecture coverage

The project implements the baseline → fingerprint → comparison → log/alert flow
described in the supplied *File Integrity Monitoring* lecture. The lecture explains
FIM concepts rather than prescribing this exact Rust architecture. The website
publisher, protected state, durable outbox, and systemd deployment are project
features built around those concepts.

| FIM topic | Implementation and scope |
|---|---|
| Known-good baseline | SQLite stores relative paths, SHA-256 fingerprints, object types, and metadata. Monitoring never updates the baseline automatically. |
| File content | Full SHA-256 scans; only the exactly validated daily block of the root `index.html` is normalized. |
| Attributes | Type, permission bits, UID and GID for files/directories; byte size and nanosecond modification time for ordinary files; creation/birth time when the filesystem exposes it. |
| File count and location | Recursive baseline membership detects additions, deletions, empty directories, and moves. A move is reported as a removed path and an added path, not attributed to a user. |
| Scheduled and continuous checking | `check` runs once; `watch` combines periodic scans with inotify events and recovery after watcher failure. |
| Logging and alerts | Rotating JSON Lines log, SQLite audit fallback, persistent email outbox, retries, reminders, and recovery events. |
| Legitimate dynamic content | A trusted local template/calendar, configured timezone, strict daily-block validation, and a separate midnight publisher. |

Directory size/timestamps change during ordinary child creation and are not stored
as baseline attributes. The daily `index.html` also excludes raw size, modification
time, and birth time: authorized daily replacement changes those values. Its
normalized content, object type, permissions, and ownership remain checked.
Creation time is optional; Unix `ctime` is **not** treated as creation time.

This implementation does not record ACLs, extended attributes, full historical
file contents, or the identity of the process/user that made a change. It does not
promise protection against root, a compromised kernel, or the monitor's own UID.
See [Security and operational limits](#security-and-operational-limits).

## Requirements

- Linux 5.6+ with `openat2`, a mounted `/proc`, and a local filesystem supporting
  inotify and `renameat2(RENAME_EXCHANGE)`. Site and staging must be on the same
  filesystem for publication.
- Rust 1.85+ with Cargo, rustfmt, and Clippy; a C compiler for bundled SQLite.
  Dependencies are pinned by the committed `Cargo.lock`.
- Python 3.11+ and the system IANA timezone database (`tzdata`) for the checks and
  deployment helpers. Full static deployment verification needs `systemd-analyze`.
- A configured local sendmail-compatible MTA for real notifications. The installer
  does not install or configure an MTA. Automated tests use local fake transports.

Native Windows is unsupported. Use a Linux VM or WSL2 with the project on its
Linux filesystem, rather than `/mnt/c`. NFS, FUSE, and arbitrary remote or
unreliable filesystems are outside the intended deployment environment.

## Quick start for a local lab

Run these commands from a fresh checkout. If a previous baseline already exists,
follow [Upgrading an existing installation](#upgrading-an-existing-installation).

```sh
git clone https://github.com/lsanarchist/fim.git
cd fim

./scripts/check
./target/release/fim --help

mkdir -p state staging
chmod 700 state staging
```

Edit `config.json`: set `mail_from`, `mail_to`, and the path to your working
`sendmail` executable. The sample addresses are placeholders. Test the transport
before relying on it:

```sh
./target/release/fim --demo config.json alert-test
# Confirm that the test message actually arrived in the intended inbox.

./target/release/fim --demo config.json page
./target/release/fim --demo config.json init
./target/release/fim --demo config.json check
./target/release/fim --demo config.json watch
```

`page` creates the initial trusted page; `init` records the current verified tree.
Use Ctrl+C to stop `watch`. The sample interval is 30 seconds, and filesystem
events can prompt a scan before the next periodic deadline.

**`--demo` relaxes root-ownership checks for trusted files and their ancestors.**
It is intended for a single-user lab, not a web server deployment. Path
confinement, object-type checks, and private `state/` and `staging/` permissions
still apply. Relative config paths are resolved against the configuration file's
directory; a shell `TZ` variable is unnecessary because `timezone` is explicit.

### Demonstrate an incident and recovery

With `watch` running, open another terminal in the checkout:

```sh
# Keep a trusted copy, including the original modification time and permissions.
cp --preserve=all site/style.css state/style.css.trusted-copy
printf '\n/* FIM demonstration change */\n' >> site/style.css
tail -n 10 state/fim.log

# Restore the existing file in place and preserve its recorded metadata.
cp --preserve=all state/style.css.trusted-copy site/style.css
tail -n 10 state/fim.log
```

Allow time for each scan, or stop `watch` and run `check` explicitly. Inspect the
`INCIDENT`, `FS_ACTIVITY`, and `RECOVERED` records and the alert outbox/deliveries.
A changed file normally causes `check` to return 1. Restoring just the bytes can
leave `METADATA_CHANGED` active because modification time is also monitored.
Replacing an inode can change its birth time, which ordinary restore commands
cannot necessarily reproduce.

Do not recreate the baseline merely to silence an incident. Restore a trusted
original and its monitored metadata, or investigate and explicitly approve a new
known-good baseline. A file changed and immediately restored while `watch` is
active can still produce an `FS_ACTIVITY` alert.

## Command reference

```text
fim [--demo] <config.json> <page|init|check|watch|alert-test|deliver>
fim --help
fim --version
```

`-h` and `-V` are also supported. Help and version do not require a configuration
file and can follow `--demo`.

| Command | Behavior |
|---|---|
| `page` | Create the first page, or verify and publish the allowed daily update. It preserves the displaced page for verification. |
| `init` | Create a new baseline from a complete, valid tree. An existing baseline is never overwritten. |
| `check` | Run one integrity check and at most one bounded attempt to submit a due email batch. Other batches remain queued. |
| `watch` | Monitor continuously, periodically rescan, process filesystem events, and service the outbox independently. |
| `alert-test` | Queue a test notification and attempt one due batch. An older due batch or busy worker can take precedence; verify the inbox. |
| `deliver` | Attempt one due outbox batch without requiring a healthy website, calendar, or template. Print the remaining pending batch count. |

For `check`, exit code **0** means the current check found no incidents or detected
operational failures; **1** means an incident or detected operational failure;
**2** means startup/command failure. A zero exit status from mail commands is not
proof of inbox delivery. Only one `watch`/`check` process may use a given
`state_dir`; `page` uses a separate lock and can run alongside the monitor.

## Configuration reference

All fields in [`config.json`](config.json) are required. Unknown fields, invalid
types, unsafe path overlaps, and out-of-range values are rejected at startup.
Trusted configuration input is limited to 1 MiB. Paths are resolved against the
configuration directory, including a relative `sendmail` path.

### Paths and daily policy

| Field | Sample default | Meaning |
|---|---|---|
| `root` | `site` | Public website tree to scan recursively. Filesystem root itself is unsupported. |
| `database` | `baseline.db` | Trusted baseline database. `init` creates it once. |
| `calendar` | `namedays.json` | Trusted local calendar with exactly all 366 `MM-DD` keys. |
| `template` | `index.template.html` | Trusted UTF-8 HTML template with one `{{DAILY}}` token and one pair of daily markers. |
| `state_dir` | `state` | Private runtime directory for `state.db`, JSONL logs, locks, and the publication receipt. |
| `staging_dir` | `staging` | Private directory for prepared/displaced pages; must support exchange with the public root. |
| `timezone` | `Europe/Bratislava` | IANA timezone for page dates, validation, and the generated midnight timer. |
| `midnight_grace_seconds` | `60` | Accept the exact previous day's block for this many seconds after midnight; range 0–300. |

Public and private paths must be disjoint; private paths must not overlap one
another. `state_dir` and `staging_dir` must exist, be private (mode 0700), and be
owned by the process user; root may administer them. Production trusted files
and their ancestors must be root-owned and protected against group/other writes.

### Scanning and logging

| Field | Sample default | Meaning and accepted range |
|---|---|---|
| `interval_seconds` | `30` | Target interval between periodic scan starts; 1–86,400 seconds. |
| `max_file_bytes` | `1073741824` | Maximum ordinary file size; from `max_index_bytes` through 1,099,511,627,776 bytes (1 TiB). |
| `max_index_bytes` | `1048576` | Maximum template/rendered `index.html` size; 1,024–16,777,216 bytes. |
| `max_entries` | `100000` | Maximum baseline/scan entry count; 1–1,000,000. Includes directories. |
| `scan_budget_seconds` | `60` | Userspace budget for a full scan; 1–300 seconds. Coordinate with systemd watchdog timeouts. |
| `log_max_bytes` | `10485760` | Rotate `fim.log` at this size; minimum 65,536 bytes. Keep up to five previous files. |

Files are hashed in 64 KiB blocks; the daily page has its own bounded in-memory
read. Each ordinary file also has a five-second userspace read budget. Exceeding
a budget or limit means incomplete coverage, not a clean result. A userspace
deadline cannot interrupt a system call stuck in the kernel.

### Mail and reminders

| Field | Sample default | Meaning and accepted range |
|---|---|---|
| `sendmail` | `/usr/sbin/sendmail` | Local sendmail-compatible executable. Root-controlled alternatives symlinks are resolved. |
| `mail_from` | `fim@example.com` | Valid sender mailbox; replace the placeholder. |
| `mail_to` | `admin@example.com` | Valid responsible recipient mailbox; replace the placeholder. |
| `alert_timeout_seconds` | `10` | Timeout for a local MTA attempt; 1–60 seconds. |
| `retry_seconds` | `30` | Initial retry delay; 1–3,600 seconds. Failures back off exponentially, capped at one hour. |
| `reminder_seconds` | `3600` | Reminder interval for active incidents; 60–31,536,000 seconds. |

The monitor loads and pins its configuration at startup. Editing it is not an
automatic in-process reload: restart `fim.service` after a deliberate change.
The config path unit only regenerates the publisher schedule. Changes to the
calendar, template, or timezone also affect baseline policy and require explicit
administration of the trusted reference.

## How monitoring and publication work

### Independent scan results

One damaged page, unreadable file, symlink, oversized object, or failed directory
does not discard results for unrelated files. An invalid daily block gets
`DAILY_INVALID` and a fingerprint of its raw bytes, so a second malicious edit
to an already-invalid page is still distinguishable.

Unverified subtrees are marked **unknown**. Their entries are not reported as
deleted, and prior incidents are not declared recovered without a new verified
observation. Directory changes during or after enumeration invalidate coverage
for the affected subtree. Content, size, ownership, permissions, and monitored timestamps are
compared separately so an unchanged content hash does not hide an attribute edit.

### Durable audit and email

`state_dir/fim.log` is a rotating JSON Lines log. SQLite stores active incidents,
audit records, and the persistent outbox separately from the trusted baseline.
Failure to write the JSONL log does not stop scanning: audit records can be
replayed from SQLite when logging recovers. Concurrent processes share a stable
`fim.log.lock` for writing and rotation. A partial log write triggers a best-effort
rollback to the previous file length; rollback failure is reported separately.
Corrupt/unreadable replay records produce `AUDIT_REPLAY_FAILED` without deleting
the original evidence, and `check` reports failure until replay recovers.
Runtime database files are opened through SQLite itself, with path/type checks
and `SQLITE_OPEN_NOFOLLOW`. Avoiding unrelated open/close operations on a live
database preserves SQLite's process-level file locks and cross-process coherence.

The mail worker runs independently of the scan loop. Failed attempts remain
queued; incident recovery does not depend on successful delivery of the original
alert. MTA stdin uses a regular temporary file to avoid blocking the scanner on
a pipe. Timeouts terminate the MTA process group; an unreaped stuck child does
not lead to an unbounded stream of replacement processes.
If a separate `deliver` process drains the outbox, a stale mail failure can clear;
pending batches still in backoff do not count as recovered mail transport.

The outbox provides **at-least-once submission to the local MTA**, not exactly-once
delivery or proof of inbox receipt. A crash between MTA acceptance and SQLite
acknowledgement can produce a duplicate. `ALERT_QUEUED` means queued, not read.

### Daily page handling

The root page contains exactly one block, for example:

```html
<!-- FIM:daily -->Dátum: 2026-09-28. Meniny: Václav.<!-- /FIM:daily -->
```

Only after exact date/name validation is that block normalized to `{{DAILY}}`.
Everything else remains under SHA-256. The allowed date is evaluated immediately
before and after reading `index.html`; both dates are accepted only when that
read itself crosses midnight. The grace window accepts the exact complete
previous-day value, not arbitrary content written near midnight.

The publisher validates the existing page, prepares a new page in private staging,
and uses `RENAME_EXCHANGE`. It then verifies the **actual displaced object** for
content, identity, and metadata. An unexpected object is retained as `candidate-*`
and reported; there is no blind rollback that could destroy a later replacement.
Verified old pages use `verified-*`, with the latest seven retained.

Authorized inotify publication events require a protected receipt, a verified
current page, and paired rename cookies connecting the specific staging file to
`index.html`. Ordinary data/metadata writes are not authorized by a nearby legal
update. Missing pairs, late receipts, and event loss are handled conservatively.

After more than one missed day, the publisher deliberately refuses to repair a
stale page automatically. Review the cause and restore the correct current block
from trusted inputs; do not recreate the baseline to bypass that review.

## Server deployment

The provided templates target a fresh Debian/Ubuntu systemd installation with a
working local MTA, such as Postfix. Run the build/checks as your ordinary user.

```sh
./scripts/check
sudo ./deploy/install.sh
sudoedit /opt/fim/config.json

# Apply the configured timezone after the initial edit.
sudo systemctl daemon-reload
sudo -u fim /opt/fim/fim /opt/fim/config.json alert-test
# Verify actual receipt before depending on alerts.

sudo -u fim /opt/fim/fim /opt/fim/config.json page
sudo /opt/fim/fim /opt/fim/config.json init
sudo systemctl enable --now fim-config.path fim.service fim-page.timer
```

The installer checks all required inputs and the IANA timezone before mutation.
It refuses existing installation roots (`/opt/fim`, `/var/lib/fim`,
`/var/www/fim`), destination unit files, and the generator target, including broken
symlinks. This includes partial installations; it is not an upgrade installer.
The preflight is not a transaction against concurrent root changes, and its
timezone check does not replace the application's full startup validation.

Configure the web server to publish **only `/var/www/fim/site`**. The web-server
account (for example `www-data`) must not own trusted/runtime files, belong to the
`fim` group, or have write access to configuration, baseline, state, or staging.
Never use `--demo` for this deployment.

| Location or component | Ownership and role |
|---|---|
| `/opt/fim` executable, config, baseline, calendar, template | Root-owned trust area. |
| `/var/lib/fim` | Private runtime state owned by `fim`, mode 0700. |
| `/var/www/fim/site` | Published tree owned by `fim`; exposed read-only to the monitor by systemd. |
| `/var/www/fim/staging` | Private publisher workspace owned by `fim`, mode 0700. |
| `fim.service` | Continuous monitor, independent mail worker, watchdog and restart policy. |
| `fim-page.timer` / `fim-page.service` | Midnight publisher in the configured timezone. |
| `fim-config.path` / `fim-config.service` | Regenerate and restart the timer after configuration changes. |
| `fim-failure@.service` | Independent Python failure notification for startup, watchdog, or publisher failure. |

The monitor and publisher share the `fim` UID but run in separate service mount
namespaces. The monitor sees the site as read-only. This is not a security boundary
against compromise of the shared UID. The templates deliberately do not set
`NoNewPrivileges=true`, because the selected MTA may require a setgid helper such
as Postfix `postdrop`; test mail delivery under the actual service user.

### Timers, watchdog, and operational checks

The systemd generator builds midnight `OnCalendar` from `/opt/fim/config.json` at
boot and `daemon-reload`. Invalid timezone configuration removes the schedule and
fails visibly instead of falling back to host time. If `fim-config.path` is
disabled, run `systemctl daemon-reload` and `systemctl restart fim-page.timer`
manually after editing. Changing timezone also changes baseline policy.

```sh
sudo systemctl status fim.service fim-page.timer fim-config.path
sudo systemctl list-timers fim-page.timer
sudo journalctl -u fim.service -u fim-page.service -u fim-config.service
sudo tail -n 20 /var/lib/fim/fim.log
```

The default service has `WatchdogSec=120` and `TimeoutStartSec=120`, intended for
the sample 60-second scan budget. Idle waits keep sending heartbeat messages at
an interval derived from `WATCHDOG_USEC`, no longer than 30 seconds, so a long
scan interval alone does not trigger a restart. Scanning and blocking operations
remain subject to the watchdog; increase service timeouts deliberately if a larger
scan budget or startup duration requires them.

The Python failure notifier works without the Rust executable, baseline, or
`state.db`. It still needs readable configuration and a working MTA. It bounds
configuration reads to 1 MiB and resolves relative mail paths against the config
directory, consistently with the application. It is not a substitute for external
service monitoring.

## Security and operational limits

- **Protected reference data:** production trust files and their ancestors are
  root-owned and not writable by group/others. Baseline and policy are loaded once
  into memory. SQLite parses a private snapshot of the exact verified baseline
  bytes; loading requires writable state storage and sufficient free space.
  Repeated digest/permission guards report `TRUST_CHANGED` without adopting a
  changed on-disk reference. This is ownership-based protection and pinning, not
  a digital signature.
- **Confined reads:** `openat2` uses `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
  RESOLVE_NO_XDEV` with no unsafe fallback. Files are hashed through open
  descriptors and checked for concurrent metadata/path changes. Symlinks,
  hardlinked files, FIFOs, sockets, and devices are not accepted as normal files.
  Non-UTF-8 paths are stored losslessly using hex encoding.
- **No global snapshot:** concurrent changes can make coverage unknown or produce
  an incident. The scan is not an atomic snapshot of the whole tree. Publisher
  exchange detects races and preserves displaced objects, but the new page may
  already be published when post-exchange verification fails. Stored candidates
  are not a cryptographically immutable forensic archive.
- **Event history has limits:** observed transient changes remain alertable even
  when a later hash matches. Kernel/application queue overflow produces
  `EVENT_LOSS` and a rescan; watcher failure is separately reported and retried
  while periodic checking continues. A rescan cannot reconstruct already-lost
  short-lived events. Monitoring cannot report events that occurred before it
  started solely from an unchanged final tree.
- **Bounded failure handling:** audit buffering is limited to 4,096 RAM events
  when storage is unavailable; overflow reports `AUDIT_LOSS`. Healthy SQLite is
  flushed in batches during large reconciliations. Pending mail and unreplayed
  audit are not deleted just to enforce a storage cap; prolonged failure needs
  disk-space monitoring. Simultaneous failure of storage, stderr/journald, and
  notification cannot guarantee preservation of every event.
- **Resource cost:** ordinary files are fully hashed; mtime/size are not used as
  a shortcut around content checking. Each scan remains proportional to the data
  read. Limits and a supervisor bound some failure modes, but a syscall blocked
  in the kernel is not interrupted by a userspace read budget. The interval is a
  scheduling target, not a guaranteed maximum detection delay; overrun produces
  `SCAN_OVERRUN`.

## Upgrading an existing installation

Version 0.3.0 uses **baseline schema 3** to record ordinary-file size, modification
time, and available birth time. Schema 1 and 2 baselines are rejected; there is
no automatic migration or rebaselining.

1. Stop the monitor and publisher timer before making an intentional upgrade.
2. Preserve the old baseline, configuration, logs, and state database for review.
3. Verify the website and trusted calendar/template independently. Investigate
   pending incidents before accepting a new reference.
4. Configure a **new database path and a new private state directory**, for example
   `baseline-v3.db` and `state-v3`. Create the state directory with the correct
   owner and mode 0700, then run `init` against the verified site. Keep the old data.
5. Test a clean check, an actual change/recovery, the configured publisher schedule,
   and real mail receipt before resuming service.

Do not rerun the fresh installer over an existing installation. The runtime state
schema is separate from the baseline schema; retaining historical state artifacts
does not imply that pending outbox entries have been migrated to the new state
directory. The `deliver` command can process preserved old state using a valid
configuration for that state directory.

## Verification and project layout

[`scripts/check`](scripts/check) is the single local verification entry point:
formatting, Rust tests, Clippy with `-D warnings`, locked release build, executable
CLI scenarios, isolated deployment tests, and artifact/native-primitive probes.
GitHub is used for source control; GitHub Actions is not required.

[`VERIFICATION.json`](VERIFICATION.json) records the commands, timestamps, source
provenance, and results of an actual local check run. Its logs are in
`target/check-logs/`. Check the report's source hash/revision and date when using
it as evidence. Native Python/ctypes and SQLite probes are explicitly separate
from compiling or executing Rust; `tests/validate_artifacts.py` alone does not
rewrite the full verification report or certify the Rust tests.

Local build and static systemd verification do **not** establish a production
installation, live timer/watchdog behavior, web-server configuration, or delivery
to a real inbox. Deployment tests use an isolated temporary tree and inert command
fixtures; they neither install host services nor run live `systemctl`.

| Path | Responsibility |
|---|---|
| `src/scan.rs`, `src/secure.rs` | Recursive scanning, fingerprints, metadata, and confined Linux filesystem operations. |
| `src/daily.rs`, `src/publisher.rs` | Daily policy, verified publication, and receipts. |
| `src/baseline.rs`, `src/config.rs` | Baseline schema, configuration, trusted inputs, and pinning. |
| `src/journal.rs`, `src/alerts.rs`, `src/monitor.rs` | Audit/state, mail outbox, watcher, and scheduling. |
| `tests/*.rs` | Rust regression scenarios for correctness and failure handling. |
| `tests/cli_smoke.py` | End-to-end scenarios against the compiled executable with a fake local MTA. |
| `tests/deploy_regression.py` | Timezone generation, installer refusal, supervisor transport, and offline unit verification. |
| `tests/validate_artifacts.py` | Artifact/schema checks and separately scoped native probes. |
| `deploy/` | Fresh installer, systemd units, timezone generator, and independent failure notifier. |

## Calendar and sources

`namedays.json` is inherited unchanged from the previous project. Its stated
source is the official Slovak calendar for **2025**, not an independently verified
2026 edition. All 366 keys are validated structurally; the semantic currency of
each name in a newer edition has not been rechecked. Updating this trusted dataset
is an explicit policy change, not a background network fetch.

See [SOURCES.md](SOURCES.md) for provenance and engineering references, and
[CHANGELOG.md](CHANGELOG.md) for the implementation history.
