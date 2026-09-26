#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"
swift build
BIN_PATH="$(swift build --show-bin-path)"
if [ "${1:-}" = "--demo" ]; then
    app_name="Chat Stasher Demo"
    bundle_id="dev.chat-stasher.menubar-demo"
else
    app_name="Chat Stasher"
    bundle_id="dev.chat-stasher.menubar"
fi
app_dir="$PWD/.build/$app_name.app"
contents="$app_dir/Contents"
mkdir -p "$contents/MacOS" "$contents/Frameworks" "$contents/Resources"
cp "$BIN_PATH/chat-stasher-menubar" "$contents/MacOS/chat-stasher-menubar"
for framework in "$BIN_PATH"/*.framework; do
    [ -d "$framework" ] || continue
    cp -R "$framework" "$contents/Frameworks/"
done
for bundle in "$BIN_PATH"/*.bundle; do
    [ -d "$bundle" ] || continue
    cp -R "$bundle" "$contents/Resources/"
done
cat > "$contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>chat-stasher-menubar</string>
    <key>CFBundleIdentifier</key><string>$bundle_id</string>
    <key>CFBundleName</key><string>$app_name</string>
    <key>CFBundleDisplayName</key><string>$app_name</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.5.0</string>
    <key>CFBundleVersion</key><string>502</string>
    <key>LSUIElement</key><true/>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>SUPublicEDKey</key><string>F5F6GYCv++DbXSfZHEqH2fjlL3csuKYnPWAPnx8ZvFM=</string>
    <key>SUFeedURL</key><string>https://github.com/dimpurr/chat-stasher/releases/latest/download/appcast.xml</string>
    <key>SUEnableAutomaticChecks</key><true/>
    <key>SUAllowsAutomaticUpdates</key><true/>
</dict>
</plist>
PLIST
if ! /usr/bin/otool -l "$contents/MacOS/chat-stasher-menubar" | /usr/bin/grep -q '@executable_path/../Frameworks'; then
    /usr/bin/install_name_tool -add_rpath @executable_path/../Frameworks "$contents/MacOS/chat-stasher-menubar"
fi
while IFS= read -r -d '' file_path; do
    if /usr/bin/file -b "$file_path" | /usr/bin/grep -q 'Mach-O'; then
        /usr/bin/codesign --force --sign - --timestamp=none "$file_path"
    fi
done < <(/usr/bin/find "$contents/Frameworks" -type f -print0)
while IFS= read -r -d '' bundle_path; do
    /usr/bin/codesign --force --sign - --timestamp=none "$bundle_path"
done < <(/usr/bin/find "$contents/Frameworks" -depth -type d \( -name '*.xpc' -o -name '*.app' -o -name '*.framework' -o -name '*.bundle' \) -print0)
/usr/bin/codesign --force --sign - --timestamp=none "$app_dir"
printf '%s\n' "$app_dir"
