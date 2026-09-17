#!/usr/bin/env bash
#
# run-gui.sh — build and launch the MacNCL IDE (editor + REPL), or run a
# standalone Lisp GUI app. macOS only. Replaces the old Windows run-gui.bat.
#
# Usage:
#   ./run-gui.sh                    build (debug) and open the IDE
#   ./run-gui.sh --release          build optimised and open the IDE (faster startup)
#   ./run-gui.sh --lean             start with core only (no Library)
#   ./run-gui.sh --eval '(form)'    evaluate a form at startup (repeatable)
#   ./run-gui.sh --demo othello-gui open the IDE and run Lisp/demos/othello-gui.lisp
#   ./run-gui.sh --app  othello-gui run that demo standalone — no IDE chrome,
#                                   quits when its window closes (--run-window)
#   ./run-gui.sh --no-build         skip the build, run the existing binary
#   ./run-gui.sh --no-bundle        run the bare binary instead of MacNCL.app
#                                   (no Dock icon / About panel / file types)
#   ./run-gui.sh -- <extra args>    pass everything after -- straight to ncl
#
# --demo/--app NAME loads Lisp/demos/NAME.lisp and calls (run-NAME).

# `set -u` is intentionally omitted: macOS ships bash 3.2, where expanding an
# empty array under `set -u` aborts. `-e -o pipefail` give us the safety that
# matters here.
set -eo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$repo"

# cargo isn't always on PATH (rustup installs to ~/.cargo/bin).
if ! command -v cargo >/dev/null 2>&1; then
  if [ -x "$HOME/.cargo/bin/cargo" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
  else
    echo "run-gui: cargo not found — install Rust via rustup (https://rustup.rs)." >&2
    exit 1
  fi
fi

usage() { sed -n '3,18p' "$0" | sed 's/^# \{0,1\}//'; }

profile="debug"
profile_flag=()
do_build=1
do_bundle=1
lean=()
evals=()
demo=""
standalone=0
passthru=()

while [ $# -gt 0 ]; do
  case "$1" in
    --release)   profile="release"; profile_flag=(--release); shift ;;
    --no-build)  do_build=0; shift ;;
    --no-bundle) do_bundle=0; shift ;;
    --lean)      lean=(--lean); shift ;;
    --eval)      [ $# -ge 2 ] || { echo "run-gui: --eval needs a form" >&2; exit 2; }
                 evals+=(--eval "$2"); shift 2 ;;
    --demo)      [ $# -ge 2 ] || { echo "run-gui: --demo needs a name" >&2; exit 2; }
                 demo="$2"; standalone=0; shift 2 ;;
    --app)       [ $# -ge 2 ] || { echo "run-gui: --app needs a name" >&2; exit 2; }
                 demo="$2"; standalone=1; shift 2 ;;
    -h|--help)   usage; exit 0 ;;
    --)          shift; passthru=("$@"); break ;;
    *)           echo "run-gui: unknown argument '$1' (use -- to pass args straight to ncl)" >&2
                 exit 2 ;;
  esac
done

bin="$repo/target/$profile/ncl"

if [ "$do_build" -eq 1 ]; then
  echo "[run-gui] building ncl ($profile, mac-gui)…"
  cargo build "${profile_flag[@]}" -p ncl-driver --features mac-gui
fi
if [ ! -x "$bin" ]; then
  echo "run-gui: binary not found at $bin (drop --no-build to build it)." >&2
  exit 1
fi

args=("${lean[@]}" "${evals[@]}")

if [ -n "$demo" ]; then
  file="$repo/Lisp/demos/${demo}.lisp"
  if [ ! -f "$file" ]; then
    echo "run-gui: demo '$demo' not found at $file. Available:" >&2
    ls "$repo/Lisp/demos"/*.lisp 2>/dev/null \
      | xargs -n1 basename | sed 's/\.lisp$//; s/^/  /' >&2
    exit 1
  fi
  if [ "$standalone" -eq 1 ]; then
    args+=(--run-window)        # just the app window; quits on close
  else
    args+=(--windows)           # the IDE, with the demo running alongside
  fi
  args+=(--load "$file" --eval "(run-${demo})")
else
  args+=(--windows)             # the IDE: editor + REPL in one window
fi

args+=("${passthru[@]}")

# Assemble a proper .app bundle so the Dock shows the λ icon, the app menu
# says "About MacNCL", and .lisp files can be associated with us.
app="$repo/build/MacNCL.app"
if [ "$do_bundle" -eq 1 ] && [ -f "$repo/resources/Info.plist" ] \
   && [ -f "$repo/resources/AppIcon.icns" ]; then
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  cp "$bin" "$app/Contents/MacOS/ncl"
  cp "$repo/resources/Info.plist" "$app/Contents/Info.plist"
  cp "$repo/resources/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
  [ -f "$repo/resources/MacNCL.sdef" ] \
    && cp "$repo/resources/MacNCL.sdef" "$app/Contents/Resources/MacNCL.sdef"
  # Refresh the registration so Finder/LaunchServices see the current icon.
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
    -f "$app" >/dev/null 2>&1 || true
  echo "[run-gui] open $app ${args[*]}"
  exec open "$app" --args "${args[@]}"
fi

echo "[run-gui] $bin ${args[*]}"
exec "$bin" "${args[@]}"
