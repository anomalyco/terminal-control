#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TERMCTRL_REAL_ZIG=$(command -v zig)
export TERMCTRL_REAL_ZIG
export PATH="$root/scripts/portable-zig:$PATH"
cd "$root"
# PATH is not a tracked input of the dependency's build script. Discard only its release
# outputs so a previous host-native archive cannot silently survive a portable rebuild.
cargo clean --release -p libghostty-vt-sys
cargo build --release "$@"
