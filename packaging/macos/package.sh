#!/usr/bin/env bash
set -euo pipefail

version="${1:?usage: package.sh <version> [<binary>] [<out-dir>]}"
binary="${2:-target/release/clean}"
out_dir="${3:-dist}"

if [[ ! -f "$binary" ]]; then
  echo "release binary not found: $binary" >&2
  exit 1
fi

numeric_version=$(printf '%s' "$version" | grep -oE '^[0-9]+(\.[0-9]+)*' || true)
[[ -n "$numeric_version" ]] || numeric_version="0.0.0"

app="$out_dir/Clean.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 755 "$binary" "$app/Contents/MacOS/clean"
install -m 644 packaging/icons/clean.icns "$app/Contents/Resources/clean.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleExecutable</key>
  <string>clean</string>
  <key>CFBundleIdentifier</key>
  <string>com.clean.app</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>Clean</string>
  <key>CFBundleDisplayName</key>
  <string>Clean</string>
  <key>CFBundleIconFile</key>
  <string>clean.icns</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>${version}</string>
  <key>CFBundleVersion</key>
  <string>${numeric_version}</string>
  <key>LSMinimumSystemVersion</key>
  <string>15.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSPrincipalClass</key>
  <string>NSApplication</string>
</dict>
</plist>
PLIST

codesign --force --sign - "$app"

dmg_root="$out_dir/dmgroot"
rm -rf "$dmg_root"
mkdir -p "$dmg_root"
cp -R "$app" "$dmg_root/"
ln -s /Applications "$dmg_root/Applications"

dmg="$out_dir/Clean-${version}-macos-$(uname -m).dmg"
rm -f "$dmg"
hdiutil create -volname "Clean ${version}" -srcfolder "$dmg_root" -ov -format UDZO "$dmg" >/dev/null
rm -rf "$dmg_root"

echo "$dmg"
