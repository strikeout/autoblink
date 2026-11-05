#!/bin/sh
# Puts the local firmware build on the box over WiFi (OTA). No USB cable is necessary.
#
# Usage: sh scripts/ota-push.sh [HOST]      (default HOST: autoblink.local)
#
# The script builds the firmware and makes the OTA image. It puts the image and its version into
# the prerelease esp32-latest on GitHub. Then it sends ota_install to the box and waits for the
# new version to start.
#
# The box installs only a version that is newer than its version. Increase the version in
# Cargo.toml and esp32/Cargo.toml first.
#
# WARNING: each box that does a check gets this build from esp32-latest. The next tagged release
# replaces it.
#
# A change to the partition table, the bootloader or the bootloader settings of sdkconfig needs
# USB. OTA writes only the app.
set -eu

host=${1:-autoblink.local}
cd "$(git rev-parse --show-toplevel)"
. "$HOME/export-esp.sh"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' esp32/Cargo.toml | head -1)
out=.context/ota
mkdir -p "$out"

(cd esp32 && cargo build --release)
espflash save-image --chip esp32 esp32/target/xtensa-esp32-espidf/release/autoblink-esp32 \
  "$out/autoblink-esp32-ota.bin"
# Firmware 1.22.0 and older downloads xdj-clock-esp32-ota.bin.
cp "$out/autoblink-esp32-ota.bin" "$out/xdj-clock-esp32-ota.bin"
echo "$version" > "$out/esp32-version.txt"

size=$(wc -c < "$out/autoblink-esp32-ota.bin")
if [ "$size" -ge $((0x1E0000)) ]; then
  echo "The image ($size bytes) is larger than the app slot. Do not install it." >&2
  exit 1
fi

gh release upload esp32-latest --clobber \
  "$out/esp32-version.txt" "$out/autoblink-esp32-ota.bin" "$out/xdj-clock-esp32-ota.bin"
python3 -I scripts/ws-command.py "$host" '{"type":"take_over"}' '{"type":"ota_install"}' \
  | grep -E '^>|error' || true

echo "Waiting for $version to start on $host"
i=0
while [ $i -lt 80 ]; do
  if curl -s -m 2 "http://$host/log" | grep -q "AUTOBLINK $version starts"; then
    echo "The box runs $version."
    exit 0
  fi
  i=$((i + 1))
  sleep 3
done
echo "The box does not run $version after 4 minutes. Read http://$host/log." >&2
exit 1
