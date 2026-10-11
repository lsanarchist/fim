# Зв'язок із попереднім оглядом

| Зауваження | Зміна в v2 | Регресійні сценарії |
|---|---|---|
| Одна помилка скасовує scan | `Scan.entries/errors/unknown`, окремі результати, сирий digest некоректного index | `damaged_index_does_not_hide_other_changes`, `symlink_does_not_stop_other_files`, `another_change_in_invalid_index_changes_report` |
| Незрозумілі видалення при неповному обході | Unknown-prefix не стає DELETED або RECOVERED | `unknown_prefix_not_deleted_or_recovered` |
| Sendmail блокує сканер | Окремий потік, regular-file stdin, timeout, kill process group, bounded reap | `mail_timeout_kills_group_and_preserves_queue`, `blocked_mail_worker_does_not_block_scan` |
| Алерт губиться після відмови | SQLite outbox, спроби/відтермінування, pending без автоматичного видалення | `outbox_and_active_survive_restart`, `repeated_state_is_not_requeued` |
| RECOVERED прив'язано до last_sent | Active-state незалежний від mail-state | `recovery_recorded_even_when_mail_failed` |
| Read-only SQLite не захищає файл | Root-owned trust paths, pinned snapshot, повторні guard-перевірки | `tampered_baseline_not_used_by_loaded_monitor`, `unsafe_trust_file_is_rejected` |
| Календар — неперевірений live-input | Завантаження при старті, guard, прив'язка календаря/шаблону/timezone до baseline | `calendar_changes_break_guard_and_baseline_policy` |
| Symlink/TOCTOU | openat2 beneath/no-symlinks/no-xdev, descriptor hashing, metadata/identity recheck | `rejects_intermediate_symlink_and_parent_escape`, `concurrent_symlink_swap_never_reads_outside_root` |
| Publisher може непомітно затерти конкурентну підміну | Exchange + перевірка й збереження фактично витісненого об'єкта; без сліпого rollback | `publisher_refuses_damaged_page`, `exchange_preserves_actual_old_inode` |
| Обхід перетинає опівніч | Час безпосередньо до/після читання index; явний IANA timezone | `read_crossing_midnight_accepts_new_and_old`, grace/future-date/DST тести |
| Немає контролю прав/власника/директорій | mode, UID, GID, тип, директорії у baseline | `permission_changes_and_empty_directories` |
| Log I/O завершує монітор | Best-effort log + SQLite audit + replay + stderr; обмежений RAM buffer | `log_failure_has_durable_audit_fallback_and_replay`, `log_rotation_is_bounded` |
| Весь файл у RAM | 64 KiB streaming; окремий ліміт index; бюджети розміру/часу/дерева | `stream_hash_matches_known_bytes`, `oversized_file_does_not_hide_smaller_file`, `deadline_expires_without_hanging` |
| Коротка зміна між scan губиться | FS_ACTIVITY незалежно від наступного хеша; overflow alert + rescan | `restored_mutation_remains_alertable`; реальний сценарій у CLI smoke |
| Легальна опівнічна публікація виглядає атакою | Приватна квитанція + paired rename cookies; не авторизує Data/Metadata події | `publisher_keeps_displaced_page_and_paired_receipt` |
| Паралельні процеси / повторний init | flock та атомарне створення baseline без overwrite | `cannot_run_two_monitors_with_same_state`, `baseline_overwrite_refused` |
| Немає перевіреної збірки | Додано 40 Rust-тестів, CLI smoke, CI та чесний machine-readable звіт | Rust-збірка й ці тести **не виконані** тут; див. VERIFICATION.json |

Залишкові межі: неатомарний snapshot дерева, втрачена історія після overflow,
завислі системні виклики, компрометація root/ядра/UID монітора, недоступність усіх
каналів журналювання, відсутність доказу доставки в inbox. Докладно — README.
