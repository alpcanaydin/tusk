#!/usr/bin/env bash
set -euo pipefail

kind="${1:?usage: package-linux.sh deb|rpm}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
bundle="target/release/bundle"
archive="$bundle/Tusk-$version-linux-x86_64.tar.gz"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/usr"
tar -C "$stage/usr" -xzf "$archive"

case "$kind" in
    deb)
        mkdir -p "$stage/DEBIAN"
        cat > "$stage/DEBIAN/control" <<EOF
Package: tusk
Version: $version
Section: devel
Priority: optional
Architecture: amd64
Maintainer: Tusk contributors
Depends: libc6 (>= 2.39), libstdc++6, libdbus-1-3, libxcb1, libxkbcommon0, libxkbcommon-x11-0, libvulkan1
Description: GPU-rendered database client for Wayland and Xwayland
EOF
        dpkg-deb --build --root-owner-group "$stage" "$bundle/Tusk-$version-ubuntu-amd64.deb"
        ;;
    rpm)
        command -v rpmbuild >/dev/null
        mkdir -p "$stage/rpmbuild" "$stage/rpmroot"
        mv "$stage/usr" "$stage/rpmroot/usr"
        cat > "$stage/tusk.spec" <<EOF
Name: tusk
Version: $version
Release: 1%{?dist}
Summary: GPU-rendered database client for Wayland and Xwayland
License: MIT
Requires: vulkan-loader
BuildArch: x86_64

%description
Tusk database client with bundled SQL language servers.

%install
mkdir -p %{buildroot}
cp -a $stage/rpmroot/usr %{buildroot}/

%files
/usr/bin/tusk
/usr/bin/postgres-language-server
/usr/bin/sqls
/usr/share/applications/tusk.desktop
/usr/share/icons/hicolor/1024x1024/apps/tusk.png
/usr/share/icons/hicolor/scalable/apps/tusk.svg
/usr/share/doc/tusk/LICENSE
EOF
        rpmbuild -bb --define "_topdir $stage/rpmbuild" --define "_rpmdir $bundle" "$stage/tusk.spec"
        mv "$bundle/x86_64/tusk-$version"-*.rpm "$bundle/Tusk-$version-fedora-x86_64.rpm"
        rmdir "$bundle/x86_64"
        ;;
    *) echo "unknown package format: $kind" >&2; exit 2 ;;
esac
