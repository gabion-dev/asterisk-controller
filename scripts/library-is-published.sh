#!/usr/bin/env bash
# scripts/library-is-published.sh

# Says whether the Java library of this checkout is the one Maven Central has
# under the library's version (java/build.gradle.kts):
#
#   scripts/library-is-published.sh
#
# Status 0 — published, and its sources are exactly this checkout's: a
# controller release may go out, it speaks the protocol of that library.
# Status 1 — published, but this checkout's sources differ: the protocol, the
# generator of its types or the hand-written part changed. The library's
# version is raised and the library released first (the Library workflow).
# Status 3 — that version is not on Central yet.
#
# The library's sources jar is built here — its types generated from the
# protocol description, its hand-written part, the description itself — and
# compared file by file with the sources jar published on Central. Needs the
# JDK and cargo the library's build needs, and access to repo1.maven.org.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "${ROOT}/java/build.gradle.kts")"
[ -n "${VERSION}" ] || { echo "ERROR: no version in java/build.gradle.kts" >&2; exit 1; }
BASE="https://repo1.maven.org/maven2/dev/gabion/node-protocol/${VERSION}"

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

STATUS="$(curl --silent --output "${WORK}/published.jar" --write-out '%{http_code}' --location \
  "${BASE}/node-protocol-${VERSION}-sources.jar")"
case "${STATUS}" in
  200) ;;
  404) echo "dev.gabion:node-protocol:${VERSION} is not on Maven Central"; exit 3 ;;
  *) echo "ERROR: Maven Central answered ${STATUS} for the sources of ${VERSION}" >&2; exit 1 ;;
esac

(cd "${ROOT}/java" && ./gradlew --quiet --console=plain sourcesJar)
mkdir "${WORK}/published" "${WORK}/here"
(cd "${WORK}/published" && unzip -q ../published.jar)
(cd "${WORK}/here" && unzip -q "${ROOT}/java/build/libs/node-protocol-java-${VERSION}-sources.jar")
if diff -r "${WORK}/published" "${WORK}/here" > "${WORK}/difference"; then
  echo "dev.gabion:node-protocol:${VERSION} on Maven Central is this checkout's library"
  exit 0
fi
echo "dev.gabion:node-protocol:${VERSION} on Maven Central is not this checkout's library:" >&2
head -40 "${WORK}/difference" >&2
echo "Raise the library's version in java/build.gradle.kts and release it first (the Library workflow)." >&2
exit 1
