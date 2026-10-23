#!/usr/bin/env bash
# Assembles dist/Mixtapes.app and dist/Mixtapes-<version>-<arch>.dmg from a release build.
# Run from the repository root on a Mac after `cargo build --release` (add --target for a cross build).
#
#   PREFIX  where GTK, libadwaita and GStreamer are installed. Homebrew is /opt/homebrew
#           (/usr/local on Intel), MacPorts is /opt/local. Found by itself when unset.
#   TARGET  the cargo target triple of a cross build, such as x86_64-apple-darwin.
#
# Layout:
#   Contents/MacOS/       mixtapes, gst-plugin-scanner, yt-dlp, node, ffmpeg, ffprobe
#   Contents/Frameworks/  every dylib the above and the modules below need
#   Contents/Resources/   lib/ (GStreamer plugins, GIO modules),
#                         share/ (schemas, icons, fonts), mixtapes.icns
# bootstrap::use_bundled_runtime points GLib, GTK and GStreamer at these at startup.
set -euo pipefail

if [[ -z "${PREFIX:-}" ]]; then
    for candidate in /opt/homebrew /opt/local /usr/local; do
        if [[ -f "$candidate/lib/pkgconfig/gtk4.pc" ]]; then
            PREFIX=$candidate
            break
        fi
    done
fi
: "${PREFIX:?no GTK 4 install found, set PREFIX}"
# This prefix first. A cross build also sets PKG_CONFIG_LIBDIR to keep the
# runner's own packages out (see .github/workflows/build-macos.yml).
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig:$PREFIX/share/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
PKG_CONFIG=${PKG_CONFIG:-$(command -v pkg-config || command -v pkgconf || echo "$PREFIX/bin/pkg-config")}

BIN=target/${TARGET:+$TARGET/}release/mixtapes
[[ -x "$BIN" ]] || { echo "$BIN is missing, build it first" >&2; exit 1; }
ARCH=$(lipo -archs "$BIN")
VERSION=${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)}
MIN_MACOS=${MIN_MACOS:-$(otool -l "$BIN" | awk '/LC_BUILD_VERSION/ { found = 1 } found && $1 == "minos" { print $2; exit }')}

APP=dist/Mixtapes.app
C=$APP/Contents
R=$C/Resources
rm -rf "$APP"
mkdir -p "$C/MacOS" "$C/Frameworks" "$R/lib/gstreamer-1.0" "$R/lib/gio/modules" "$R/share/glib-2.0/schemas" "$R/share/icons" "$R/share/fonts"

echo "App, Info.plist and icon"
cp "$BIN" "$C/MacOS/mixtapes"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@MIN_MACOS@/$MIN_MACOS/g" mac/Info.plist > "$C/Info.plist"
printf 'APPL????' > "$C/PkgInfo"
# mixtapes.icns is Icon Composer's Default export of mac/Mixtapes.icon, for
# macOS 15 and earlier.
cp mac/mixtapes.icns "$R/"
# The layered Liquid Glass icon from Icon Composer (mac/Mixtapes.icon), for
# macOS 26 and later, which frame the flat icns in a grey square. ICON_CAR
# names an Assets.car compiled beforehand (CI does that on a macOS 26 runner);
# otherwise mac/icon.sh compiles it here when it can. Without it the flat icon
# is all the app gets.
icon_out=$(mktemp -d)
if [[ -n "${ICON_CAR:-}" ]]; then
    cp "$ICON_CAR" "$icon_out/Assets.car"
elif ! xcrun --find actool > /dev/null 2>&1; then
    echo "  no actool (Xcode 26), flat icon only"
elif ! bash mac/icon.sh "$icon_out" "$MIN_MACOS" 2> "$icon_out/actool.log"; then
    echo "  actool failed (it needs macOS 26), flat icon only"
fi
if [[ -f "$icon_out/Assets.car" ]]; then
    cp "$icon_out/Assets.car" "$R/"
    /usr/libexec/PlistBuddy -c "Add :CFBundleIconName string Mixtapes" "$C/Info.plist"
fi
rm -rf "$icon_out"

