#!/usr/bin/env python3
"""Artifact/native-primitives checks. NOT a replacement for cargo test or Rust compilation."""
import ctypes
import datetime
import errno
import json
import os
import platform
import re
import shutil
import sqlite3
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
checks = []


def passed(name):
    checks.append({'check': name, 'status': 'passed'})


def main():
    c = json.loads((ROOT / 'config.json').read_text())
    cargo = tomllib.loads((ROOT / 'Cargo.toml').read_text())
    assert cargo['package']['name'] == 'fim' and c['timezone'] == 'Europe/Bratislava'
    assert set(c) == set(re.findall(r'pub (\w+):', (ROOT / 'src/config.rs').read_text().split('pub struct Config {')[1].split('}\n')[0]))
    passed('Cargo.toml and config.json parse; configuration fields match Config')
    names = json.loads((ROOT / 'namedays.json').read_text())
    days = {(datetime.date(2000, 1, 1) + datetime.timedelta(days=i)).strftime('%m-%d') for i in range(366)}
    assert set(names) == days and all(isinstance(v, str) and v for v in names.values())
    passed('Calendar: exactly all 366 month/day keys')
    template = (ROOT / 'index.template.html').read_text()
    assert all(template.count(x) == 1 for x in ('{{DAILY}}', '<!-- FIM:daily -->', '<!-- /FIM:daily -->'))
    passed('Template: unique token and daily markers')

    db = sqlite3.connect(':memory:')
    # Execute the actual schema strings embedded in Rust, not reconstructed schemas.
    for source in ('journal.rs', 'baseline.rs'):
        text = (ROOT / 'src' / source).read_text()
        schemas = re.findall(r'execute_batch\("((?:[^"\\]|\\.)*)"\)', text, flags=re.S)
        assert schemas, source
        for schema in schemas:
            db.executescript(schema.replace('\\\n', ''))
    assert db.execute('PRAGMA user_version').fetchone()[0] == 2
    for table in ('baseline', 'policy', 'active', 'audit', 'outbox'):
        db.execute(f'SELECT * FROM {table} LIMIT 0')
    passed('Actual Rust-embedded SQLite schemas execute successfully')
    db.execute("INSERT INTO outbox(id,body) VALUES('x','test')")
    db.execute("UPDATE outbox SET attempts=attempts+1,next_attempt=10,last_error='failure' WHERE id='x'")
    assert db.execute('SELECT count(*) FROM outbox WHERE submitted_at IS NULL').fetchone()[0] == 1
    db.execute("UPDATE outbox SET submitted_at=11 WHERE id='x'")
    assert db.execute('SELECT count(*) FROM outbox WHERE submitted_at IS NULL').fetchone()[0] == 0
    passed('SQLite outbox: failed attempt retained, successful submission marked')

    if platform.system() == 'Linux' and platform.machine() in ('x86_64', 'aarch64'):
        libc = ctypes.CDLL(None, use_errno=True)
        class OpenHow(ctypes.Structure):
            _fields_ = [('flags', ctypes.c_uint64), ('mode', ctypes.c_uint64), ('resolve', ctypes.c_uint64)]
        def open_beneath(fd, path):
            how = OpenHow(os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC, 0, 0x01 | 0x04 | 0x08)
            result = libc.syscall(ctypes.c_long(437), ctypes.c_int(fd), ctypes.c_char_p(os.fsencode(path)),
                                  ctypes.byref(how), ctypes.c_size_t(ctypes.sizeof(how)))
            if result < 0:
                raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))
            return result
        with tempfile.TemporaryDirectory() as td:
            p = Path(td)
            (p / 'site').mkdir()
            (p / 'stage').mkdir()
            (p / 'outside').write_text('secret')
            (p / 'site/normal').write_text('inside')
            (p / 'site/link').symlink_to(p / 'outside')
            (p / 'site/ancestor').symlink_to(p)
            root_fd = os.open(p / 'site', os.O_RDONLY | os.O_DIRECTORY)
            stage_fd = os.open(p / 'stage', os.O_RDONLY | os.O_DIRECTORY)
            try:
                fd = open_beneath(root_fd, 'normal')
                assert os.read(fd, 100) == b'inside'
                os.close(fd)
                passed('Native Linux openat2: confined regular-file read')
                for name in ('link', 'ancestor/outside', '../outside'):
                    try:
                        fd = open_beneath(root_fd, name)
                    except OSError as e:
                        assert e.errno in (errno.ELOOP, errno.EXDEV)
                    else:
                        os.close(fd)
                        raise AssertionError(f'Unconfined path accepted: {name}')
                passed('Native Linux openat2: leaf/intermediate symlinks and parent escape rejected')
                (p / 'stage/new').write_text('new')
                (p / 'site/old').write_text('actual displaced bytes')
                rc = libc.renameat2(stage_fd, b'new', root_fd, b'old', 2)
                assert rc == 0, ctypes.get_errno()
                assert (p / 'site/old').read_text() == 'new'
                assert (p / 'stage/new').read_text() == 'actual displaced bytes'
                passed('Native renameat2 RENAME_EXCHANGE preserves displaced file')
                rc = libc.renameat2(stage_fd, b'new', root_fd, b'old', 1)
                assert rc == -1 and ctypes.get_errno() == errno.EEXIST
                passed('Native renameat2 RENAME_NOREPLACE refuses overwrite')
            finally:
                os.close(root_fd)
                os.close(stage_fd)
    else:
        checks.append({'check': 'Native Linux syscall tests', 'status': 'not run', 'reason': 'Unsupported host'})

    rust_tests = len(re.findall(r'#\[test\]', (ROOT / 'tests/regression.rs').read_text()))
    report = {
        'environment': {'platform': platform.platform(), 'python': platform.python_version(),
                        'rustc_available': shutil.which('rustc') is not None,
                        'cargo_available': shutil.which('cargo') is not None},
        'executed_checks': checks,
        'rust_regression_tests_written': rust_tests,
        'not_executed': ['Rust compilation/type checking', f'{rust_tests} Rust regression tests',
                         'CLI smoke tests against compiled Rust binary', 'Clippy/rustfmt',
                         'systemd deployment', 'Real email inbox delivery'],
        'important': 'Native syscall and SQLite checks do not establish that the Rust program compiles or passes its tests.'
    }
    out = ROOT / 'VERIFICATION.json'
    out.write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n')
    print(json.dumps(report, indent=2, ensure_ascii=False))


if __name__ == '__main__':
    main()
