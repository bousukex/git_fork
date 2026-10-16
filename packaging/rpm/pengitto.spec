# pengitto for Fedora, built from a source snapshot with its crates vendored
# (no network while building). Made by scripts/rpm.sh, which writes the
# version in (from Cargo.toml) in the snapshot it builds from.
%global debug_package %{nil}

Name:           pengitto
Version:        @VERSION@
Release:        1%{?dist}
Summary:        A Git client for Linux, after Fork's layout
# pengitto's own (LICENSE-MIT, LICENSE-APACHE); THIRD_PARTY_LICENSES lists what
# is built in.
License:        MIT OR Apache-2.0
URL:            https://github.com/bousukex/git_fork
Source0:        pengitto-%{version}.tar.gz
ExclusiveArch:  x86_64

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  desktop-file-utils

# legacy-names:begin
# pengitto was called Tine before (0.1.0): its package is replaced.
Obsoletes:      tine < 0.2.0
# legacy-names:end

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
pengitto is a Git client for Linux with the layout and feel of Fork: history
with the commit graph, staging by file, hunk and line, branches, merges,
rebases (interactive too), a 3-way merge editor, worktrees and Git LFS.

%prep
%autosetup -n pengitto-%{version}

%build
# Only the vendored crates (packaged in the source), nothing cached.
export CARGO_HOME="$PWD/.cargo-home"
cargo build --release --locked --offline -p pengitto

%install
sh scripts/install.sh --prefix %{buildroot}%{_prefix} --binary target/release/pengitto \
    --exec %{_bindir}/pengitto --no-integration --no-manifest
# The licenses go with %%license instead.
rm -rf %{buildroot}%{_datadir}/doc/pengitto

%check
target/release/pengitto --version

%files
%license LICENSE-MIT LICENSE-APACHE packaging/THIRD_PARTY_LICENSES
%{_bindir}/pengitto
%{_datadir}/applications/pengitto.desktop
%{_datadir}/icons/hicolor/*/apps/pengitto.png
