#!/bin/sh
# Makes target/appimage/Tine-<version>-x86_64.AppImage from a built tine.
# No libraries are bundled: the host's glibc (2.39 or newer), display and
# GPU libraries are used, and nothing like LD_LIBRARY_PATH reaches the
# programs Tine starts. The AppImage tools are pinned (packaging/appimage/
# tools.lock): APPIMAGETOOL and APPIMAGE_RUNTIME name them.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
. "$root/scripts/lib/common.sh"

binary=$root/target/release/tine
out=$root/target/appimage
appdir_only=0
while [ $# -gt 0 ]; do
    case $1 in
        --binary) binary=${2:?--binary needs a file}; shift ;;
        --out) out=${2:?--out needs a folder}; shift ;;
        --appdir-only) appdir_only=1 ;;
        -h | --help) say "Usage: scripts/appimage.sh [--binary FILE] [--out DIR] [--appdir-only]"; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
    shift
done
[ -f "$binary" ] || die "no executable at $binary (build it first: cargo build --release -p tine)"
version=$("$binary" --version 2>/dev/null) || die "$binary doesn't start"
version=${version#tine }

work=$(mktemp -d "${TMPDIR:-/tmp}/tine-appimage.XXXXXX")
trap 'rm -rf "$work"' EXIT
app=$work/Tine.AppDir
mkdir -p "$app/usr/bin" "$app/usr/share/applications" "$app/usr/share/doc/tine"
install -m 755 "$binary" "$app/usr/bin/tine"
sed 's|^Exec=@EXEC@ -- %F$|Exec=tine -- %F|' "$root/packaging/tine.desktop.in" >"$app/usr/share/applications/tine.desktop"
grep -q '^Exec=tine -- %F$' "$app/usr/share/applications/tine.desktop" || die "the menu entry's template has changed"
if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$app/usr/share/applications/tine.desktop"
fi
for n in 16 24 32 48 64 128 256 512; do
    mkdir -p "$app/usr/share/icons/hicolor/${n}x$n/apps"
    install -m 644 "$root/packaging/icons/hicolor/${n}x$n/apps/tine.png" "$app/usr/share/icons/hicolor/${n}x$n/apps/tine.png"
done
install -m 644 "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$root/packaging/THIRD_PARTY_LICENSES" "$root/packaging/APPIMAGE_NOTICES" "$app/usr/share/doc/tine/"
cp "$app/usr/share/applications/tine.desktop" "$app/tine.desktop"
cp "$root/packaging/icons/hicolor/256x256/apps/tine.png" "$app/tine.png"
ln -s tine.png "$app/.DirIcon"
cat >"$app/AppRun" <<'APPRUN'
#!/bin/sh
# Runs Tine from the mounted image (it is its own helper for git: it finds
# itself as /proc/<pid>/exe).
here=$(dirname "$(readlink -f "$0")")
exec "$here/usr/bin/tine" "$@"
APPRUN
chmod 755 "$app/AppRun"
mkdir -p "$out"
if [ "$appdir_only" = 1 ]; then
    rm -rf "$out/Tine.AppDir"
    cp -a "$app" "$out/Tine.AppDir"
    say "Made $out/Tine.AppDir"
    exit 0
fi

# The pinned tools.
pinned() {
    awk -F '\t' -v n="$1" '$1 == n { print $2 }' "$root/packaging/appimage/tools.lock"
}
where() {
    awk -F '\t' -v n="$1" '$1 == n { print $3 }' "$root/packaging/appimage/tools.lock"
}
tool=${APPIMAGETOOL:-}
runtime=${APPIMAGE_RUNTIME:-}
if [ -z "$tool" ] || [ -z "$runtime" ]; then
    die "set APPIMAGETOOL and APPIMAGE_RUNTIME to the pinned tools:
  appimagetool: $(where appimagetool) (sha256 $(pinned appimagetool))
  runtime:      $(where runtime) (sha256 $(pinned runtime))"
fi
[ "$(hash_of "$tool")" = "$(pinned appimagetool)" ] || die "$tool isn't the pinned appimagetool (see packaging/appimage/tools.lock)"
[ "$(hash_of "$runtime")" = "$(pinned runtime)" ] || die "$runtime isn't the pinned runtime (see packaging/appimage/tools.lock)"
command -v file >/dev/null 2>&1 || die "appimagetool needs the file command (apt/dnf: file)"
image=$out/Tine-$version-x86_64.AppImage
rm -f "$image"
# appimagetool is an AppImage itself: run it without needing FUSE (what it
# extracts is left behind, in TMPDIR: the work folder, removed after).
TMPDIR=$work APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 "$tool" --no-appstream --runtime-file "$runtime" "$app" "$image" >"$work/log" 2>&1 ||
    { cat "$work/log" >&2; die "appimagetool failed"; }
say "Made $image"
