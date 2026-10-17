#!/bin/sh
# Installs the fastpaste package from DIR into the clean container this runs
# in, as root, and checks what a fresh install needs: the dependencies
# resolve, every library the binary links or dlopens is there, the files
# landed, and the binary runs.
#
#   smoke-test.sh DIR VERSION
#
# DIR holds the .deb and the .rpm; the one this distribution installs is
# picked by its package manager. VERSION is the Cargo version (0.1.0).
#
# The release workflow runs it on each supported distribution. By hand:
#
#   docker run --rm -v "$PWD:/src:ro" -w /src ubuntu:22.04 \
#       sh packaging/linux/smoke-test.sh target/debian 0.1.0
#
# It cannot show a window — that needs a Wayland session — so the GUI
# itself is covered by --version and the library checks only.
set -eu

dir=$1
version=$2

fail() {
    echo "smoke-test: FAIL: $*" >&2
    exit 1
}

# Libraries winit, glutin and xkbcommon-dl dlopen: the package declares
# them by hand, so nothing else catches a missing one. Keep in step with
# the list in crates/fastpaste-gui/Cargo.toml.
DLOPENED="libwayland-client.so.0 libwayland-egl.so.1 libxkbcommon.so.0 libEGL.so.1"

if command -v apt-get >/dev/null 2>&1; then
    set -- "$dir"/fastpaste_*.deb
    [ -f "$1" ] || fail "no .deb in $dir"
    pkg=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends "$pkg"
    dpkg -s fastpaste | grep -E '^(Version|Depends):'
elif command -v dnf >/dev/null 2>&1; then
    set -- "$dir"/fastpaste-*.rpm
    [ -f "$1" ] || fail "no .rpm in $dir"
    dnf install -y -q "$1"
    rpm -q --requires fastpaste
    # cargo-generate-rpm uses a script path it cannot find as the script
    # text itself, so a wrong path in Cargo.toml still builds.
    rpm -q --scripts fastpaste | grep -q 'udevadm trigger' ||
        fail "the post-install script is not the one in packaging/linux/scripts"
else
    fail "neither apt-get nor dnf found"
fi

got=$(fastpaste --version) || fail "fastpaste --version exited non-zero"
[ "$got" = "fastpaste $version" ] || fail "--version printed '$got', expected 'fastpaste $version'"
echo "ok: $got"

missing=$(ldd /usr/bin/fastpaste | grep 'not found' || true)
[ -z "$missing" ] || fail "unresolved linked libraries: $missing"
echo "ok: every linked library resolves"

for so in $DLOPENED; do
    ldconfig -p | awk -v so="$so" '$1 == so { found = 1 } END { exit !found }' ||
        fail "$so (dlopened) is not installed"
done
echo "ok: every dlopened library is installed: $DLOPENED"

for f in \
    /usr/share/applications/fastpaste.desktop \
    /usr/share/icons/hicolor/256x256/apps/fastpaste.png \
    /usr/share/icons/hicolor/32x32/apps/fastpaste.png \
    /usr/lib/udev/rules.d/70-fastpaste-uinput.rules; do
    [ -f "$f" ] || fail "$f is missing"
done
echo "ok: desktop file, icons and udev rule installed"

# Last, because it installs more packages, which could otherwise hide a
# dependency the package itself fails to pull in.
if command -v apt-get >/dev/null 2>&1; then
    apt-get install -y -qq --no-install-recommends desktop-file-utils >/dev/null
else
    dnf install -y -q desktop-file-utils >/dev/null
fi
desktop-file-validate /usr/share/applications/fastpaste.desktop ||
    fail "fastpaste.desktop does not validate"
echo "ok: fastpaste.desktop validates"

echo "smoke-test: PASS"
