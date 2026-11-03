#!/usr/bin/env bash
# scripts/check-install.sh

# Installs a controller binary as a telephony node on the machine it runs on
# and judges the service systemd runs:
#
#   sudo scripts/check-install.sh <controller binary>
#
# - an install from nothing, the secret on stdin, ends in a running node:
#   both halves ready, the node's files owned by its user and readable by it
#   alone;
# - SIGHUP (systemctl reload) reaches the controller;
# - a launcher that dies is started again by systemd, and its halves go with
#   it;
# - an install again, without a secret, keeps the secret and the certificate;
# - a secret typed at a terminal is asked for and not shown;
# - a stop ends Asterisk first, then the controller, then the launcher, each
#   with status 0.
#
# Needs root and systemd, and changes the machine: a throwaway one — the
# Checks workflow runs it on a fresh runner. The application it names does
# not exist; the node keeps looking for it, as a node does.
set -euo pipefail

BINARY="$1"
UNIT=gabion-telephony
STATE=/var/lib/gabion-telephony

fail() {
  echo "ERROR: $1" >&2
  journalctl -u "${UNIT}" --no-pager -o cat | tail -60 >&2
  exit 1
}

# The journal's position now: what a step waits for must come after it. The
# whole journal's, not the unit's — before the first install the unit has no
# lines, and no position.
cursor() {
  local position
  position="$(journalctl -n 0 --show-cursor --no-pager | sed -n 's/^-- cursor: //p')"
  [ -n "${position}" ] || fail "the journal gives no position"
  echo "${position}"
}

# Wait up to a minute for a line of the unit's journal after <cursor>.
wait_for() {
  local after="$1" pattern="$2" what="$3"
  for _ in $(seq 1 120); do
    if journalctl -u "${UNIT}" --after-cursor "${after}" --no-pager -o cat | grep -q -- "${pattern}"; then
      return 0
    fi
    sleep 0.5
  done
  fail "${what}: no line matching ${pattern} within 60 seconds"
}

install_node() {
  "${BINARY}" install --node check --application wss://application.invalid/api/telephony/node \
    --public 192.0.2.10 "$@"
}

echo "== an install from nothing"
START="$(cursor)"
echo "secret-of-the-check" | install_node --secret-from-stdin
wait_for "${START}" '"event":"controller-ready"' "the controller was not ready"
wait_for "${START}" '"event":"asterisk-ready"' "Asterisk was not ready"
[ "$(systemctl is-active "${UNIT}")" = active ] || fail "the service is not active"
for file in "${STATE}/node-secret" "${STATE}/tls/node.crt" "${STATE}/tls/node.key"; do
  [ "$(stat -c '%U %a' "${file}")" = "gabion-telephony 600" ] || fail "${file}: $(stat -c '%U %a' "${file}")"
done
[ "$(stat -c '%U %a' "${STATE}")" = "gabion-telephony 700" ] || fail "${STATE}: $(stat -c '%U %a' "${STATE}")"
[ "$(cat "${STATE}/node-secret")" = "secret-of-the-check" ] || fail "the secret is not the one given"
if ps -eo args | grep -v grep | grep -q "secret-of-the-check"; then
  fail "the secret is on a command line"
fi

echo "== SIGHUP reaches the controller"
AT="$(cursor)"
systemctl reload "${UNIT}"
wait_for "${AT}" "SIGHUP" "the controller did not take SIGHUP"

echo "== a launcher that dies is started again"
AT="$(cursor)"
RESTARTS="$(systemctl show -p NRestarts --value "${UNIT}")"
kill -KILL "$(systemctl show -p MainPID --value "${UNIT}")"
wait_for "${AT}" '"event":"asterisk-ready"' "the node did not come back"
[ "$(systemctl show -p NRestarts --value "${UNIT}")" -gt "${RESTARTS}" ] || fail "systemd did not restart it"
ASTERISKS="$(pgrep -c -u gabion-telephony -x asterisk || true)"
[ "${ASTERISKS}" = 1 ] || fail "${ASTERISKS} Asterisk processes after the restart, not 1"

echo "== an install again keeps the secret and the certificate"
KEPT="$(sha256sum "${STATE}/node-secret" "${STATE}/tls/node.crt" "${STATE}/tls/node.key")"
AT="$(cursor)"
install_node
wait_for "${AT}" '"event":"asterisk-ready"' "the node did not come back after the install"
[ "$(sha256sum "${STATE}/node-secret" "${STATE}/tls/node.crt" "${STATE}/tls/node.key")" = "${KEPT}" ] \
  || fail "the secret or the certificate changed"

echo "== a secret typed at a terminal is asked for and not shown"
# The installer is run on a terminal of its own; the secret arrives a moment
# later, as a person would paste it after the question.
AT="$(cursor)"
TRANSCRIPT="$(mktemp)"
COMMAND="$(printf '%q ' "${BINARY}" install --node check \
  --application wss://application.invalid/api/telephony/node --public 192.0.2.10 --secret-from-stdin)"
(sleep 3; printf 'typed-secret-of-the-check\n') | script -qec "${COMMAND}" "${TRANSCRIPT}" >/dev/null \
  || fail "the install on a terminal failed: $(cat "${TRANSCRIPT}")"
grep -q "the node's secret (not shown)" "${TRANSCRIPT}" || fail "the installer did not ask for the secret"
if grep -q "typed-secret-of-the-check" "${TRANSCRIPT}"; then
  fail "the secret was shown on the terminal"
fi
[ "$(cat "${STATE}/node-secret")" = "typed-secret-of-the-check" ] || fail "the typed secret is not the node's"
wait_for "${AT}" '"event":"asterisk-ready"' "the node did not come back after the install on a terminal"

echo "== a stop: Asterisk first, then the controller, each with status 0"
AT="$(cursor)"
systemctl stop "${UNIT}"
STOPS="$(journalctl -u "${UNIT}" --after-cursor "${AT}" --no-pager -o cat | grep '"event":"stopped"' || true)"
echo "${STOPS}"
[ "$(echo "${STOPS}" | sed -n 1p)" = '{"event":"stopped","half":"asterisk","code":0,"signal":null}' ] \
  || fail "Asterisk did not stop first with status 0"
[ "$(echo "${STOPS}" | sed -n 2p)" = '{"event":"stopped","half":"controller","code":0,"signal":null}' ] \
  || fail "the controller did not stop second with status 0"
[ "$(systemctl show -p ExecMainStatus --value "${UNIT}")" = 0 ] || fail "the launcher did not end with 0"
[ "$(systemctl show -p Result --value "${UNIT}")" = success ] || fail "systemd did not count the stop a success"
pgrep -u gabion-telephony >/dev/null && fail "processes of the node are left"

echo "The install, the service and its stop are as the launcher's rule says"
