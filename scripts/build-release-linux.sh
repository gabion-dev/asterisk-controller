#!/usr/bin/env bash
# scripts/build-release-linux.sh

# Builds and checks the controller for Linux, inside AlmaLinux 9 — the base
# the Asterisk tree is built on — and puts the binary into <out>.
#
#   docker run --rm -v "$PWD:/src:ro" -v "$PWD/out:/out" almalinux:9 \
#     bash /src/scripts/build-release-linux.sh /src /out
#
# glibc is compatible only upward, so a binary built on glibc 2.34 runs on any
# host the Asterisk tree runs on: the node's platforms are one fact. Before the
# binary is kept, every check of the repository runs here — the controller
# against the pinned Asterisk included — and the newest glibc symbol version
# the binary needs is held to that floor.
set -euo pipefail

SRC="$1"
OUT="$2"
FLOOR="2.34"

# A C compiler for ring, `kill` for the test that renews the node's
# certificate, `readelf` for the floor.
dnf install -y --setopt=install_weak_deps=False gcc util-linux binutils tar gzip >/dev/null

# The checks run as an ordinary user, as a node runs: a builder of its own,
# with a copy of the checkout it can write to.
useradd --create-home builder
mkdir /home/builder/src
tar -C "${SRC}" --exclude=./target --exclude=./java/build --exclude=./java/.gradle -cf - . \
  | tar -C /home/builder/src -xf -
chown -R builder: /home/builder/src

su builder -s /bin/bash -c '
  set -euo pipefail
  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain none
  . "${HOME}/.cargo/env"
  cd "${HOME}/src"
  # The toolchain pinned in rust-toolchain.toml is installed on first use.
  cargo --version
  cargo fmt --all --check
  cargo clippy --all-targets --locked
  bash scripts/fetch-asterisk.sh
  cargo test --locked
  cargo build --release --locked -p asterisk-controller
'

BINARY=/home/builder/src/target/release/asterisk-controller
NEEDED="$(readelf -V "${BINARY}" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -1)"
if [ "$(printf '%s\n%s\n' "${NEEDED}" "${FLOOR}" | sort -V | tail -1)" != "${FLOOR}" ]; then
  echo "ERROR: the binary needs glibc ${NEEDED}; the node's floor is ${FLOOR}" >&2
  exit 1
fi
echo "The binary needs glibc ${NEEDED} at most; the floor is ${FLOOR}"

mkdir -p "${OUT}"
cp "${BINARY}" "${OUT}/asterisk-controller"
