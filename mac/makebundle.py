#!/usr/bin/env python3

import os
import sys
from argparse import ArgumentParser
from subprocess import CalledProcessError
from subprocess import check_output
from subprocess import run

if __name__ == "__main__":
    if not os.path.isdir("mac"):
        sys.exit(
            'Unable to find "mac" directory. make sure you run '
            "this script from the project root"
        )

    parser = ArgumentParser(
        description="Create a macOS .app bundle. "
        "Requires PyInstaller and hdiutil (macOS)."
    )
    parser.add_argument("--version", help="version number of the .app bundle")
    args = parser.parse_args()

    if args.version:
        version = args.version
    else:
        try:
            version = check_output(["git", "describe", "--tags"])
            version = version.decode().strip()
        except CalledProcessError:
            version = "unknown"
    dmg_name = f"mixtapes-{version}.dmg"

    # the .spec has to be in the project root
    run(["cp", "mac/com.pocoguy.Muse.spec", "com.pocoguy.Muse.spec"], check=True)
    run(["pyinstaller", "com.pocoguy.Muse.spec"], check=True)
    # we only want Gajim.app in the dmg
    run(["rm", "-rf", "dist/launch"], check=True)
    run(
        [
            "hdiutil",
            "create",
            "-volname",
            "Gajim",
            "-srcfolder",
            "dist",
            "-ov",
            "-format",
            "UDZO",
            dmg_name,
        ],
        check=True,
    )
