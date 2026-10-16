#!/bin/sh
# Makes the source and binary RPMs of pengitto (target/rpm/): from a snapshot
# of the repository (HEAD; with --dirty, also the changes to tracked files
# not committed yet — untracked files never), with every crate vendored
# into the source, so the build needs no network.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
. "$root/scripts/lib/common.sh"

dirty=0 out=$root/target/rpm
while [ $# -gt 0 ]; do
    case $1 in
        --dirty) dirty=1 ;;
        --out) out=${2:?--out needs a folder}; shift ;;
        -h | --help) say "Usage: scripts/rpm.sh [--dirty] [--out DIR]"; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
    shift
done
command -v rpmbuild >/dev/null 2>&1 || die "rpmbuild isn't there (dnf: rpm-build; apt: rpm)"
rev=HEAD
if [ "$dirty" = 1 ]; then
    rev=$(git -C "$root" stash create)
    rev=${rev:-HEAD}
fi
version=$(git -C "$root" show "$rev:Cargo.toml" | awk -F '"' '/^version = / { print $2; exit }')
[ -n "$version" ] || die "can't read the version from Cargo.toml"
case $version in
    *[!A-Za-z0-9._+~^]*) die "an RPM version can't be $version (no '-')" ;;
esac

work=$(mktemp -d "${TMPDIR:-/tmp}/pengitto-rpm.XXXXXX")
trap 'rm -rf "$work"' EXIT
src=$work/pengitto-$version
mkdir -p "$src"
git -C "$root" archive --format=tar "$rev" | tar -x -C "$src"
# The version written into the spec (the source RPM then rebuilds by itself).
spec=$src/packaging/rpm/pengitto.spec
V=$version awk '{ if ($0 == "Version:        @VERSION@") print "Version:        " ENVIRON["V"]; else print }' "$spec" >"$work/pengitto.spec"
grep -qx "Version:        $version" "$work/pengitto.spec" || die "the spec has no Version line to write $version into"
mv -f "$work/pengitto.spec" "$spec"
# The crates of that very snapshot, and cargo told to use them.
(
    cd "$src"
    mkdir -p .cargo
    cargo vendor --locked --versioned-dirs vendor >.cargo/config.toml
)
# The licenses listed must be those of these crates.
(cd "$src" && python3 scripts/third-party.py --check)
tar -czf "$work/pengitto-$version.tar.gz" -C "$work" "pengitto-$version"

nodeps=
# Built elsewhere than Fedora: its build requirements aren't packages here.
[ -f /etc/fedora-release ] || nodeps=--nodeps
rpmbuild -ba $nodeps \
    --define "_topdir $work/rpmbuild" \
    --define "_sourcedir $work" \
    "$spec" >"$work/log" 2>&1 || { tail -40 "$work/log" >&2; die "rpmbuild failed"; }
mkdir -p "$out"
find "$work/rpmbuild/RPMS" "$work/rpmbuild/SRPMS" -name '*.rpm' -exec cp {} "$out/" \;
say "Made:"
ls "$out"/*.rpm | sed 's/^/  /'
