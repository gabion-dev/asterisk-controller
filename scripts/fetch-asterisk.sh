#!/usr/bin/env bash
# scripts/fetch-asterisk.sh

# Fetches the Asterisk build the controller is checked against into
# target/asterisk-server.
#
#   scripts/fetch-asterisk.sh
#
# The release and the checksums of its four archives are pinned here: this is
# the Asterisk this controller is known to work with. An archive that differs
# is refused. A tree that is already there and stamped with the pinned release
# is kept as it is.
set -euo pipefail

RELEASE="22.11.0-r3"
BASE="https://github.com/gabion-dev/asterisk-server/releases/download/${RELEASE}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TREE="${ROOT}/target/asterisk-server"
STAMP="${TREE}/.release"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    ARCHIVE="asterisk-server-linux-amd64.tar.gz"
    SHA256="28145d8df5909528580bc04abff350af770f2e4086bae0084d978352ad463a21" ;;
  Linux-aarch64)
    ARCHIVE="asterisk-server-linux-arm64.tar.gz"
    SHA256="f43c5a4eb868d9ec64c3cc0ee15e44615c13923b0af050aaf3f6d3e2da2b5ba4" ;;
  Darwin-arm64)
    ARCHIVE="asterisk-server-darwin-arm64.tar.gz"
    SHA256="dad28302be12f39f687e509f45a4de7fa3396d0514524cc681d2e3392633aca4" ;;
  Darwin-x86_64)
    ARCHIVE="asterisk-server-darwin-amd64.tar.gz"
    SHA256="56791a5a2f1c04ad5eabcb94d9e8e5055885101454ca3d2c7a141520b731e676" ;;
  *)
    echo "ERROR: no Asterisk build for $(uname -s) $(uname -m)" >&2
    exit 1 ;;
esac

if [ -f "${STAMP}" ] && [ "$(cat "${STAMP}")" = "${RELEASE}" ] && [ -x "${TREE}/sbin/asterisk" ]; then
  echo "Asterisk ${RELEASE} is already in ${TREE}"
  exit 0
fi

rm -rf "${TREE}"
mkdir -p "${TREE}"
DOWNLOAD="${ROOT}/target/${ARCHIVE}"
curl --fail --location --silent --show-error -o "${DOWNLOAD}" "${BASE}/${ARCHIVE}"

if command -v sha256sum > /dev/null; then
  ACTUAL="$(sha256sum "${DOWNLOAD}" | cut -d' ' -f1)"
else
  ACTUAL="$(shasum -a 256 "${DOWNLOAD}" | cut -d' ' -f1)"
fi
if [ "${ACTUAL}" != "${SHA256}" ]; then
  echo "ERROR: ${ARCHIVE} does not match its pinned checksum" >&2
  echo "  expected ${SHA256}" >&2
  echo "  got      ${ACTUAL}" >&2
  rm -f "${DOWNLOAD}"
  exit 1
fi

tar -xzf "${DOWNLOAD}" -C "${TREE}"
rm -f "${DOWNLOAD}"
echo "${RELEASE}" > "${STAMP}"
echo "Asterisk ${RELEASE} is in ${TREE}"
