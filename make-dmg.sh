#!/bin/zsh
# Build the MacNCL disk-image installer: dist/MacNCL-<version>.dmg
#
# Pipeline: release build -> stage MacNCL.app (binary + resources + bundled
# Lisp) -> ad-hoc codesign -> UDRW image -> mount -> write .DS_Store layout
# (window bounds, icon view, background, icon positions) -> detach ->
# convert to compressed UDZO -> verify.
#
# The Finder-window styling is written straight into the volume's .DS_Store
# with the `ds_store` library, because Finder AppleScript (`view options`
# et al.) is gone from the macOS 26 Finder dictionary. The library is
# installed into a throwaway venv under dist/ on first use; with no network
# (or --no-layout) the image is still produced, just without styling.
set -euo pipefail

cd "$(dirname "$0")"
repo=$(pwd)
version=$(defaults read "$repo/resources/Info.plist" CFBundleShortVersionString)
out="$repo/dist/MacNCL-$version.dmg"
volname="MacNCL $version"
voldir="/Volumes/$volname"

want_layout=1
if [[ "${1:-}" == "--no-layout" ]]; then
  want_layout=0
fi

echo "==> release build (mac-gui)"
cargo build --release -p ncl-driver --features mac-gui

staging="$repo/dist/staging"
rm -rf "$staging"
mkdir -p "$staging/MacNCL.app/Contents/MacOS" "$staging/MacNCL.app/Contents/Resources"

echo "==> staging bundle"
cp target/release/ncl "$staging/MacNCL.app/Contents/MacOS/ncl"
strip "$staging/MacNCL.app/Contents/MacOS/ncl" || true
cp resources/Info.plist "$staging/MacNCL.app/Contents/"
cp resources/AppIcon.icns resources/MacNCL.sdef "$staging/MacNCL.app/Contents/Resources/"
cp -R Lisp "$staging/MacNCL.app/Contents/Resources/Lisp"
ln -s /Applications "$staging/Applications"
mkdir -p "$staging/.background"
cp resources/dmg-background.png "$staging/.background/background.png"

echo "==> ad-hoc codesign"
codesign --force --deep --sign - "$staging/MacNCL.app"

have_layout=0
if [[ $want_layout == 1 ]]; then
  venv="$repo/dist/.venv"
  if [[ ! -x "$venv/bin/python" ]]; then
    echo "==> preparing ds_store venv (one-time, needs network)"
    rm -rf "$venv"
    python3 -m venv "$venv" && "$venv/bin/pip" -q install ds_store
  fi
  [[ -x "$venv/bin/python" ]] && have_layout=1
fi
if [[ $want_layout == 1 && $have_layout == 0 ]]; then
  echo "    !! ds_store unavailable -- building an unstyled image"
fi

if [[ $have_layout == 0 ]]; then
  rm -f "$out"
  hdiutil create -ov -format UDZO -volname "$volname" -srcfolder "$staging" \
    "$out" >/dev/null
  rm -rf "$staging"
  echo "==> $out (unstyled)"
  du -h "$out" | cut -f1 | xargs echo "    size:"
  exit 0
fi

rw="$repo/dist/MacNCL-rw-$version.dmg"
rm -f "$rw" "$out"
hdiutil create -ov -format UDRW -volname "$volname" -srcfolder "$staging" \
  "$rw" >/dev/null

dev=$(hdiutil attach -readwrite -noverify -noautoopen -owners off "$rw" |
  sed -n 's|^\(/dev/disk[0-9]*\).*'"$volname"'.*|\1|p' | tail -1)
cleanup() { hdiutil detach "$dev" >/dev/null 2>&1 || hdiutil detach -force "$dev" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "==> writing .DS_Store layout"
"$venv/bin/python" - "$voldir" <<'PY'
import sys
from ds_store import DSStore
from mac_alias import Alias

voldir = sys.argv[1]
alias = Alias.for_file(voldir + "/.background/background.png")

bwsp = {
    "ShowStatusBar": False,
    "WindowBounds": "{{100, 100}, {760, 500}}",
    "ContainerShowSidebar": False,
    "PreviewPaneVisibility": False,
    "SidebarWidth": 0,
    "ShowTabView": False,
    "ShowToolbar": False,
    "ShowPathbar": False,
    "ShowSidebar": False,
}
icvp = {
    "viewOptionsVersion": 1,
    "backgroundType": 2,
    "backgroundImageAlias": alias.to_bytes(),
    "backgroundColorRed": 1.0,
    "backgroundColorGreen": 1.0,
    "backgroundColorBlue": 1.0,
    "gridOffsetX": 0.0,
    "gridOffsetY": 0.0,
    "gridSpacing": 100.0,
    "arrangeBy": "none",
    "showIconPreview": True,
    "showItemInfo": False,
    "labelOnBottom": True,
    "textSize": 12.0,
    "iconSize": 128.0,
    "scrollPositionX": 0.0,
    "scrollPositionY": 0.0,
}

# Icon positions (points, y from top) land on the background's drop zones.
with DSStore.open(voldir + "/.DS_Store", "w+") as d:
    d["."]["vSrn"] = ("long", 1)
    d["."]["bwsp"] = bwsp
    d["."]["icvp"] = icvp
    d["MacNCL.app"]["Iloc"] = (170, 220)
    d["Applications"]["Iloc"] = (490, 220)
PY

# The RW mount leaves filesystem-event noise behind; keep the image clean.
rm -rf "$voldir/.fseventsd" "$voldir/.Trashes" 2>/dev/null || true

hdiutil detach "$dev" >/dev/null
trap - EXIT

echo "==> compressing"
hdiutil convert -format UDZO -o "$out" "$rw" >/dev/null
rm -f "$rw"
rm -rf "$staging"

echo "==> verifying"
hdiutil verify "$out" >/dev/null && echo "    image verifies"

echo "==> $out"
du -h "$out" | cut -f1 | xargs echo "    size:"
