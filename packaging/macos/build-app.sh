#!/usr/bin/env bash
# Builds Bunsin.app from bunsin-app. The bundle is not signed; README.md in this folder
# says how to sign and notarize it by hand.
#
# Usage: packaging/macos/build-app.sh [output directory]
# The default output directory is target/package. The script prints the path of the bundle.
# BUNSIN_BUNDLE_ID sets the bundle identifier; the default is only for local builds.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
out=${1:-"$repo/target/package"}
target_dir=${CARGO_TARGET_DIR:-"$repo/target"}
bundle_id=${BUNSIN_BUNDLE_ID:-local.bunsin.app}

cd "$repo"
# `cargo pkgid` prints for example path+file:///src/bunsin/crates/app#bunsin-app@0.1.0.
version=$(cargo pkgid -p bunsin-app --locked)
version=${version##*[@#]}

cargo build --release -p bunsin-app --locked

app="$out/Bunsin.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 0755 "$target_dir/release/bunsin-app" "$app/Contents/MacOS/bunsin-app"
# The repository has no icon yet, so the bundle names none and macOS shows its default.
cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>Bunsin</string>
    <key>CFBundleExecutable</key>
    <string>bunsin-app</string>
    <key>CFBundleIdentifier</key>
    <string>$bundle_id</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Bunsin</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.productivity</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST
plutil -lint "$app/Contents/Info.plist" >/dev/null
printf 'APPL????' >"$app/Contents/PkgInfo"
echo "$app"
