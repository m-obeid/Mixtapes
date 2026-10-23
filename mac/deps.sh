#!/usr/bin/env bash
# Packs the x86_64 GTK stack the Intel build links against into
# dist/mixtapes-macos-deps-x86_64.tar.xz, for CI to cross-build on an Apple
# silicon runner. Homebrew no longer builds Intel bottles and GitHub retires
# its Intel runners, so the stack comes from MacPorts on an Intel Mac and is
# attached to a release. Rebuild it only to move to newer GTK or GStreamer.
#
# Needs on that Mac:
#   sudo port install gtk4 -x11 +quartz libadwaita glib-networking \
#     gstreamer1-gst-plugins-base gstreamer1-gst-plugins-good \
#     gstreamer1-gst-plugins-bad gstreamer1-gst-plugins-ugly
#
# The tree unpacks to /opt/local on the runner: MacPorts' install names and
# pkg-config files name that prefix.
set -euo pipefail

PREFIX=/opt/local
OUT=$PWD/dist/mixtapes-macos-deps-x86_64.tar.xz
mkdir -p dist

# Which ports went in, for anyone wondering later.
manifest_dir=$(mktemp -d)
"$PREFIX/bin/port" -q installed active > "$manifest_dir/ports.txt"

# Shared libraries and their links, pkg-config files, the modules GStreamer
# and GIO load, the plugin scanner, compiled schemas and the hicolor theme.
# No headers: the Rust bindings link through pkg-config and compile no C
# against GTK. No static archives.
cd "$PREFIX"
files=(lib/*.dylib lib/pkgconfig lib/gstreamer-1.0 lib/gio/modules libexec/gstreamer-1.0/gst-plugin-scanner
    share/glib-2.0/schemas/gschemas.compiled share/icons/hicolor)
# Some ports link their libraries and pkg-config files into lib from
# elsewhere: OpenSSL and SDL from libexec, libstdc++ from lib/libgcc. The
# targets go in too, or the links would dangle on the runner.
while IFS= read -r link; do
    target=$(readlink "$link")
    case "$target" in
        "$PREFIX"/*) files+=("${target#"$PREFIX"/}") ;;
        */*) files+=("$(dirname "$link")/$target") ;;
    esac
done < <(find lib/*.dylib lib/pkgconfig -type l)
tar -cJf "$OUT" -C "$manifest_dir" ports.txt -C "$PREFIX" "${files[@]}"
rm -rf "$manifest_dir"

echo "$OUT ($(du -h "$OUT" | cut -f1))"
shasum -a 256 "$OUT"
