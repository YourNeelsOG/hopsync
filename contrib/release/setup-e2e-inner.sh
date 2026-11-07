#!/bin/bash
# The checks setup-e2e.sh runs inside each container. Usage: setup-e2e-inner.sh CASE
set -u
fails=0
ok() { echo "ok   - $1"; }
bad() { echo "FAIL - $1"; fails=$((fails + 1)); }
check() { if eval "$2" >/dev/null 2>&1; then ok "$1"; else bad "$1 [$2]"; fi; }
SRC=/opt/incoming/hopsync

# Logs in over HTTP the way the browser does, as a check that the password really was set.
login() {
    local port=$1 pw=$2
    curl -s -o /dev/null -w '%{http_code}' -H "Host: 127.0.0.1:$port" -H "Origin: http://127.0.0.1:$port" \
        -H 'Content-Type: application/json' -d "{\"password\":\"$pw\"}" "http://127.0.0.1:$port/api/login"
}
needs_setup() { curl -s "http://127.0.0.1:$1/api/health" | grep -q '"setup_required":true'; }

root_systemd() {
    cp "$SRC" /tmp/hopsync-copy && chmod +x /tmp/hopsync-copy
    H=/tmp/hopsync-copy

    out=$(printf '%s\n' "$PW" | $H setup --plain --password-stdin --folder / 2>&1)
    [ $? -ne 0 ] && ok "refuses / as the transfer folder" || bad "accepted /: $out"
    out=$(printf '%s\n' "$PW" | $H setup --plain --password-stdin --folder '/srv/a b' 2>&1)
    [ $? -ne 0 ] && ok "refuses a path with a space, naming the rule" || bad "accepted a space: $out"

    out=$(printf '%s\n' short | $H setup --plain --password-stdin 2>&1); rc=$?
    [ "$rc" -ne 0 ] && echo "$out" | grep -q '12 characters' && ok "refuses a short password" || bad "short password: rc=$rc $out"

    out=$(printf '%s\n' "$PW" | $H setup --plain --password-stdin 2>&1); rc=$?
    [ "$rc" -eq 0 ] && ok "setup succeeds" || bad "setup failed: $out"
    echo "$out" | grep -q 'systemctl status hopsync' && ok "prints systemctl status hopsync" || bad "no status command: $out"
    echo "$out" | grep -q 'ssh -L 7890:127.0.0.1:7890' && ok "prints the tunnel command" || bad "no tunnel: $out"
    printf '%s' "$out" | grep -q "$(printf '\033')" && bad "escape codes in plain output" || ok "plain output has no escape codes"
    printf '%s' "$out" | grep -q "$PW" && bad "password in output" || ok "password not in output"

    check "systemctl status hopsync says active" "systemctl status hopsync --no-pager | grep -q 'active (running)'"
    check "the service is enabled at boot" "systemctl is-enabled hopsync"
    check "the binary went to /usr/local/bin" "test -x /usr/local/bin/hopsync"
    check "it runs as the unprivileged user" "test \"\$(systemctl show -p User --value hopsync)\" = hopsync"
    check "the process is not root" "test \"\$(ps -o user= -p \$(systemctl show -p MainPID --value hopsync))\" = hopsync"
    check "the default folder exists and is theirs" "test \"\$(stat -c %U /srv/hopsync)\" = hopsync"
    check "the setup token is gone" "test ! -e /var/lib/hopsync/data/setup-token"
    check "setup is no longer required" "! needs_setup 7890"
    check "the unit is marked as managed" "head -1 /etc/systemd/system/hopsync.service | grep -q 'Managed by hopsync setup'"
    check "the unit file is not world-writable" "test \$(stat -c %a /etc/systemd/system/hopsync.service) = 644"
    [ "$(login 7890 "$PW")" = 200 ] && ok "the password logs in" || bad "login failed"
    [ "$(login 7890 wrong-password-here)" = 401 ] && ok "a wrong password does not" || bad "wrong password accepted"
    journalctl -u hopsync --no-pager | grep -q "$PW" && bad "password in the journal" || ok "password not in the journal"
    check "transfers can write to the folder" "sudo -u hopsync touch /srv/hopsync/.probe && rm /srv/hopsync/.probe"

    pid=$(systemctl show -p MainPID --value hopsync)
    out=$($H setup --plain --password-stdin </dev/null 2>&1); rc=$?
    [ "$rc" -eq 0 ] && ok "running setup again works" || bad "second setup: $out"
    [ "$pid" = "$(systemctl show -p MainPID --value hopsync)" ] && ok "a second setup does not restart it, so jobs keep running" || bad "second setup restarted the service"
    [ "$(login 7890 "$PW")" = 200 ] && ok "the password survives a second setup" || bad "password lost"

    check "hopsync status works" "$H status | grep -qi 'active'"
    check "hopsync stop works" "$H stop && ! systemctl is-active hopsync"
    check "hopsync start works" "$H start && systemctl is-active hopsync"
    check "hopsync restart works" "$H restart && systemctl is-active hopsync"

    mkdir -p /srv/rootonly
    out=$(printf '%s\n' "$PW" | $H setup --plain --password-stdin --folder /srv/rootonly 2>&1); rc=$?
    [ "$rc" -ne 0 ] && echo "$out" | grep -q 'chown' && ok "a root-owned folder is refused, with the fix" || bad "root-owned folder: rc=$rc $out"
    check "that refusal left the service running" "systemctl is-active hopsync"

    useradd -m alice 2>/dev/null
    mkdir -p /home/alice/media && chown alice:alice /home/alice/media
    out=$($H setup --plain --password-stdin --folder /home/alice/media </dev/null 2>&1); rc=$?
    [ "$rc" -eq 0 ] && ok "a folder in someone's home works" || bad "home folder: $out"
    check "it now runs as that folder's owner" "test \"\$(systemctl show -p User --value hopsync)\" = alice"
    sleep 1
    check "and it is active" "systemctl is-active hopsync"
    check "and reads the folder through ProtectHome" "grep -q 'BindPaths=/home/alice/media' /etc/systemd/system/hopsync.service"

    out=$($H uninstall --plain --yes 2>&1); rc=$?
    [ "$rc" -eq 0 ] && ok "uninstall works" || bad "uninstall: $out"
    check "uninstall removed the unit" "test ! -e /etc/systemd/system/hopsync.service"
    check "uninstall stopped it" "! systemctl is-active hopsync"
    check "uninstall kept the data" "test -e /var/lib/hopsync/data"
    check "uninstall kept the transfer folder" "test -d /home/alice/media"
    echo "$out" | grep -q '/var/lib/hopsync' && ok "uninstall says where the data stayed" || bad "no data path: $out"

    printf '[Service]\nExecStart=/bin/true\n' >/etc/systemd/system/hopsync.service
    out=$(printf '%s\n' "$PW" | $H setup --plain --password-stdin 2>&1); rc=$?
    [ "$rc" -ne 0 ] && ok "refuses to replace a unit it did not write" || bad "replaced a foreign unit: $out"
    check "the foreign unit is untouched" "grep -q /bin/true /etc/systemd/system/hopsync.service"
    rm -f /etc/systemd/system/hopsync.service

    python3 -m http.server 7890 --bind 127.0.0.1 >/dev/null 2>&1 &
    decoy=$!
    sleep 1
    out=$(printf '%s\n' "$PW" | $H setup --plain --password-stdin --port 7890 2>&1); rc=$?
    [ "$rc" -ne 0 ] && ok "an explicit busy port is refused" || bad "busy port accepted: $out"
    out=$($H setup --plain --password-stdin </dev/null 2>&1); rc=$?
    [ "$rc" -eq 0 ] && echo "$out" | grep -q 'ssh -L 7891:127.0.0.1:7891' && ok "without --port it picks the next free one" || bad "no free port: rc=$rc $out"
    kill "$decoy"
    $H uninstall --plain --yes --purge >/dev/null 2>&1
    check "purge removed the data" "test ! -e /var/lib/hopsync/data"
}

