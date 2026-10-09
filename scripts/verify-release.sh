#!/usr/bin/env bash
# scripts/verify-release.sh

# Checks that a built controller runs on this host, which has nothing of ours
# installed: started without arguments, it must load — every library it needs
# is the host's — and refuse with its usage.
#
#   scripts/verify-release.sh <binary>
set -uo pipefail

BINARY="$1"
OUTPUT="$("${BINARY}" 2>&1)"
STATUS=$?
if [ "${STATUS}" -ne 1 ] || ! printf '%s' "${OUTPUT}" | grep -q 'usage: asterisk-controller'; then
  echo "ERROR: the controller did not run here (status ${STATUS}):" >&2
  printf '%s\n' "${OUTPUT}" >&2
  exit 1
fi
echo "The controller runs here"
