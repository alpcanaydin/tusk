#!/bin/bash
# Compile assets/icon/Tusk.icon (Icon Composer) into assets/icon/compiled/
# (Assets.car + Tusk.icns), which bundle.sh copies into Tusk.app. Run it after
# changing the icon, on a Mac with Xcode 26 or later, and commit the result:
# CI runners' actool can't compile this format (Xcode 26 crashes on macOS 15).
set -euo pipefail
cd "$(dirname "$0")/.."
out=assets/icon/compiled
tmp=$(mktemp -d)
xcrun actool --compile "$tmp" --platform macosx \
  --minimum-deployment-target 14.0 --app-icon Tusk \
  --output-partial-info-plist "$tmp/icon.plist" \
  assets/icon/Tusk.icon >/dev/null
[ -f "$tmp/Tusk.icns" ] && [ -f "$tmp/Assets.car" ] || {
  echo "compile-icon: actool produced no icon (need Xcode 26+; have: $(xcodebuild -version | head -1))" >&2
  exit 1
}
mkdir -p "$out"
cp "$tmp/Tusk.icns" "$tmp/Assets.car" "$out/"
rm -rf "$tmp"
echo "compile-icon: wrote $out/Tusk.icns and $out/Assets.car"
