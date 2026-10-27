#!/bin/sh
# Runs the checks of CI on this machine, and the firmware build that CI does not run for a pull request.
#
# Usage: sh scripts/check.sh [--esp32 | --no-esp32]
#
# The script builds the firmware when the branch changes esp32/, crates/ or webui/.
# --esp32 always builds the firmware. --no-esp32 never builds it.
# The firmware build needs the esp toolchain and ~/export-esp.sh (see AGENTS.md).
set -eu

cd "$(git rev-parse --show-toplevel)"

esp32=auto
case "${1:-}" in
  --esp32) esp32=yes ;;
  --no-esp32) esp32=no ;;
  "") ;;
  *) echo "usage: sh scripts/check.sh [--esp32 | --no-esp32]" >&2; exit 2 ;;
esac

if [ ! -f vendor/rusty_link/Cargo.toml ]; then
  echo "vendor/rusty_link is missing. Do the steps in AGENTS.md, section \"Vendor Setup\"." >&2
  exit 1
fi

step() { printf '\n==> %s\n' "$*"; }

step "Format"
cargo fmt --all --check
rustfmt --check --edition 2021 esp32/build.rs esp32/src/main.rs

step "Clippy"
cargo clippy --locked --all-targets -- -D warnings

step "Tests"
cargo test --locked

if [ "$esp32" = auto ]; then
  esp32=no
  base=$(git merge-base HEAD origin/main 2>/dev/null || true)
  if [ -n "$base" ] && git diff --name-only "$base" HEAD -- esp32 crates webui | grep -q .; then
    esp32=yes
  fi
fi

if [ "$esp32" = yes ]; then
  if [ ! -f "$HOME/export-esp.sh" ]; then
    echo "The branch changes the firmware, but ~/export-esp.sh is missing. Install the esp toolchain (see AGENTS.md)." >&2
    exit 1
  fi
  # export-esp.sh sets PATH and LIBCLANG_PATH for the esp toolchain.
  . "$HOME/export-esp.sh"
  step "Firmware"
  (cd esp32 && cargo build --release --locked)
  step "Firmware with the DMX output"
  (cd esp32 && cargo build --release --locked --features dmx)
else
  step "Firmware: skipped. The branch does not change esp32/, crates/ or webui/."
fi

step "All checks passed"
