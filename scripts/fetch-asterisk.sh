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

RELEASE="22.11.0-r1"
BASE="https://github.com/gabion-dev/asterisk-server/releases/download/${RELEASE}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TREE="${ROOT}/target/asterisk-server"
STAMP="${TREE}/.release"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    ARCHIVE="asterisk-server-linux-amd64.tar.gz"
    SHA256="3898749a64140deb1c161202b9f141f13e6a398e9a8fb5ad9e796001ae5d62cc" ;;
  Linux-aarch64)
    ARCHIVE="asterisk-server-linux-arm64.tar.gz"
    SHA256="3856472dbb2b202bdce285e3d7360b0fb7212a7d50ded3a86dd9c982b15033b7" ;;
  Darwin-arm64)
    ARCHIVE="asterisk-server-darwin-arm64.tar.gz"
    SHA256="bc10200c4ef0e85a88f7ce27708df1c4707758b3440dbeee0cb558286fbfc950" ;;
  Darwin-x86_64)
    ARCHIVE="asterisk-server-darwin-amd64.tar.gz"
    SHA256="483543937bfe8723759437daab86a3124f1df3ac5a321ee16f7587049f36f4db" ;;
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
