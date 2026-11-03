#!/usr/bin/env bash
# Assembles dist/mixtapes, a self-contained Windows install of the release build.
# Run from the repository root in an MSYS2 UCRT64 shell after `cargo build --release`.
#
# Layout, the same as an MSYS2 prefix, so GLib, GTK and GStreamer find their
# data relative to their DLLs without any environment variables:
#   bin/      mixtapes.exe, every DLL it needs, yt-dlp, node, ffmpeg, botguard
#   lib/      GStreamer plugins, GIO modules, gdk-pixbuf loaders
#   libexec/  gst-plugin-scanner
#   share/    GSettings schemas, Adwaita and hicolor icon themes, Adwaita fonts
#   etc/      fonts.conf
set -euo pipefail

PREFIX="${MSYSTEM_PREFIX:?run this from an MSYS2 shell}"
OUT=dist/mixtapes
rm -rf "$OUT"
mkdir -p "$OUT/bin" "$OUT/lib" "$OUT/libexec/gstreamer-1.0" "$OUT/share/glib-2.0/schemas" "$OUT/share/icons"

echo "App and helper programs"
cp target/release/mixtapes.exe "$OUT/bin/"
# The installer points the taskbar identity (AppUserModelID) at this.
cp windows/mixtapes.ico "$OUT/bin/"
# yt-dlp reaches these through PATH, which the app starts with its own folder.
cp "$PREFIX/bin/node.exe" "$PREFIX/bin/ffmpeg.exe" "$PREFIX/bin/ffprobe.exe" "$OUT/bin/"
# The official build bundles yt-dlp-ejs, the YouTube challenge solver node runs.
curl -fsSL --retry 5 -o "$OUT/bin/yt-dlp.exe" https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe
# GLib starts its own session bus with this. GApplication needs the bus to
# find a running Mixtapes, so a second launch hands over instead of opening
# another window.
cp "$PREFIX/bin/gdbus.exe" "$OUT/bin/"
# WebView2's loader for the sign-in page, from the webview2-com-sys build.
cp "$(ls -t target/release/build/webview2-com-sys-*/out/x64/WebView2Loader.dll | head -n 1)" "$OUT/bin/"
# Vendored, since Codeberg's bot filter rejects CI downloads.
BG_ZIP=vendor/rustypipe-botguard/rustypipe-botguard-v0.1.2-x86_64-pc-windows-msvc.zip
echo "edb9e482ca77e3e4ecd4a2d3270f58c9169f03770db1568b3429cc8cf4c4b279  $BG_ZIP" | sha256sum -c -
unzip -o -q "$BG_ZIP" rustypipe-botguard.exe -d "$OUT/bin"

echo "GStreamer, GIO and gdk-pixbuf modules"
cp -r "$PREFIX/lib/gstreamer-1.0" "$OUT/lib/"
rm -f "$OUT"/lib/gstreamer-1.0/*.a "$OUT"/lib/gstreamer-1.0/*.la
cp "$PREFIX/libexec/gstreamer-1.0/gst-plugin-scanner.exe" "$OUT/libexec/gstreamer-1.0/"
mkdir -p "$OUT/lib/gio/modules"
cp "$PREFIX"/lib/gio/modules/*.dll "$OUT/lib/gio/modules/"
cp -r "$PREFIX/lib/gdk-pixbuf-2.0" "$OUT/lib/"
# souphttpsrc loads libsoup at run time, so no import table names it.
cp "$PREFIX/bin/libsoup-3.0-0.dll" "$OUT/bin/"
# ANGLE, which the patched GTK draws through. libepoxy loads it at runtime, so
# the import scan below finds only what these two need.
cp "$PREFIX/bin/libEGL.dll" "$PREFIX/bin/libGLESv2.dll" "$OUT/bin/"

echo "Fonts"
# Adwaita Sans and Mono, which style.css asks for. fonts.conf points
# fontconfig at them and at the Windows font folders.
mkdir -p "$OUT/share/fonts" "$OUT/etc/fonts"
cp fonts/*.ttf fonts/LICENSE.adwaita-fonts "$OUT/share/fonts/"
cp windows/fonts.conf "$OUT/etc/fonts/fonts.conf"

echo "Schemas and icons"
cp "$PREFIX/share/glib-2.0/schemas/gschemas.compiled" "$OUT/share/glib-2.0/schemas/"
cp -r "$PREFIX/share/icons/Adwaita" "$PREFIX/share/icons/hicolor" "$OUT/share/icons/"

echo "DLL dependencies"
# ntldd -R walks import tables recursively. Anything outside the MSYS2 prefix
# is a Windows system DLL and stays out. One ntldd and one cygpath run for the
# whole tree: a process per line took over half an hour on the CI runner.
prefix_bin=$(cygpath -u "$PREFIX/bin" | tr '[:upper:]' '[:lower:]')
find "$OUT" \( -iname '*.exe' -o -iname '*.dll' \) -print0 \
    | xargs -0 ntldd -R \
    | sed -n 's/.* => \(.*\) (0x[0-9a-fA-F]*)$/\1/p' \
    | sort -u \
    | cygpath -u -f - \
    | awk -v bin="$prefix_bin/" 'index(tolower($0), bin) == 1 && index(substr($0, length(bin) + 1), "/") == 0' \
    | sort -u \
    | xargs -r -d '\n' cp -t "$OUT/bin/"

# The loaders cache should hold paths relative to the install. Printed so a
# broken one shows up in the CI log.
head -n 8 "$OUT/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"

echo "Bundle size: $(du -sh "$OUT" | cut -f1)"
