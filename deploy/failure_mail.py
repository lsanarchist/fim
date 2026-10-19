#!/usr/bin/env python3
"""Supervisor fallback: works even if the FIM executable, baseline or state DB is broken."""
import json
import os
import signal
import subprocess
import sys
from email.message import EmailMessage
from pathlib import Path

CONFIG_LIMIT = 1024 * 1024


def load_config(path: Path) -> dict:
    with path.open('rb') as handle:
        content = handle.read(CONFIG_LIMIT + 1)
    if len(content) > CONFIG_LIMIT:
        raise ValueError('Configuration exceeds 1 MiB')
    config = json.loads(content)
    # Match the Rust application's config-relative path policy, including
    # conventional sendmail alternatives symlinks. Never resolve via PATH.
    transport = Path(os.path.abspath(path.parent / config['sendmail']))
    config['sendmail'] = str(transport.resolve(strict=True))
    return config


def notify_failure(config_path: Path, unit: str) -> None:
    config = load_config(config_path)
    mail = EmailMessage()
    mail['From'] = config['mail_from']
    mail['To'] = config['mail_to']
    mail['Subject'] = 'FIM SERVICE FAILURE'
    mail.set_content(f'FIM service failed: {unit}\nInspect systemctl status and journalctl.\n'
                     'This supervisor notification does not rely on the FIM baseline or outbox.\n')
    proc = subprocess.Popen([config['sendmail'], '-i', '-t'], stdin=subprocess.PIPE,
                            start_new_session=True)
    try:
        proc.communicate(mail.as_bytes(), timeout=10)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            pass
        raise RuntimeError('Supervisor mail timeout; inspect journald/independent monitoring')
    if proc.returncode:
        raise RuntimeError(f'Supervisor sendmail exit {proc.returncode}')


if __name__ == '__main__':
    notify_failure(Path('/opt/fim/config.json'), sys.argv[1] if len(sys.argv) > 1 else 'unknown')
