#!/usr/bin/env bash
set -euo pipefail

version=$1
output=$2
root=$(cd "$(dirname "$0")" && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
app="$stage/Synth.app"

mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$(dirname "$output")"
install -m 644 "$root/synth.icns" "$app/Contents/Resources/synth.icns"
lipo -create \
    target/aarch64-apple-darwin/release/synth \
    target/x86_64-apple-darwin/release/synth \
    -output "$app/Contents/MacOS/synth"
lipo "$app/Contents/MacOS/synth" -verify_arch arm64 x86_64

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>synth</string>
    <key>CFBundleIdentifier</key><string>com.sintezator2763.synth</string>
    <key>CFBundleIconFile</key><string>synth.icns</string>
    <key>CFBundleName</key><string>Synth</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

plutil -lint "$app/Contents/Info.plist"
codesign --force --sign - "$app"
codesign --verify --strict "$app"
ln -s /Applications "$stage/Applications"
hdiutil create -volname Synth -srcfolder "$stage" -format UDZO "$output"
hdiutil verify "$output"
