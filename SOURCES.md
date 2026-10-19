# Sources and provenance

## User-provided lecture

The current review uses **`File_Integrity_Monitoring (1).pptx`**. Its relevant
topics are:

| Slide | Topic used in this project |
|---|---|
| 3 | Compare the current state with a known-correct baseline. |
| 4–5 | Manual/periodic scanning and continuous event-based monitoring. |
| 7 | File/directory counts and locations; file name, extension, size, creation time, modification time, and content. |
| 8 | Select protected assets, establish a baseline, calculate fingerprints, monitor, log, and alert. |
| 10 | Cost of hashing, legitimate dynamic files, and alert volume. |
| 12 | Establishing a trusted origin for the baseline. |
| 13 | Overview of checksum/hash families; this implementation uses SHA-256. |

The presentation describes FIM concepts. It does not provide a precise grading
rubric, mandate Rust or SQLite, or specify this website's daily publisher.
Regulations illustrated on slides 14–16 are not evidence that this application
is certified or satisfies a particular compliance framework.

Baseline schema 3 adds byte size, modification time, and available filesystem
birth time for ordinary files to close the attribute coverage gap highlighted
by slide 7. Directory size/timestamps and the daily root `index.html`'s raw
size/timestamps are deliberately excluded because expected updates change them;
the remaining integrity checks and the scope of this exception are documented
in [README.md](README.md).

## Historical project inputs

The earlier repository documentation records these inputs:

- `File_Integrity_Monitoring.pdf`: the previous lecture reference.
- `fim-rust.zip` / the previous `main.rs`: original implementation, HTML template,
  and calendar.
- `SB prednáška 2026 - 02.pdf`: a steganography lecture, not a source of FIM
  implementation requirements.

These entries preserve the existing provenance record; they do not imply that
all historical attachments were supplied or independently rechecked in the current
review. The syscall confinement, outbox, race handling, and supervisor are
engineering additions to the project, not code supplied by the lecture.

## Engineering references

- [Linux `openat2`](https://man7.org/linux/man-pages/man2/openat2.2.html): confined
  path resolution and supported resolve flags.
- [Linux `renameat2`](https://man7.org/linux/man-pages/man2/renameat2.2.html):
  `RENAME_EXCHANGE` and `RENAME_NOREPLACE` behavior.
- [Rust `Metadata`](https://doc.rust-lang.org/std/fs/struct.Metadata.html):
  byte size, modification time, and optional creation/birth time. Availability
  depends on the platform and filesystem; Unix inode-change time is not a
  substitute for birth time.
- [Rust Unix process groups](https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html):
  controlling the local MTA process group.
- [SQLite POSIX lock caveat](https://www.sqlite.org/howtocorrupt.html#posix_advisory_locks_canceled_by_a_separate_thread_doing_close_):
  closing a separate descriptor for an active database can cancel the process's
  advisory locks. Runtime database opening avoids such non-SQLite descriptors.
- [SQLite connection flags](https://www.sqlite.org/c3ref/open.html):
  `SQLITE_OPEN_NOFOLLOW` for refusing a symbolic-link database pathname.
- [notify 8.2 API and limitations](https://docs.rs/notify/8.2.0/notify/).
- [notify Linux event mapping and cookies](https://docs.rs/notify/8.2.0/src/notify/inotify.rs.html).
- [notify watcher configuration](https://docs.rs/notify/8.2.0/notify/struct.Config.html).
- [Rust installation](https://rust-lang.org/tools/install/).

## Name-day calendar

The calendar is inherited from the previous project without a new factual review
of each name. Its previously stated source is the
[official Slovak calendar for 2025](https://www.culture.gov.sk/storage/2020/03/Oficialne-kalendarium_2025.pdf).
Structural validation checks all 366 month/day keys and their values. That is not
confirmation of a 2026 edition or a guarantee that every inherited name remains
current. Calendar updates are explicit trusted-policy changes, not live downloads.
