#!/usr/bin/env python3
"""Run against a BUILT Rust binary. Uses fake local sendmail; never sends real email."""
import json
import os
import shlex
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

PROJECT = Path(__file__).resolve().parents[1]
BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else PROJECT / 'target/release/fim').resolve()


def wait_for(predicate, timeout=8):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError('Timed out waiting for the expected FIM event')


def setup(p):
    for d in ('site', 'state', 'staging'):
        (p / d).mkdir(mode=0o700 if d != 'site' else 0o755)
    for name in ('namedays.json', 'index.template.html'):
        (p / name).write_bytes((PROJECT / name).read_bytes())
    (p / 'site/style.css').write_text('body { margin: 0; }\n')
    mta = p / 'sendmail-test'
    mta.write_text('#!/bin/sh\ncat >> ' + shlex.quote(str(p / 'mail.eml')) + '\n')
    mta.chmod(0o700)
    c = json.loads((PROJECT / 'config.json').read_text())
    c.update(sendmail=str(mta), interval_seconds=2, alert_timeout_seconds=2, retry_seconds=1)
    (p / 'config.json').write_text(json.dumps(c))


def command(p, cmd, expected=0):
    r = subprocess.run([str(BINARY), '--demo', str(p / 'config.json'), cmd],
                       text=True, capture_output=True, timeout=20)
    assert r.returncode == expected, (cmd, r.returncode, r.stdout, r.stderr)
    return r


def logs(p):
    path = p / 'state/fim.log'
    if not path.exists() or not path.is_file():
        return []
    return [json.loads(x) for x in path.read_text().splitlines() if x.strip()]


def main():
    if not BINARY.is_file():
        raise SystemExit(f'Build the Rust binary first: {BINARY}')
    with tempfile.TemporaryDirectory(prefix='fim-cli-') as td:
        p = Path(td)
        setup(p)
        command(p, 'page')
        command(p, 'init')
        command(p, 'check')
        print('PASS CLI: init + clean check')
        good = (p / 'site/index.html').read_bytes()
        (p / 'site/index.html').write_text('invalid daily block')
        (p / 'site/new.php').write_text('new file')
        command(p, 'check', 1)
        text = json.dumps(logs(p))
        assert 'DAILY_INVALID' in text and 'new.php' in text and 'ADDED' in text
        print('PASS CLI: damaged index does not hide added file')
        (p / 'site/index.html').write_bytes(good)
        (p / 'site/new.php').unlink()
        command(p, 'check')
        assert any(e['kind'] == 'RECOVERED' for e in logs(p))
        print('PASS CLI: recovery')
        command(p, 'init', 2)
        print('PASS CLI: baseline overwrite refusal')

        # Block the fake MTA for much longer than the configured mail deadline.
        (p / 'sendmail-test').write_text('#!/bin/sh\nsleep 30\n')
        offset = len(logs(p))
        with (p / 'watch-stderr.txt').open('w') as err:
            proc = subprocess.Popen([str(BINARY), '--demo', str(p / 'config.json'), 'watch'],
                                    stdout=err, stderr=err)
            try:
                wait_for(lambda: any(e['kind'] == 'HEARTBEAT' for e in logs(p)[offset:]))
                original = (p / 'site/style.css').read_bytes()
                (p / 'site/style.css').write_bytes(b'transient modification')
                (p / 'site/style.css').write_bytes(original)
                wait_for(lambda: any(e['kind'] == 'FS_ACTIVITY' and 'style.css' in e['detail']
                                    for e in logs(p)[offset:]))
                print('PASS CLI: restored mutation still recorded and queued')
                (p / 'site/second.txt').write_text('new event while mail is blocked')
                wait_for(lambda: any(e['kind'] == 'INCIDENT' and 'second.txt' in e['detail']
                                    for e in logs(p)[offset:]))
                wait_for(lambda: any(e['kind'] == 'ALERT_FAILED' for e in logs(p)[offset:]))
                assert proc.poll() is None
                print('PASS CLI: mail timeout does not stop scanning')
            finally:
                proc.send_signal(signal.SIGTERM)
                try:
                    proc.wait(timeout=12)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=2)
                    raise AssertionError('Monitor did not shut down within deadline')
        assert proc.returncode in (0, 1)
    print('All CLI smoke scenarios passed. Inbox delivery was NOT tested.')


if __name__ == '__main__':
    main()