sudo_user() {
    useradd -m bob 2>/dev/null
    echo 'bob ALL=(ALL) NOPASSWD:ALL' >/etc/sudoers.d/bob
    runuser -u bob -- mkdir -p /home/bob/.local/bin
    install -o bob -m 755 "$SRC" /home/bob/.local/bin/hopsync
    out=$(runuser -u bob -- sh -c "printf '%s\n' '$PW' | sudo /home/bob/.local/bin/hopsync setup --plain --password-stdin" 2>&1); rc=$?
    [ "$rc" -eq 0 ] && ok "sudo setup succeeds" || bad "sudo setup: $out"
    check "the system service is active" "systemctl is-active hopsync"
    check "the binary was copied out of the home folder" "grep -q 'ExecStart=/usr/local/bin/hopsync' /etc/systemd/system/hopsync.service"
    [ "$(login 7890 "$PW")" = 200 ] && ok "the password logs in" || bad "login failed"
}

# A shared seedbox: no root and no systemctl --user, because there is no login session.
shared_box() {
    useradd -m carol 2>/dev/null
    useradd -m dave 2>/dev/null
    runuser -u carol -- mkdir -p /home/carol/.local/bin
    install -o carol -m 755 "$SRC" /home/carol/.local/bin/hopsync
    H=/home/carol/.local/bin/hopsync
    as_carol() { runuser -u carol -- env -i HOME=/home/carol USER=carol PATH=/home/carol/.local/bin:/usr/bin:/bin LANG=C.UTF-8 "$@"; }
    as_carol sh -c '(crontab -l 2>/dev/null; echo "5 4 * * * /usr/bin/true # mine") | crontab -'

    # Another customer on the same box already holds the default port.
    runuser -u dave -- python3 -m http.server 7890 --bind 127.0.0.1 >/dev/null 2>&1 &
    sleep 1

    out=$(printf '%s\n' "$PW" | as_carol hopsync setup --plain --password-stdin 2>&1); rc=$?
    [ "$rc" -eq 0 ] && ok "setup succeeds without root or systemd" || bad "setup failed: $out"
    echo "$out" | grep -q 'hopsync status' && ok "prints hopsync status" || bad "no status command: $out"
    echo "$out" | grep -q 'sudo' && ok "mentions the sudo option" || bad "no sudo hint: $out"
    echo "$out" | grep -q 'ssh -L 7891:127.0.0.1:7891' && ok "skips the other customer's port" || bad "port: $out"
    port=7891
    [ "$(login "$port" "$PW")" = 200 ] && ok "the password logs in" || bad "login failed"
    [ "$(login 7890 "$PW")" = 000 ] || curl -s http://127.0.0.1:7890/ | grep -qv hopsync && ok "the other customer's port was left alone" || bad "decoy changed"
    check "the daemon runs as carol" "test \"\$(ps -o user= -C hopsync | sort -u)\" = carol"
    check "the daemon is its own session, so logout cannot stop it" "test \"\$(ps -o sid= -C hopsync | tr -d ' ')\" = \"\$(ps -o pid= -C hopsync | tr -d ' ')\""
    check "the data is private" "test \$(stat -c %a /home/carol/.local/share/hopsync) = 700"
    check "the setup token is gone" "test ! -e /home/carol/.local/share/hopsync/setup-token"
    grep -q "$PW" /home/carol/.local/share/hopsync/hopsync.log && bad "password in the log" || ok "password not in the log"
    check "the log is private" "test \$(stat -c %a /home/carol/.local/share/hopsync/hopsync.log) = 600"
    check "cron restarts it after a reboot" "as_carol crontab -l | grep -q '@reboot.*hopsync start'"
    check "the existing cron job is kept" "as_carol crontab -l | grep -q '# mine'"

    check "hopsync status says running" "as_carol hopsync status | grep -qi running"
    pid=$(pgrep -u carol -x hopsync)
    out=$(as_carol hopsync setup --plain --password-stdin </dev/null 2>&1); rc=$?
    [ "$rc" -eq 0 ] && ok "running setup again works" || bad "second setup: $out"
    [ "$pid" = "$(pgrep -u carol -x hopsync)" ] && ok "a second setup does not restart it" || bad "second setup restarted it"
    [ "$(as_carol crontab -l | grep -c '@reboot.*hopsync start')" = 1 ] && ok "cron has one line after two setups" || bad "duplicate cron line"

    check "hopsync stop works" "as_carol hopsync stop && ! pgrep -u carol -x hopsync"
    check "hopsync status says stopped" "as_carol hopsync status | grep -qi stopped"
    check "stop twice is fine" "as_carol hopsync stop"
    check "hopsync start works" "as_carol hopsync start && pgrep -u carol -x hopsync"
    [ "$(login "$port" "$PW")" = 200 ] && ok "the password survives a restart" || bad "password lost"
    old=$(pgrep -u carol -x hopsync)
    check "hopsync restart works" "as_carol hopsync restart"
    [ "$old" != "$(pgrep -u carol -x hopsync)" ] && ok "restart made a new process" || bad "restart kept the process"
    check "hopsync logs prints the log" "as_carol hopsync logs --no-follow | grep -q 'web UI and API'"

    # A pid file pointing at someone else's process must never get that process killed.
    as_carol hopsync stop
    sleep 300 &
    other=$!
    echo "$other" >/home/carol/.local/share/hopsync/hopsync.pid
    chown carol /home/carol/.local/share/hopsync/hopsync.pid
    as_carol hopsync stop >/dev/null 2>&1
    kill -0 "$other" 2>/dev/null && ok "a stale pid file cannot kill another process" || bad "killed an unrelated process"
    kill "$other"

    as_carol hopsync start
    out=$(as_carol hopsync uninstall --plain --yes 2>&1); rc=$?
    [ "$rc" -eq 0 ] && ok "uninstall works" || bad "uninstall: $out"
    check "uninstall stopped it" "! pgrep -u carol -x hopsync"
    check "uninstall removed only its cron line" "! as_carol crontab -l | grep -q hopsync && as_carol crontab -l | grep -q '# mine'"
    check "uninstall kept the data" "test -d /home/carol/.local/share/hopsync"
    pkill -u dave python3
}