echo "GStreamer"
plugins_dir=$("$PKG_CONFIG" --variable=pluginsdir gstreamer-1.0)
scanner_dir=$("$PKG_CONFIG" --variable=pluginscannerdir gstreamer-1.0)
cp "$scanner_dir/gst-plugin-scanner" "$C/MacOS/"
# Only what playback needs. Copying all of them drags in GTK 3, X11 and PulseAudio.
#   playbin and its parts, the macOS output (osxaudio, whose atdec decodes AAC
#   through AudioToolbox), HTTPS, the visualizer's spectrum, Opus/WebM and
#   M4A streams, downloaded MP3, FLAC, Ogg and WAV files, and HLS for live stations.
PLUGINS=(
    coreelements playback typefindfunctions audioconvert audioresample volume app
    autodetect osxaudio soup spectrum equalizer
    matroska isomp4 opus opusparse audioparsers
    mpg123 flac ogg vorbis wavparse id3demux icydemux apetag
    adaptivedemux2 hls
)
for name in "${PLUGINS[@]}"; do
    found=$(ls "$plugins_dir"/libgst"$name".{dylib,so} 2>/dev/null | head -n 1 || true)
    if [[ -n "$found" ]]; then
        cp "$found" "$R/lib/gstreamer-1.0/"
    else
        echo "  not installed: $name"
    fi
done

# souphttpsrc and adaptivedemux2 open libsoup at run time by its bare name, so
# no load command names it. It goes in Frameworks by hand, and the app loads
# it at startup (bootstrap::use_bundled_runtime).
soup_lib=$("$PKG_CONFIG" --variable=libdir libsoup-3.0)/libsoup-3.0.0.dylib
cp "$soup_lib" "$C/Frameworks/"

echo "GIO modules"
# glib-networking's TLS backend, for HTTPS inside GStreamer. It trusts the macOS keychain.
# From the prefix, not gio-2.0.pc: Homebrew's names glib's own keg, while
# glib-networking links its module into the prefix.
cp "$PREFIX"/lib/gio/modules/libgiognutls.* "$R/lib/gio/modules/"

echo "Helper programs"
# The app puts Contents/MacOS first on PATH (bootstrap::prefer_bundled_programs),
# where yt-dlp, and the app's own tagging and cover code, look for these.
case "$ARCH" in
    x86_64) helper_arch=x64 ;;
    arm64) helper_arch=arm64 ;;
    *) echo "no helper programs for $ARCH" >&2; exit 1 ;;
esac
helpers_tmp=$(mktemp -d)
# yt-dlp's macOS build is universal and carries its own Python.
curl -fsSL --retry 5 -o "$C/MacOS/yt-dlp" https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos
# Node runs yt-dlp's YouTube challenge solver. The official build needs only system libraries.
NODE_VERSION=${NODE_VERSION:-v24.22.1}
node_tar="node-$NODE_VERSION-darwin-$helper_arch.tar.gz"
curl -fsSL --retry 5 -o "$helpers_tmp/$node_tar" "https://nodejs.org/dist/$NODE_VERSION/$node_tar"
curl -fsSL --retry 5 "https://nodejs.org/dist/$NODE_VERSION/SHASUMS256.txt" | grep " $node_tar\$" | (cd "$helpers_tmp" && shasum -a 256 -c -)
tar -xzf "$helpers_tmp/$node_tar" -C "$helpers_tmp"
cp "$helpers_tmp/node-$NODE_VERSION-darwin-$helper_arch/bin/node" "$C/MacOS/"
# ffmpeg and ffprobe, which yt-dlp needs to extract audio. Static builds, pinned.
FFMPEG_RELEASE=https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1
ffmpeg_sums="929b375c1182d956c51f7ac25e0b2b0411fb01f6f407aa15c9758efeb4242106  ffmpeg-darwin-x64.gz
d4da574d6e2e197bd259b47d69cf262df9e312af24ad960444f6d806d3d4c186  ffprobe-darwin-x64.gz
8923876afa8db5585022d7860ec7e589af192f441c56793971276d450ed3bbfa  ffmpeg-darwin-arm64.gz
d986a8ec7b030899fe66a8a288ed809a3543338705a3ce178cfb85869c5d80be  ffprobe-darwin-arm64.gz"
for tool in ffmpeg ffprobe; do
    file="$tool-darwin-$helper_arch.gz"
    curl -fsSL --retry 5 -o "$helpers_tmp/$file" "$FFMPEG_RELEASE/$file"
    grep " $file\$" <<< "$ffmpeg_sums" | (cd "$helpers_tmp" && shasum -a 256 -c -)
    gunzip -c "$helpers_tmp/$file" > "$C/MacOS/$tool"
done
chmod +x "$C/MacOS/yt-dlp" "$C/MacOS/node" "$C/MacOS/ffmpeg" "$C/MacOS/ffprobe"
rm -rf "$helpers_tmp"

