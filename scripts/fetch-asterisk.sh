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

RELEASE="22.11.0-r2"
BASE="https://github.com/gabion-dev/asterisk-server/releases/download/${RELEASE}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TREE="${ROOT}/target/asterisk-server"
STAMP="${TREE}/.release"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    ARCHIVE="asterisk-server-linux-amd64.tar.gz"
    SHA256="83f7f918042ce122318779359174b6ed30f9089b196882bb0350cc099780327d" ;;
  Linux-aarch64)
    ARCHIVE="asterisk-server-linux-arm64.tar.gz"
    SHA256="f4c8e08f5b2aa03067382b610e2d068d6758f4cb60cd97c9f5ddca38fe7cd334" ;;
  Darwin-arm64)
    ARCHIVE="asterisk-server-darwin-arm64.tar.gz"
    SHA256="dc3ba7e139a8e306b38eb287334a0fbb38333852078018726622238c5e22927f" ;;
  Darwin-x86_64)
    ARCHIVE="asterisk-server-darwin-amd64.tar.gz"
    SHA256="12644c64a7b372d06ccbf3df2106f51af8766226ef500ad4b11bdc0ce8183f5b" ;;
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
