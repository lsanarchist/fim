#!/bin/sh
# Fresh Debian/Ubuntu systemd install only. Never replace a previous trusted baseline.
set -eu
[ "$(id -u)" -eq 0 ] || { echo 'Run with sudo.' >&2; exit 1; }
cd "$(dirname "$0")/.."
[ -x target/release/fim ] || { echo 'Build first: cargo build --release' >&2; exit 1; }
[ ! -e /opt/fim/config.json ] || { echo 'Existing installation: review/migrate manually, not overwritten.' >&2; exit 1; }
[ ! -e /var/www/fim/site/index.html ] || { echo 'Existing website: not overwritten.' >&2; exit 1; }
command -v python3 >/dev/null
command -v systemctl >/dev/null
id fim >/dev/null 2>&1 || useradd --system --user-group --home-dir /var/lib/fim --shell /usr/sbin/nologin fim
[ "$(id -u fim)" -ne 0 ] || { echo 'fim must not be root.' >&2; exit 1; }
install -d -o root -g root -m 0755 /opt/fim /var/www/fim
install -d -o fim -g fim -m 0755 /var/www/fim/site
install -d -o fim -g fim -m 0700 /var/lib/fim /var/www/fim/staging
install -o root -g root -m 0755 target/release/fim /opt/fim/fim
install -o root -g root -m 0644 namedays.json index.template.html /opt/fim/
install -o fim -g fim -m 0644 site/style.css /var/www/fim/site/style.css
python3 - <<'PY'
import json
from pathlib import Path
c=json.loads(Path('config.json').read_text())
c.update(root='/var/www/fim/site', database='/opt/fim/baseline.db',
         calendar='/opt/fim/namedays.json', template='/opt/fim/index.template.html',
         state_dir='/var/lib/fim', staging_dir='/var/www/fim/staging')
p=Path('/opt/fim/config.json'); p.write_text(json.dumps(c,indent=2)+'\n'); p.chmod(0o644)
PY
install -o root -g root -m 0755 deploy/failure_mail.py /opt/fim/failure_mail.py
install -o root -g root -m 0644 deploy/*.service deploy/*.timer /etc/systemd/system/
systemctl daemon-reload
cat <<'TEXT'
Installed, NOT started. Configure mail_from/mail_to and a working local sendmail MTA.
Then:
  sudo -u fim /opt/fim/fim /opt/fim/config.json alert-test
  sudo -u fim /opt/fim/fim /opt/fim/config.json page
  sudo /opt/fim/fim /opt/fim/config.json init
  sudo systemctl enable --now fim.service fim-page.timer
Publish ONLY /var/www/fim/site in your web server. Do not make www-data a member of fim.
TEXT
