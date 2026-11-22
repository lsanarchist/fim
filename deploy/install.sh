#!/bin/sh
# Installs FIM on a Debian/Kali server. Run as root from the repository:
#   cargo build --release && sudo deploy/install.sh
# The first run copies config.example.json to /etc/fim/config.json and stops,
# so you can fill in the Telegram token; then run the script again.
set -eu
cd "$(dirname "$0")/.."

[ "$(id -u)" = 0 ] || { echo "Run as root." >&2; exit 1; }
[ -x target/release/fim ] || { echo "Build first: cargo build --release" >&2; exit 1; }
command -v nginx >/dev/null || apt-get install -y nginx

# 1. Monitor user: may read the website, may not write it.
id fim >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin fim

# 2. Program, template and calendar (read-only, owned by root).
install -m 0755 target/release/fim /usr/local/bin/fim
install -d -m 0755 /usr/local/share/fim
install -m 0644 index.template.html namedays.json /usr/local/share/fim/

# 3. Website: owned by root, readable by everyone. Copied only on the first install:
#    re-running the script must not change monitored files.
if [ ! -d /var/www/fim ]; then
    install -d -m 0755 -o root -g root /var/www/fim
    install -p -m 0644 -o root -g root site/style.css site/robots.txt /var/www/fim/
fi

# 4. Private data outside the website: database and log belong to the monitor.
install -d -m 0750 -o fim -g fim /var/lib/fim /var/log/fim
[ -f /var/log/fim/fim.log ] || install -m 0640 -o fim -g fim /dev/null /var/log/fim/fim.log

# 5. Configuration with the token: root may edit it, the fim group may only read it.
if [ ! -f /etc/fim/config.json ]; then
    install -d -m 0755 /etc/fim
    install -m 0640 -o root -g fim config.example.json /etc/fim/config.json
    echo "Fill in telegram_bot_token and telegram_chat_id in /etc/fim/config.json, then run this script again."
    exit 0
fi

# 6. First page and baseline (the baseline is never overwritten).
/usr/local/bin/fim /etc/fim/config.json page
[ -f /var/lib/fim/baseline.db ] || runuser -u fim -- /usr/local/bin/fim /etc/fim/config.json init

# 7. nginx serves only /var/www/fim.
install -m 0644 deploy/nginx-fim.conf /etc/nginx/sites-available/fim
ln -sf /etc/nginx/sites-available/fim /etc/nginx/sites-enabled/fim
rm -f /etc/nginx/sites-enabled/default
nginx -t

# 8. systemd: monitor + midnight timer.
install -m 0644 deploy/fim.service deploy/fim-page.service deploy/fim-page.timer /etc/systemd/system/
systemctl daemon-reload
systemctl enable nginx fim.service fim-page.timer
systemctl restart nginx fim.service fim-page.timer
systemctl --no-pager status fim.service fim-page.timer | grep -E '^\S|Active:'
