#!/bin/sh
# Removes what scripts/install.sh installed (by its record), but no file
# changed since (unless --force), and never your settings or recent
# repositories.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
. "$root/scripts/lib/common.sh"

prefix= force=0
while [ $# -gt 0 ]; do
    case $1 in
        --prefix) prefix=${2:?--prefix needs a folder}; shift ;;
        --prefix=*) prefix=${1#*=} ;;
        --force) force=1 ;;
        -h | --help) say "Usage: scripts/uninstall.sh [--prefix DIR] [--force]"; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
    shift
done

locate
# (Whether there is an installation is seen holding the lock: one may be
# under way.)
if [ -d "$STATE" ]; then
    lock
fi
if [ ! -f "$MANIFEST" ] && [ ! -f "$PENDING" ]; then
    say "Tine isn't installed in $DATA (there is no record of an installation)."
    exit 0
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/tine-uninstall.XXXXXX")
trap 'rm -rf "$work"' EXIT
trap 'exit 1' INT TERM HUP
: >"$work/kept"
removed= kept=
seen=
for m in "$MANIFEST" "$PENDING"; do
    [ -f "$m" ] || continue
    while IFS="$tab" read -r hash dest; do
        case "$nl$seen$nl" in *"$nl$dest$nl"*) continue ;; esac
        seen="$seen$nl$dest"
        [ -f "$dest" ] || continue
        if [ "$force" = 1 ] || owned "$dest" "$(hash_of "$dest")"; then
            rm -f "$dest"
            removed="$removed$nl  $dest"
        else
            kept="$kept$nl  $dest"
            printf '%s\n' "$dest" >>"$work/kept"
        fi
    done <"$m"
done
# What is kept stays in the record (a later --force takes it).
if [ -n "$kept" ]; then
    for m in "$MANIFEST" "$PENDING"; do
        [ -f "$m" ] || continue
        awk -F "$tab" 'NR == FNR { k[$0] = 1; next } $2 in k' "$work/kept" "$m"
    done | awk '!seen[$0]++' >"$MANIFEST.tmp"
    mv -fT -- "$MANIFEST.tmp" "$MANIFEST"
else
    rm -f "$MANIFEST"
fi
rm -f "$PENDING"
rmdir "$DATA/doc/tine" 2>/dev/null || true
refresh_caches

say "Removed:${removed:- nothing}"
[ -z "$kept" ] || say "Kept (changed since they were installed; --force removes them):$kept"
data=${XDG_DATA_HOME:-$HOME/.local/share}/tine
case $data in /*) ;; *) data=$HOME/.local/share/tine ;; esac
if [ -d "$data" ]; then
    say "Your settings and recent repositories are kept in $data."
fi