echo "Schemas, icons and fonts"
# No gdk-pixbuf loaders: PNG and JPEG are built into gdk-pixbuf, and GTK draws
# SVG icons with its own renderer. The empty cache keeps gdk-pixbuf away from
# a package manager's loaders on the same Mac.
mkdir -p "$R/lib/gdk-pixbuf-2.0"
: > "$R/lib/gdk-pixbuf-2.0/loaders.cache"
cp "$PREFIX/share/glib-2.0/schemas/gschemas.compiled" "$R/share/glib-2.0/schemas/"
# Adwaita from GNOME's release, not the package manager's, which can lag years
# behind. It is plain SVG and PNG, the same for every architecture. macOS
# draws its own cursors, so those stay out.
ADWAITA_ICONS=${ADWAITA_ICONS:-51.0}
icons_tmp=$(mktemp -d)
curl -fsSL --retry 5 "https://download.gnome.org/sources/adwaita-icon-theme/${ADWAITA_ICONS%%.*}/adwaita-icon-theme-$ADWAITA_ICONS.tar.xz" | tar -xJ -C "$icons_tmp"
cp -R "$icons_tmp/adwaita-icon-theme-$ADWAITA_ICONS/Adwaita" "$R/share/icons/"
cp "$icons_tmp/adwaita-icon-theme-$ADWAITA_ICONS/index.theme" "$R/share/icons/Adwaita/"
rm -rf "$R/share/icons/Adwaita/cursors" "$icons_tmp"
cp -R "$PREFIX/share/icons/hicolor" "$R/share/icons/"
# Homebrew links its gtk3 demo icons into the Cellar. Dangling copies fail codesign --deep --strict.
find "$R/share/icons/hicolor" -type l ! -exec test -e {} \; -delete
cp fonts/*.ttf fonts/LICENSE.adwaita-fonts "$R/share/fonts/"

echo "Libraries"
# dylibbundler copies every non-system dylib these load into Frameworks and
# points their load paths there. Plugins and modules are loaded by the main
# executable, so @executable_path resolves to Contents/MacOS for them too.
targets=("$C/MacOS/mixtapes" "$C/MacOS/gst-plugin-scanner" "$C/Frameworks/libsoup-3.0.0.dylib")
while IFS= read -r -d '' module; do
    targets+=("$module")
done < <(find "$R/lib" -type f \( -name '*.dylib' -o -name '*.so' \) -print0)
args=()
for file in "${targets[@]}"; do
    args+=(-x "$file")
done
dylibbundler -of -cd -b -d "$C/Frameworks" -p @executable_path/../Frameworks/ -s "$PREFIX/lib" "${args[@]}" > /dev/null

# The bare name the soup plugins ask for, so the copy the app loaded answers it.
install_name_tool -id libsoup-3.0.0.dylib "$C/Frameworks/libsoup-3.0.0.dylib" 2> /dev/null

echo "Signing"
# Ad hoc: no developer identity, but every Mach-O must carry a valid signature
# after dylibbundler rewrote it, or Apple silicon refuses to load it.
find "$C/Frameworks" "$R/lib" -type f \( -name '*.dylib' -o -name '*.so' \) -exec codesign --force --sign - {} + 2> /dev/null
codesign --force --sign - "$C/MacOS/gst-plugin-scanner" "$C/MacOS/yt-dlp" "$C/MacOS/node" "$C/MacOS/ffmpeg" "$C/MacOS/ffprobe" 2> /dev/null
codesign --force --sign - "$APP" 2> /dev/null

echo "Disk image"
# create-dmg has Finder lay the window out over the background, through
# AppleScript. dmgbuild writes the same settings without Finder, but on macOS 15
# Finder ignores the background it records. The first run from a terminal or
# SSH asks for permission to control Finder.
# tiffutil merges the 1x and 2x backgrounds into one image Retina screens pick
# from. The window is 28 points taller than the 640x400 background, for the title bar.
DMG="dist/Mixtapes-$VERSION-$ARCH.dmg"
dmg_tmp=$(mktemp -d)
mkdir "$dmg_tmp/stage"
cp -R "$APP" "$dmg_tmp/stage/"
tiffutil -cathidpicheck mac/dmg-background.png mac/dmg-background@2x.png -out "$dmg_tmp/background.tiff" 2> /dev/null
rm -f "$DMG"
create-dmg --volname Mixtapes --volicon "$R/mixtapes.icns" --background "$dmg_tmp/background.tiff" \
    --window-pos 200 120 --window-size 640 428 --icon-size 128 --text-size 13 \
    --icon Mixtapes.app 160 190 --hide-extension Mixtapes.app --app-drop-link 480 190 \
    --format ULMO --hdiutil-quiet "$DMG" "$dmg_tmp/stage" > /dev/null
rm -rf "$dmg_tmp"

echo "Done: $APP ($(du -sh "$APP" | cut -f1)), $DMG ($(du -sh "$DMG" | cut -f1))"
