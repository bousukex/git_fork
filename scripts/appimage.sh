#!/bin/sh
# Makes target/appimage/pengitto-<version>-x86_64.AppImage from a built pengitto.
# No libraries are bundled: the host's glibc (2.39 or newer), display and
# GPU libraries are used, and nothing like LD_LIBRARY_PATH reaches the
# programs pengitto starts. The AppImage tools are pinned (packaging/appimage/
# tools.lock): APPIMAGETOOL and APPIMAGE_RUNTIME name them.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
. "$root/scripts/lib/common.sh"

binary=$root/target/release/pengitto
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
[ -f "$binary" ] || die "no executable at $binary (build it first: cargo build --release -p pengitto)"
version=$("$binary" --version 2>/dev/null) || die "$binary doesn't start"
version=${version#pengitto }

work=$(mktemp -d "${TMPDIR:-/tmp}/pengitto-appimage.XXXXXX")
trap 'rm -rf "$work"' EXIT
app=$work/pengitto.AppDir
mkdir -p "$app/usr/bin" "$app/usr/share/applications" "$app/usr/share/doc/pengitto"
install -m 755 "$binary" "$app/usr/bin/pengitto"
sed 's|^Exec=@EXEC@ -- %F$|Exec=pengitto -- %F|' "$root/packaging/pengitto.desktop.in" >"$app/usr/share/applications/pengitto.desktop"
grep -q '^Exec=pengitto -- %F$' "$app/usr/share/applications/pengitto.desktop" || die "the menu entry's template has changed"
if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$app/usr/share/applications/pengitto.desktop"
fi
for n in 16 24 32 48 64 128 256 512; do
    mkdir -p "$app/usr/share/icons/hicolor/${n}x$n/apps"
    install -m 644 "$root/packaging/icons/hicolor/${n}x$n/apps/pengitto.png" "$app/usr/share/icons/hicolor/${n}x$n/apps/pengitto.png"
done
install -m 644 "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$root/packaging/THIRD_PARTY_LICENSES" "$root/packaging/APPIMAGE_NOTICES" "$app/usr/share/doc/pengitto/"
cp "$app/usr/share/applications/pengitto.desktop" "$app/pengitto.desktop"
cp "$root/packaging/icons/hicolor/256x256/apps/pengitto.png" "$app/pengitto.png"
ln -s pengitto.png "$app/.DirIcon"
install -m 755 "$root/packaging/appimage/AppRun" "$app/AppRun"
# What --install and --uninstall run (laid out as in the repository; the
# icons and licenses are the image's own).
setup=$app/usr/share/pengitto/setup
mkdir -p "$setup/scripts/lib" "$setup/packaging"
install -m 644 "$root/scripts/install.sh" "$root/scripts/uninstall.sh" "$setup/scripts/"
install -m 644 "$root/scripts/lib/common.sh" "$setup/scripts/lib/"
install -m 644 "$root/packaging/pengitto.desktop.in" "$setup/packaging/"
ln -s ../../../icons "$setup/packaging/icons"
ln -s ../../../doc/pengitto/THIRD_PARTY_LICENSES "$setup/packaging/THIRD_PARTY_LICENSES"
ln -s ../../doc/pengitto/LICENSE-MIT "$setup/LICENSE-MIT"
ln -s ../../doc/pengitto/LICENSE-APACHE "$setup/LICENSE-APACHE"
mkdir -p "$out"
if [ "$appdir_only" = 1 ]; then
    rm -rf "$out/pengitto.AppDir"
    cp -a "$app" "$out/pengitto.AppDir"
    say "Made $out/pengitto.AppDir"
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
image=$out/pengitto-$version-x86_64.AppImage
rm -f "$image"
# appimagetool is an AppImage itself: run it without needing FUSE (what it
# extracts is left behind, in TMPDIR: the work folder, removed after).
TMPDIR=$work APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 "$tool" --no-appstream --runtime-file "$runtime" "$app" "$image" >"$work/log" 2>&1 ||
    { cat "$work/log" >&2; die "appimagetool failed"; }
say "Made $image"
