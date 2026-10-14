# Shared by install.sh and uninstall.sh (sourced; POSIX sh).

die() {
    printf 'tine: %s\n' "$*" >&2
    exit 1
}

say() {
    printf '%s\n' "$*"
}

nl=$(printf '\nx')
nl=${nl%x}
tab=$(printf '\t')

# Where things go: BIN (the executable) and DATA (menus, icons, docs).
# With a prefix, PREFIX/bin and PREFIX/share; else ~/.local/bin and the
# user's XDG data folder.
locate() {
    if [ -n "${prefix:-}" ]; then
        case $prefix in /*) ;; *) prefix=$(pwd)/$prefix ;; esac
        BIN=$prefix/bin
        DATA=$prefix/share
    else
        BIN=$HOME/.local/bin
        case ${XDG_DATA_HOME:-} in
            /*) DATA=$XDG_DATA_HOME ;;
            *) DATA=$HOME/.local/share ;;
        esac
    fi
    STATE=$DATA/tine
    MANIFEST=$STATE/install-manifest
    PENDING=$STATE/install-manifest.pending
}

# A path a desktop file can hold: no control characters (tabs, line
# breaks…), and UTF-8.
check_path() {
    case $1 in *"$nl"*) die "can't use a path with a line break: $1" ;; esac
    if printf '%s' "$1" | LC_ALL=C grep -q '[[:cntrl:]]'; then
        die "can't use a path with control characters: $1"
    fi
    printf '%s' "$1" | iconv -f UTF-8 -t UTF-8 >/dev/null 2>&1 || die "can't use a path that isn't UTF-8: $1"
}

# Serializes installs and uninstalls of the same place. The lock file stays
# (one removed would let a run waiting on it and a run opening a new one
# both go ahead).
lock() {
    mkdir -p "$STATE"
    exec 9>>"$STATE/install.lock"
    flock 9
    # (For tests: holds on to the lock while this file is there.)
    while [ -n "${TINE_INSTALL_PAUSE:-}" ] && [ -e "$TINE_INSTALL_PAUSE" ]; do
        sleep 0.05
    done
}

hash_of() {
    sha256sum <"$1" | cut -d' ' -f1
}

# Whether $1 with hash $2 is a file a Tine installation put there, as it
# put it (by the manifest, or the one of an installation cut short).
owned() {
    for m in "$MANIFEST" "$PENDING"; do
        [ -f "$m" ] || continue
        if P=$1 H=$2 awk -F "$tab" '$1 == ENVIRON["H"] && $2 == ENVIRON["P"] { found = 1 } END { exit !found }' "$m"; then
            return 0
        fi
    done
    return 1
}

# Keeps desktop caches current where they are kept (a cache made where
# there was none could later hide other programs' icons).
refresh_caches() {
    if [ -f "$DATA/applications/mimeinfo.cache" ] && command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$DATA/applications" >/dev/null 2>&1 || true
    fi
    if [ -f "$DATA/icons/hicolor/icon-theme.cache" ] && command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -f -t "$DATA/icons/hicolor" >/dev/null 2>&1 || true
    fi
    if command -v kbuildsycoca6 >/dev/null 2>&1; then
        kbuildsycoca6 >/dev/null 2>&1 || true
    fi
}
