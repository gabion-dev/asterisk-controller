#!/usr/bin/env bash
# java-check/run.sh

# Proves that the Java side of the protocol holds: generates the Java library
# from the protocol description, compiles it with a real Java compiler against
# the same Jackson the Gabion framework uses, and runs it through the shared
# vectors — the same files the Rust tests read.
#
#   java-check/run.sh
#
# Needs a JDK 21 (javac, java) and cargo. The four library jars are taken from
# Maven Central and checked against pinned SHA-256 sums; a jar that differs is
# refused. They are kept under target/ and not downloaded twice.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${HERE}/.." && pwd)"
WORK="${ROOT}/target/java-check"
PACKAGE="dev.gabion.check.protocol"
CENTRAL="https://repo1.maven.org/maven2"

# group path, artifact, version, sha256
JARS=(
  "tools/jackson/core jackson-databind 3.0.3 dfbb79130910decf063125c1e693a39c5ec8fb8b478d7f39671901d47ce7f6a2"
  "tools/jackson/core jackson-core 3.0.3 4149bb2b3bbffb5339089174e26f454081e6a01a104d24b22f78a16b948799eb"
  "com/fasterxml/jackson/core jackson-annotations 2.20 959a2ffb2d591436f51f183c6a521fc89347912f711bf0cae008cdf045d95319"
  "org/jspecify jspecify 1.0.0 1fad6e6be7557781e4d33729d49ae1cdc8fdda6fe477bb0cc68ce351eafdfbab"
)

mkdir -p "${WORK}/lib"
CLASSPATH=""
for entry in "${JARS[@]}"; do
  read -r group artifact version sha256 <<<"${entry}"
  jar="${WORK}/lib/${artifact}-${version}.jar"
  if [ ! -f "${jar}" ]; then
    curl --fail --location --silent --show-error \
      -o "${jar}" "${CENTRAL}/${group}/${artifact}/${version}/${artifact}-${version}.jar"
  fi
  if ! echo "${sha256}  ${jar}" | sha256sum --check --status; then
    echo "ERROR: ${artifact}-${version}.jar does not match its pinned checksum" >&2
    rm -f "${jar}"
    exit 1
  fi
  CLASSPATH="${CLASSPATH}${jar}:"
done

echo "=== Generate the Java library ==="
rm -rf "${WORK}/generated" "${WORK}/classes"
mkdir -p "${WORK}/classes"
cargo run --quiet --manifest-path "${ROOT}/Cargo.toml" -p protocol-java -- \
  "${ROOT}/protocol/node-protocol.schema.json" "${PACKAGE}" "${WORK}/generated"

echo "=== Compile ==="
# Every warning the compiler knows is an error: the generated code is held to
# the same standard as code written by hand.
find "${WORK}/generated" "${HERE}" -name '*.java' > "${WORK}/sources.txt"
javac --release 21 -Xlint:all -Werror -proc:none \
  -cp "${CLASSPATH}" -d "${WORK}/classes" @"${WORK}/sources.txt"
# The protocol description travels as a resource next to the classes.
PACKAGE_DIR="${PACKAGE//.//}"
cp "${WORK}/generated/${PACKAGE_DIR}/node-protocol.schema.json" "${WORK}/classes/${PACKAGE_DIR}/"

echo "=== Shared vectors ==="
java -cp "${CLASSPATH}${WORK}/classes" VectorCheck "${PACKAGE}" \
  "${ROOT}/protocol/messages.vectors.json" "${ROOT}/protocol/audio-frames.vectors.json"
