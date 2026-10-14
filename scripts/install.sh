#!/bin/sh
# Installs Tine: the executable in ~/.local/bin, its menu entry, icons and
# licenses in your data folder (or all under --prefix). Everything is made
# ready and checked first, then put in place; files Tine didn't put there
# (or that were changed since) aren't overwritten without --force.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
. "$root/scripts/lib/common.sh"

usage() {
    cat <<'USAGE'
Usage: scripts/install.sh [OPTION]...
  --prefix DIR       install into DIR/bin and DIR/share (default: ~/.local/bin
                     and your data folder, ~/.local/share)
  --binary FILE      install FILE (a tine executable, or a Tine AppImage)
  --no-build         don't build: install target/release/tine as it is
  --exec PATH        the executable path the menu entry runs (default: the
                     installed one)
  --no-integration   don't refresh desktop caches
  --no-manifest      keep no record of what was installed (packages)
  --force            overwrite files Tine didn't put there
USAGE
}

prefix= binary= build=1 exec_path= integrate=1 manifest=1 force=0
while [ $# -gt 0 ]; do
    case $1 in
        --prefix) prefix=${2:?--prefix needs a folder}; shift ;;
        --prefix=*) prefix=${1#*=} ;;
        --binary) binary=${2:?--binary needs a file}; shift ;;
        --binary=*) binary=${1#*=} ;;
        --exec) exec_path=${2:?--exec needs a path}; shift ;;
        --exec=*) exec_path=${1#*=} ;;
        --no-build) build=0 ;;
        --no-integration) integrate=0 ;;
        --no-manifest) manifest=0 ;;
        --force) force=1 ;;
        -h | --help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
    shift
done

locate
exec_path=${exec_path:-$BIN/tine}
for p in "$BIN" "$DATA" "$exec_path"; do
    check_path "$p"
