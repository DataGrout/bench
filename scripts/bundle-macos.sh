#!/usr/bin/env bash
# Wrap the release binary in a macOS application bundle.
#
# Two things need the bundle: the menu bar shows the app's name only when an
# Info.plist supplies it (an unbundled binary shows winit's placeholder), and
# audio input can only prompt for microphone permission from a bundle that
# declares NSMicrophoneUsageDescription.
#
# Usage: scripts/bundle-macos.sh [--no-audio]
# Output: target/release/bundle/Bench.app
set -euo pipefail

cd "$(dirname "$0")/.."

features="--features audio"
if [[ "${1:-}" == "--no-audio" ]]; then
  features=""
fi

version=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')

cargo build --release -p bench-gui $features

app="target/release/bundle/Bench.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/dgbench "$app/Contents/MacOS/dgbench"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>            <string>Bench</string>
  <key>CFBundleDisplayName</key>     <string>Bench</string>
  <key>CFBundleIdentifier</key>      <string>ai.datagrout.bench</string>
  <key>CFBundleExecutable</key>      <string>dgbench</string>
  <key>CFBundlePackageType</key>     <string>APPL</string>
  <key>CFBundleShortVersionString</key> <string>${version}</string>
  <key>CFBundleVersion</key>         <string>${version}</string>
  <key>LSMinimumSystemVersion</key>  <string>11.0</string>
  <key>NSHighResolutionCapable</key> <true/>
  <key>NSMicrophoneUsageDescription</key>
  <string>Bench shows audio input on the scope and analyses it.</string>
  <!-- .bench profiles: Finder shows them as Bench documents and offers
       "Open With Bench". Delivery of a double-clicked file into a running
       eframe window is not wired (the toolkit does not surface the open-file
       event on macOS), so the ways in are the Profiles menu, dragging the
       file onto the window, or launching with the path on the command line:
       open -a Bench --args --profile path.bench -->
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key>      <string>Bench profile</string>
      <key>CFBundleTypeRole</key>      <string>Viewer</string>
      <key>LSHandlerRank</key>         <string>Owner</string>
      <key>LSItemContentTypes</key>
      <array><string>ai.datagrout.bench.profile</string></array>
    </dict>
  </array>
  <key>UTExportedTypeDeclarations</key>
  <array>
    <dict>
      <key>UTTypeIdentifier</key>      <string>ai.datagrout.bench.profile</string>
      <key>UTTypeDescription</key>     <string>Bench profile</string>
      <key>UTTypeConformsTo</key>      <array><string>public.json</string></array>
      <key>UTTypeTagSpecification</key>
      <dict>
        <key>public.filename-extension</key>
        <array><string>bench</string></array>
      </dict>
    </dict>
  </array>
</dict>
</plist>
PLIST

echo "built $app (version $version${features:+, with audio})"
echo "run:  open $app"
