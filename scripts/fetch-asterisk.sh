#!/usr/bin/env bash
# scripts/fetch-asterisk.sh

# Puts the Asterisk build the controller is checked against into
# target/asterisk-server.
#
#   scripts/fetch-asterisk.sh
#
# The release and the checksums of its four archives are pinned in the
# controller itself (crates/asterisk-controller/src/asterisk_release.rs), and
# the tree is put in place by the controller's own `tree` mode — the way a
# telephony node puts it in place. A machine the tree would not run on is told
# what it lacks; an archive that differs is refused; a tree already stamped
# with the pinned release is kept as it is.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"
cargo run --quiet --locked -p asterisk-controller -- tree "${ROOT}/target/asterisk-server"
