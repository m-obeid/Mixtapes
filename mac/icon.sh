#!/usr/bin/env bash
# Compiles mac/Mixtapes.icon, the layered Liquid Glass icon from Icon Composer,
# into OUT_DIR/Assets.car for macOS 26 and later.
#
#   mac/icon.sh OUT_DIR [MIN_MACOS]
#
# Needs Xcode 26's actool running on macOS 26: on older macOS its asset agent
# loads frameworks only macOS 26 has, and crashes. CI compiles the icon in a
# job of its own for that reason and hands Assets.car to the build jobs.
set -euo pipefail

out=${1:?usage: mac/icon.sh OUT_DIR [MIN_MACOS]}
min_macos=${2:-15.0}
mkdir -p "$out"
xcrun actool mac/Mixtapes.icon --compile "$out" --platform macosx --minimum-deployment-target "$min_macos" \
    --app-icon Mixtapes --output-partial-info-plist "$out/partial.plist" > /dev/null
test -f "$out/Assets.car"
