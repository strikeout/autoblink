#!/bin/sh
# Cargo runner for macOS. Cargo gives the binary path and then the arguments.
#
# macOS blocks local network access for a command-line binary that has no
# permission. A TCP connection or a UDP unicast to a player then fails with
# "No route to host". System Settings shows no Local Network switch for a bare
# binary. This script copies xdj-clock into an app bundle, signs the bundle and
# runs the copy. The bundle then gets its own switch.
#
# The script signs with the identity in XDJ_CODESIGN_IDENTITY. If that variable
# is not set, it uses the first "Apple Development" identity. A stable identity
# keeps the permission after a rebuild. An ad-hoc signature does not.
#
# The script runs each other binary, for example a test binary, without a change.
set -eu

bin=$1
shift
[ "$(basename "$bin")" = xdj-clock ] || exec "$bin" "$@"

app="$(dirname "$bin")/xdj-clock.app"
mkdir -p "$app/Contents/MacOS"
cp "$bin" "$app/Contents/MacOS/xdj-clock"
cat > "$app/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>com.strikeout.xdj-clock</string>
	<key>CFBundleName</key>
	<string>xdj-clock</string>
	<key>CFBundleExecutable</key>
	<string>xdj-clock</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>NSLocalNetworkUsageDescription</key>
	<string>xdj-clock reads the beat, the track and the phrase data from the Pro DJ Link players.</string>
</dict>
</plist>
EOF

identity=${XDJ_CODESIGN_IDENTITY:-$(security find-identity -v -p codesigning |
	awk -F'"' '/Apple Development/ { print $2; exit }')}
if [ -z "$identity" ]; then
	echo "macos-run.sh: no signing identity. The Local Network permission is lost after each build." >&2
	identity=-
fi
codesign --force --sign "$identity" "$app" 2>/dev/null ||
	echo "macos-run.sh: codesign failed. The app runs with the old signature." >&2

exec "$app/Contents/MacOS/xdj-clock" "$@"
