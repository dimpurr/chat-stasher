#!/bin/sh
set -eu

cd "$(dirname "$0")"
swift build
if [ "${1:-}" = "--demo" ]; then
    app_name="Chat Stasher Demo"
    bundle_id="dev.chat-stasher.menubar-demo"
else
    app_name="Chat Stasher"
    bundle_id="dev.chat-stasher.menubar-prototype"
fi
app_dir="$PWD/.build/$app_name.app"
mkdir -p "$app_dir/Contents/MacOS"
cp .build/debug/chat-stasher-menubar "$app_dir/Contents/MacOS/chat-stasher-menubar"
cat > "$app_dir/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>chat-stasher-menubar</string>
    <key>CFBundleIdentifier</key>
    <string>$bundle_id</string>
    <key>CFBundleName</key>
    <string>$app_name</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSUIElement</key>
    <true/>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST
printf '%s\n' "$app_dir"
