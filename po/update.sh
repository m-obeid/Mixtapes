#!/bin/sh
# Rebuild po/mixtapes.pot from the source, then merge it into every translation in po/LINGUAS.
# Run it after changing text in the app. Needs GNU gettext 0.24 or newer, the first to read Rust.
set -eu
cd "$(dirname "$0")/.."

# src/i18n/tests.rs holds made-up messages for the unit tests.
find src -name '*.rs' ! -path 'src/i18n/tests.rs' | LC_ALL=C sort | xgettext \
    --files-from=- \
    --language=Rust \
    --from-code=UTF-8 \
    --add-comments=Translators \
    --keyword \
    --keyword='tr!' \
    --keyword='trn!:1,2' \
    --keyword='trc!:1c,2' \
    --keyword='tr_noop!' \
    --keyword='trc_noop!:1c,2' \
    --package-name=mixtapes \
    --msgid-bugs-address=https://github.com/m-obeid/Mixtapes/issues \
    --output=po/mixtapes.pot

grep -v '^#' po/LINGUAS | while read -r lang; do
    [ -n "$lang" ] || continue
    if [ -f "po/$lang.po" ]; then
        msgmerge --quiet --update --backup=none "po/$lang.po" po/mixtapes.pot
    else
        msginit --no-translator --locale="$lang" --input=po/mixtapes.pot --output-file="po/$lang.po"
    fi
done
