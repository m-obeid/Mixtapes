#!/usr/bin/env bash
# Builds MSYS2's gtk4 package with windows/gtk-patches/*.patch applied.
#
#   windows/build-gtk.sh fetch   clones the recipe into gtk-recipe/ and prints its version
#   windows/build-gtk.sh build   builds the package into gtk-pkg/
#
# The recipe is MSYS2's own, so the package matches theirs apart from the patches.
set -euo pipefail

case "${1:-}" in
fetch)
    rm -rf gtk-recipe
    git clone --quiet --depth 1 --filter=blob:none --sparse https://github.com/msys2/MINGW-packages gtk-recipe
    git -C gtk-recipe sparse-checkout set mingw-w64-gtk4
    sed -n 's/^pkgver=//p; s/^pkgrel=//p' gtk-recipe/mingw-w64-gtk4/PKGBUILD | paste -sd-
    ;;
build)
    recipe=gtk-recipe/mingw-w64-gtk4
    for patch in windows/gtk-patches/*.patch; do
        cp "$patch" "$recipe/mixtapes-$(basename "$patch")"
    done
    # Applied as soon as prepare() enters the source tree.
    sed -i '/^  cd gtk-\${pkgver}$/a\  for p in "${startdir}"/mixtapes-*.patch; do patch -Np1 -i "$p"; done' "$recipe/PKGBUILD"
    if ! grep -q 'mixtapes-\*\.patch' "$recipe/PKGBUILD"; then
        echo "MSYS2's gtk4 recipe changed, the patches have nowhere to go" >&2
        exit 1
    fi
    (cd "$recipe" && MINGW_ARCH=ucrt64 makepkg-mingw --syncdeps --cleanbuild --force --noconfirm --skippgpcheck --nocheck)
    mkdir -p gtk-pkg
    cp "$recipe"/mingw-w64-ucrt-x86_64-gtk4-*.pkg.tar.zst gtk-pkg/
    ;;
*)
    echo "usage: $0 fetch|build" >&2
    exit 2
    ;;
esac
