#!/bin/sh
# Builds Spatium.app next to this script (release build, ad-hoc signed).
set -e
cd "$(dirname "$0")"
cargo build --release --offline
APP=Spatium.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/spatium "$APP/Contents/MacOS/spatium"
cat > "$APP/Contents/Info.plist" <<PL
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>Spatium</string>
<key>CFBundleIdentifier</key><string>app.spatium.disk</string>
<key>CFBundleExecutable</key><string>spatium</string>
<key>CFBundleVersion</key><string>0.1.0</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>NSHighResolutionCapable</key><true/>
<key>LSMinimumSystemVersion</key><string>11.0</string>
</dict></plist>
PL
codesign --force --sign - "$APP" >/dev/null 2>&1 || true
echo "built $APP"
