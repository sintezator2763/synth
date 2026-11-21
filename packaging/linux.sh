#!/usr/bin/env bash
set -euo pipefail

binary=$(realpath "$1")
output=$(realpath -m "$2")
root=$(cd "$(dirname "$0")" && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
appdir="$stage/Synth.AppDir"

mkdir -p "$appdir/usr/bin" "$(dirname "$output")"
install -m 755 "$binary" "$appdir/usr/bin/synth"
install -m 644 "$root/synth.png" "$appdir/synth.png"
ln -s synth.png "$appdir/.DirIcon"
ln -s usr/bin/synth "$appdir/AppRun"

cat > "$appdir/synth.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Synth
Exec=synth
Icon=synth
Categories=Utility;TextEditor;
Terminal=false
DESKTOP

cd "$stage"
curl -fsSL --retry 3 https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage -o appimagetool
curl -fsSL --retry 3 https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64 -o runtime-x86_64
sha256sum -c <<'SHA256'
ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0  appimagetool
2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d  runtime-x86_64
SHA256
chmod +x appimagetool
ARCH=x86_64 ./appimagetool --appimage-extract-and-run --runtime-file runtime-x86_64 "$appdir" "$output"
