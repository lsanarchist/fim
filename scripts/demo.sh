#!/bin/sh
# Demonstration on the deployed server (after deploy/install.sh). Run as root:
#   sudo scripts/demo.sh 2>&1 | tee docs/demo-output.txt
# In a terminal the script pauses at every ">>> SCREENSHOT" line.
set -u
CFG=/etc/fim/config.json
SITE=/var/www/fim
LOG=/var/log/fim/fim.log
BACKUP=$(mktemp -d)
WAIT=15 # a little more than interval_seconds, so fim.service has run a cycle

step() { printf '\n========== %s ==========\n' "$*"; }
mark() { MARK=$(wc -l < "$LOG"); }
new_log() { echo "--- new lines in $LOG:"; tail -n +"$((MARK + 1))" "$LOG"; }
wait_service() { echo "(waiting ${WAIT}s for fim.service)"; sleep "$WAIT"; }
check() {
    runuser -u fim -- /usr/local/bin/fim "$CFG" check > /dev/null
    code=$?
    echo "fim check -> exit code $code"
    return "$code"
}
shot() {
    echo ">>> SCREENSHOT: $*"
    if [ -t 0 ]; then printf '    Enter = continue '; read -r _; fi
}

step "0. Environment"
date
hostname
systemctl is-active nginx fim.service fim-page.timer
cp -a "$SITE/style.css" "$SITE/robots.txt" "$SITE/index.html" "$BACKUP/"

step "1. Page served by nginx"
curl -s http://localhost/ | grep 'FIM:daily'
shot "browser http://localhost/ with today's date and name day"

step "2. check on the clean site"
check || { echo "The site does not match the baseline; fix it before the demo."; exit 1; }
shot "terminal: exit code 0"

step "3. style.css modified"
mark
echo 'body { background: red }' >> "$SITE/style.css"
wait_service
new_log
check
shot "terminal with the INCIDENT line + Telegram message MODIFIED style.css"

step "4. file added and file deleted"
mark
echo '<?php system($_GET["c"]); ?>' > "$SITE/backdoor.php"
rm "$SITE/robots.txt"
wait_service
new_log
shot "Telegram messages ADDED backdoor.php and DELETED robots.txt"

step "5. forged daily block"
mark
sed -i 's/Meniny má [^.]*\./Meniny má Hacker./' "$SITE/index.html"
curl -s http://localhost/ | grep 'FIM:daily'
wait_service
new_log
shot "Telegram message DAILY_BLOCK index.html"

step "6. recovery (original files with original metadata)"
mark
rm "$SITE/backdoor.php"
cp -a "$BACKUP/style.css" "$BACKUP/robots.txt" "$BACKUP/index.html" "$SITE/"
wait_service
new_log
check
shot "RECOVERED lines + Telegram messages 'FIM: obnovené', exit code 0"

step "7. daily update by the timer (manual start of fim-page.service)"
mark
systemctl list-timers fim-page.timer --no-pager
systemctl start fim-page.service
curl -s http://localhost/ | grep 'FIM:daily'
wait_service
new_log
check
shot "PAGE_UPDATED, no INCIDENT, exit code 0"

rm -rf "$BACKUP"
