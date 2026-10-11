#!/usr/bin/env python3
"""Supervisor fallback: works even if the FIM executable, baseline or state DB is broken."""
import json
import os
import signal
import subprocess
import sys
from email.message import EmailMessage
from pathlib import Path


def main() -> None:
    config = json.loads(Path('/opt/fim/config.json').read_text(encoding='utf-8'))
    mail = EmailMessage()
    mail['From'] = config['mail_from']
    mail['To'] = config['mail_to']
    mail['Subject'] = 'FIM SERVICE FAILURE'
    unit = sys.argv[1] if len(sys.argv) > 1 else 'unknown'
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
    main()
