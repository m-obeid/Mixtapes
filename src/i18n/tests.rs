//! Kept out of mod.rs so po/update.sh can skip the made-up messages below.

use super::*;

    fn catalog(text: &str) -> (&'static RawCatalog, Catalog) {
        let compiled = po::compile(&po::parse(text).unwrap());
        let entries: Vec<(&'static str, &'static [&'static str])> = compiled
            .entries
            .into_iter()
            .map(|(key, forms)| {
                let forms: Vec<&'static str> = forms.into_iter().map(|form| &*form.leak()).collect();
                (&*key.leak(), &*forms.leak())
            })
            .collect();
        let raw: &'static RawCatalog = Box::leak(Box::new(RawCatalog { lang: "pl", plural_forms: compiled.plural_forms.leak(), entries: entries.leak() }));
        (raw, Catalog::load(raw))
    }

    const POLISH: &str = r#"
msgid ""
msgstr ""
"Plural-Forms: nplurals=3; plural=(n==1 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);\n"

msgid "Home"
msgstr "Główna"

msgctxt "verb"
msgid "Play"
msgstr "Odtwórz"

msgid "{n} song"
msgid_plural "{n} songs"
msgstr[0] "{n} utwór"
msgstr[1] "{n} utwory"
msgstr[2] "{n} utworów"
"#;

    #[test]
    fn catalog_lookups() {
        let (_, catalog) = catalog(POLISH);
        assert_eq!(catalog.get("Home"), Some("Główna"));
        assert_eq!(catalog.get("Play"), None);
        assert_eq!(catalog.get(&po::key(Some("verb"), "Play")), Some("Odtwórz"));
        assert_eq!(catalog.get_plural("{n} song", 1), Some("{n} utwór"));
        assert_eq!(catalog.get_plural("{n} song", 3), Some("{n} utwory"));
        assert_eq!(catalog.get_plural("{n} song", 5), Some("{n} utworów"));
        assert_eq!(catalog.get_plural("{n} album", 5), None);
    }

    #[test]
    fn english_without_a_catalog() {
        // No test calls init, so the process runs in English.
        assert_eq!(tr!("Home"), "Home");
        assert_eq!(trc!("verb", "Play"), "Play");
        assert_eq!(trn!("{n} song", "{n} songs", 1), "1 song");
        assert_eq!(trn!("{n} song", "{n} songs", 2_usize), "2 songs");
        let name = "Mix";
        assert_eq!(tr!("Added to {name}", name), "Added to Mix");
        assert_eq!(tr!("{a} of {b}", a = 1 + 1, b = "ten"), "2 of ten");
        assert_eq!(trn!("{n} song by {artist}", "{n} songs by {artist}", 4, artist = "X"), "4 songs by X");
    }

    #[test]
    fn format_handles_braces() {
        assert_eq!(format("{{literal}} {x}", &[("x", &1)]), "{literal} 1");
        assert_eq!(format("{unknown} {x", &[("x", &1)]), "{unknown} {x");
        assert_eq!(format("{x}{x}", &[("x", &"ab")]), "abab");
        assert_eq!(format("a } b", &[]), "a } b");
    }

    #[test]
    fn pick_follows_the_preference_order() {
        let (raw, _) = catalog(POLISH);
        let catalogs = std::slice::from_ref(raw);
        assert_eq!(pick(["pl_PL.UTF-8", "pl_PL", "pl", "C"].into_iter(), catalogs).map(|c| c.lang), Some("pl"));
        assert_eq!(pick(["de_DE", "de", "pl", "C"].into_iter(), catalogs).map(|c| c.lang), Some("pl"));
        assert!(pick(["en_US", "en", "pl", "C"].into_iter(), catalogs).is_none());
        assert!(pick(["C"].into_iter(), catalogs).is_none());
    }

    #[test]
    fn shipped_catalogs_are_sorted_and_parse() {
        for raw in CATALOGS {
            assert!(raw.entries.is_sorted_by(|a, b| a.0 < b.0), "{} is not sorted", raw.lang);
            assert!(raw.plural_forms.is_empty() || Plural::parse(raw.plural_forms).is_some(), "{} has a broken plural rule", raw.lang);
        }
    }