user_manager() {
    useradd -m erin 2>/dev/null
    runuser -u erin -- mkdir -p /home/erin/.local/bin
    install -o erin -m 755 "$SRC" /home/erin/.local/bin/hopsync
    loginctl enable-linger erin
    uid=$(id -u erin)
    for i in $(seq 1 40); do [ -S "/run/user/$uid/bus" ] && break; sleep 0.25; done
    as_erin() { runuser -u erin -- env HOME=/home/erin USER=erin XDG_RUNTIME_DIR="/run/user/$uid" PATH=/home/erin/.local/bin:/usr/bin:/bin "$@"; }
    if ! as_erin systemctl --user show-environment >/dev/null 2>&1; then
        echo "skip - this container has no working user manager"
        return
    fi
    out=$(printf '%s\n' "$PW" | as_erin hopsync setup --plain --password-stdin 2>&1); rc=$?
    [ "$rc" -eq 0 ] && ok "setup succeeds with a user service" || bad "setup failed: $out"
    echo "$out" | grep -q 'systemctl --user status hopsync' && ok "prints the --user status command" || bad "no command: $out"
    check "the user service is active" "as_erin systemctl --user is-active hopsync"
    check "the user service is enabled" "as_erin systemctl --user is-enabled hopsync"
    [ "$(login 7890 "$PW")" = 200 ] && ok "the password logs in" || bad "login failed"
    check "hopsync status works" "as_erin hopsync status | grep -qi active"
    out=$(as_erin hopsync uninstall --plain --yes 2>&1); rc=$?
    [ "$rc" -eq 0 ] && ok "uninstall works" || bad "uninstall: $out"
    check "uninstall removed the unit" "test ! -e /home/erin/.config/systemd/user/hopsync.service"
}

"$1"
echo "failures: $fails"
exit "$fails"
