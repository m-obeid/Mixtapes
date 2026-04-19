# Compile gresource file
glib-compile-resources src/muse.gresource.xml --target=muse.gresource

# Bundle Mixtapes with PyInstaller
pyinstaller Mixtapes.spec --noconfirm
