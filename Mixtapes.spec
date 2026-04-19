# -*- mode: python ; coding: utf-8 -*-
import os
import ytmusicapi

brew = os.popen("brew --prefix").read().strip()
ytmusicapi_locales_path = os.path.join(os.path.dirname(ytmusicapi.__file__), 'locales')


a = Analysis(
    ["src/main.py"],
    pathex=["src"],
    binaries=[
        (f"{brew}/lib/libgtk-4.1.dylib", "."),
        (f"{brew}/lib/libadwaita-1.0.dylib", "."),
        (f"{brew}/lib/libgstreamer-1.0.0.dylib", "."),
        (f"{brew}/lib/libgobject-2.0.0.dylib", "."),
        (f"{brew}/lib/libglib-2.0.0.dylib", "."),
    ],
    datas=[
        (ytmusicapi_locales_path, 'ytmusicapi/locales'),
        ('muse.gresource', '.'),
        ("src/ui", "ui"),
        ("src/player", "player"),
        ("assets", "assets"),
        (f"{brew}/lib/girepository-1.0", "girepository-1.0"),
        (f"{brew}/share/glib-2.0/schemas", "share/glib-2.0/schemas"),
        (f"{brew}/share/icons/Adwaita", "share/icons/Adwaita"),
    ],
    hiddenimports=[],
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[],
    noarchive=False,
    optimize=0,
)
pyz = PYZ(a.pure)

exe = EXE(
    pyz,
    a.scripts,
    [],
    exclude_binaries=True,
    name="Mixtapes",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=True,
    console=False,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
)
coll = COLLECT(
    exe,
    a.binaries,
    a.datas,
    strip=False,
    upx=True,
    upx_exclude=[],
    name="Mixtapes",
)
app = BUNDLE(
    coll,
    name="Mixtapes.app",
    icon="mac/com.pocoguy.Muse.icns",
    bundle_identifier="com.pocoguy.Muse",
)