done
case $exec_path in
    /*) ;;
    *) die "the menu entry needs an absolute path to run: $exec_path" ;;
esac
# The spec allows no '=' in the program's path; with a '%' (written %%),
# GLib's launchers can't find the program.
case $exec_path in
    *=* | *%*) die "a menu entry can't run a program whose path has '=' or '%' in it: $exec_path" ;;
esac

# The executable.
if [ -z "$binary" ]; then
    binary=$root/target/release/tine
    if [ "$build" = 1 ]; then
        command -v cargo >/dev/null 2>&1 || die "cargo isn't there to build Tine (with a built one: --no-build)"
        (cd "$root" && cargo build --release -p tine)
    fi
fi
[ -f "$binary" ] || die "no executable at $binary (build it first: cargo build --release -p tine)"
version=$("$binary" --version 2>/dev/null) || version=
case $version in
    "tine "*) ;;
    *) die "$binary doesn't start here (needs glibc 2.39 or newer)" ;;
esac

# (A package's staging area has no other installation to wait for.)
if [ "$manifest" = 1 ]; then
    lock
fi

# Everything goes to a temporary file next to its place first (those left
# over are removed whatever happens).
work=$(mktemp -d "${TMPDIR:-/tmp}/tine-install.XXXXXX")
: >"$work/staged"
cleanup() {
    while IFS= read -r t; do
        rm -f "$t"
    done <"$work/staged"
    rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 1' INT TERM HUP
list=$work/list
: >"$list"

# Stages $1 (a file) for $2 with mode $3.
stage() {
    mkdir -p "$(dirname "$2")"
    tmp=$(mktemp "$(dirname "$2")/.tine-install.XXXXXX")
    printf '%s\n' "$tmp" >>"$work/staged"
    cat "$1" >"$tmp"
    chmod "$3" "$tmp"
    printf '%s\t%s\t%s\n' "$tmp" "$2" "$(hash_of "$tmp")" >>"$list"
}

stage "$binary" "$BIN/tine" 755
# The copy to be installed checks itself (git, its helpers) as it will run.
# (Not in a package's build, which needn't have git.)
if [ "$manifest" = 1 ] && ! "$tmp" --self-check >"$work/self-check" 2>&1; then
    die "$binary doesn't work here (its self-check failed); nothing was installed:$nl$(cat "$work/self-check")"
fi
desktop=$work/tine.desktop
# The path as the Exec key wants it: quoted (\, ", `, $ escaped; % doubled),
# then as a string value (\ doubled again).
quoted=$(printf '%s' "$exec_path" | sed -e 's/[\\"`$]/\\&/g' -e 's/%/%%/g')
TINE_EXEC=$(printf '"%s"' "$quoted" | sed -e 's/\\/\\\\/g') \
    awk '{ if ($0 == "Exec=@EXEC@ -- %F") print "Exec=" ENVIRON["TINE_EXEC"] " -- %F"; else print }' \
    "$root/packaging/tine.desktop.in" >"$desktop"
if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$desktop" || die "the menu entry made for $exec_path isn't valid"
fi
stage "$desktop" "$DATA/applications/tine.desktop" 644
for n in 16 24 32 48 64 128 256 512; do
    stage "$root/packaging/icons/hicolor/${n}x$n/apps/tine.png" "$DATA/icons/hicolor/${n}x$n/apps/tine.png" 644
done
for f in LICENSE-MIT LICENSE-APACHE; do
    stage "$root/$f" "$DATA/doc/tine/$f" 644
done
stage "$root/packaging/THIRD_PARTY_LICENSES" "$DATA/doc/tine/THIRD_PARTY_LICENSES" 644

# Nothing is overwritten that Tine didn't put there as it is; and never a
# folder (a file moved onto one would go into it).
conflicts= folders=
while IFS="$tab" read -r tmp dest hash; do
    [ -e "$dest" ] || [ -L "$dest" ] || continue
    if [ -d "$dest" ] && [ ! -L "$dest" ]; then
        folders="$folders$nl  $dest"
    elif [ -L "$dest" ] || [ ! -f "$dest" ]; then
        conflicts="$conflicts$nl  $dest (not a plain file)"
    elif ! owned "$dest" "$(hash_of "$dest")"; then
        conflicts="$conflicts$nl  $dest"
    fi
done <"$list"
if [ -n "$folders" ]; then
    die "these are folders where files go; nothing was installed (move them away first):$folders"
fi
if [ -n "$conflicts" ] && [ "$force" = 0 ]; then
    die "these are there already and weren't put there by Tine's installation (or were changed since); nothing was installed (--force overwrites them):$conflicts"
fi

# The record first (a run cut short is taken up by the next), then the
# files. What runs cut short put there stays in the record until one
# finishes: any of them may have put some of its files in place.
record=$work/record
cut -f2,3 "$list" | awk -F "$tab" -v OFS="$tab" '{ print $2, $1 }' >"$record"
if [ "$manifest" = 1 ]; then
    { if [ -f "$PENDING" ]; then cat "$PENDING"; fi; cat "$record"; } | awk '!seen[$0]++' >"$PENDING.tmp"
    mv -fT -- "$PENDING.tmp" "$PENDING"
fi
placed= n=0
while IFS="$tab" read -r tmp dest hash; do
    # (For tests: as if stopped after this many.)
    if [ -n "${TINE_INSTALL_STOP_AFTER:-}" ] && [ "$n" -ge "$TINE_INSTALL_STOP_AFTER" ]; then
        die "stopped (TINE_INSTALL_STOP_AFTER); in place: ${placed:-nothing}"
    fi
    if ! mv -fT -- "$tmp" "$dest"; then
        die "putting $dest in place failed; in place: ${placed:-nothing} (running the installation again finishes it)"
    fi
    placed="$placed$nl  $dest"
    n=$((n + 1))
done <"$list"
if [ "$manifest" = 1 ]; then
    # Files of an earlier installation (or of runs cut short) this one
    # doesn't have go too, if they are still as put there.
    for m in "$MANIFEST" "$PENDING"; do
        [ -f "$m" ] || continue
        while IFS="$tab" read -r hash dest; do
            if D=$dest awk -F "$tab" '$2 == ENVIRON["D"] { f = 1 } END { exit f }' "$record" &&
                [ -f "$dest" ] && [ ! -L "$dest" ] && [ "$(hash_of "$dest")" = "$hash" ]; then
                rm -f "$dest"
            fi
        done <"$m"
    done
    cp "$record" "$MANIFEST.tmp"
    mv -fT -- "$MANIFEST.tmp" "$MANIFEST"
    rm -f "$PENDING"
fi

if [ "$integrate" = 1 ]; then
    refresh_caches
fi

say "Installed $version:$placed"
case ":$PATH:" in
    *":$BIN:"*) ;;
    *) say "Note: $BIN isn't on your PATH (the menu entry works anyway)." ;;
esac
if [ -n "$prefix" ]; then
    here=$(cd "$DATA" && pwd -P)
    same() {
        case $1 in /*) ;; *) return 1 ;; esac
        [ "$(cd "$1" 2>/dev/null && pwd -P)" = "$here" ]
    }
    found=
    home_data=${XDG_DATA_HOME:-}
    case $home_data in /*) ;; *) home_data=$HOME/.local/share ;; esac
    if same "$home_data"; then
        found=1
    fi
    # XDG_DATA_DIRS: split at ':' only (folders may have spaces, or '*').
    saved_ifs=$IFS
    IFS=:
    set -f
    for d in ${XDG_DATA_DIRS:-/usr/local/share:/usr/share}; do
        if same "$d"; then
            found=1
        fi
    done
    set +f
    IFS=$saved_ifs
    [ -n "$found" ] || say "Note: menus don't look in $DATA; Tine's entry won't show there."
elif [ -f /usr/share/applications/tine.desktop ]; then
    say "Note: Tine is also installed system-wide (a package); this installation's menu entry takes its place for you."
fi
