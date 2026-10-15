#!/usr/bin/env bash
# Cross-compiles mdview for RHEL 8 on ppc64le (glibc 2.28, GTK 3.22) from any machine,
# so nothing needs to be installed (and no sudo is needed) on the target.
#
# Needs: rustup target powerpc64le-unknown-linux-gnu, zig, cargo-zigbuild, curl, and a
# tar that can read RPMs (bsdtar/libarchive — the default tar on macOS; on Linux use
# `bsdtar` or install rpm2cpio and adapt the extract step).
#
# Output: target/powerpc64le-unknown-linux-gnu/release/mdview
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
work="${MDVIEW_CROSS_DIR:-$root/target/el8-cross}"
sysroot="$work/sysroot"
mirror="https://repo.almalinux.org/almalinux/8"   # binary-compatible with RHEL 8
target=powerpc64le-unknown-linux-gnu

# Runtime libraries mdview links against (headers aren't needed: the Rust bindings declare everything).
packages=(
  BaseOS/glib2-2.56.4-178.el8_10
  BaseOS/gdk-pixbuf2-2.36.12-8.el8_10
  AppStream/atk-2.28.1-1.el8
  AppStream/cairo-1.15.12-6.el8
  AppStream/cairo-gobject-1.15.12-6.el8
  AppStream/gtk3-3.22.30-12.el8_10
  AppStream/harfbuzz-1.7.5-4.el8
  AppStream/pango-1.42.4-8.el8
)

if [ ! -e "$sysroot/usr/lib64/libgtk-3.so" ]; then
  echo "==> Fetching RHEL 8 compatible ppc64le GTK libraries"
  mkdir -p "$work/rpms" "$sysroot"
  for p in "${packages[@]}"; do
    f="$(basename "$p").ppc64le.rpm"
    [ -f "$work/rpms/$f" ] || curl -fsSL -o "$work/rpms/$f" "$mirror/${p%%/*}/ppc64le/os/Packages/$f"
    tar -xf "$work/rpms/$f" -C "$sysroot" ./usr/lib64
  done
  for so in libgtk-3 libgdk-3 libpangocairo-1.0 libpango-1.0 libharfbuzz libatk-1.0 \
            libcairo-gobject libcairo libgdk_pixbuf-2.0 libgio-2.0 libgobject-2.0 libglib-2.0; do
    real="$(cd "$sysroot/usr/lib64" && ls "$so".so.* | head -1)"
    ln -sf "$real" "$sysroot/usr/lib64/$so.so"
  done
fi

echo "==> Compiling glibc 2.28 compatibility shim"
zig cc -target powerpc64le-linux-gnu.2.28 -mabi=ibmlongdouble -O2 -c "$here/ieee128-shim.c" -o "$work/ieee128-shim.o"

echo "==> Building mdview"
export MDVIEW_SYSROOT_LIBS="$sysroot/usr/lib64"
export PKG_CONFIG="$here/pkg-config"
export PKG_CONFIG_ALLOW_CROSS=1
export CARGO_TARGET_POWERPC64LE_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=$work/ieee128-shim.o"
cd "$root"
cargo zigbuild --release --target "$target.2.28"

out="$root/target/$target/release/mdview"
echo "==> Built $out ($(du -h "$out" | cut -f1))"
