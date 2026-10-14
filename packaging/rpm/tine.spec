# Tine for Fedora, built from a source snapshot with its crates vendored
# (no network while building). Made by scripts/rpm.sh, which writes the
# version in (from Cargo.toml) in the snapshot it builds from.
%global debug_package %{nil}

Name:           tine
Version:        @VERSION@
Release:        1%{?dist}
Summary:        A Git client for Linux, after Fork's layout
# Tine's own license isn't decided yet; this is what Cargo.toml declares.
# THIRD_PARTY_LICENSES lists what is built in.
License:        MIT OR Apache-2.0
URL:            https://github.com/bousukex/git_fork
Source0:        tine-%{version}.tar.gz
ExclusiveArch:  x86_64

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  desktop-file-utils

Requires:       git-core >= 2.43
Requires:       fontconfig
Requires:       hicolor-icon-theme
# Loaded at run time (not linked, so rpm can't see them).
Requires:       libwayland-client.so.0()(64bit)
Requires:       libxkbcommon.so.0()(64bit)
Requires:       (libvulkan.so.1()(64bit) or libEGL.so.1()(64bit))
Recommends:     libwayland-egl.so.1()(64bit)
Recommends:     libX11.so.6()(64bit)
Recommends:     libX11-xcb.so.1()(64bit)
Recommends:     libXcursor.so.1()(64bit)
Recommends:     libXi.so.6()(64bit)
Recommends:     libxkbcommon-x11.so.0()(64bit)
Recommends:     libdbus-1.so.3()(64bit)
# Opening files and folders, and the file dialog.
Recommends:     xdg-utils
Recommends:     dbus-tools
Recommends:     xdg-desktop-portal

%description
Tine is a Git client for Linux with the layout and feel of Fork: history
with the commit graph, staging by file, hunk and line, branches, merges,
rebases (interactive too), a 3-way merge editor, worktrees and Git LFS.

%prep
%autosetup -n tine-%{version}

%build
# Only the vendored crates (packaged in the source), nothing cached.
export CARGO_HOME="$PWD/.cargo-home"
cargo build --release --locked --offline -p tine

%install
sh scripts/install.sh --prefix %{buildroot}%{_prefix} --binary target/release/tine \
    --exec %{_bindir}/tine --no-integration --no-manifest
# The licenses go with %%license instead.
rm -rf %{buildroot}%{_datadir}/doc/tine

%check
target/release/tine --version

%files
%license packaging/THIRD_PARTY_LICENSES
%{_bindir}/tine
%{_datadir}/applications/tine.desktop
%{_datadir}/icons/hicolor/*/apps/tine.png
