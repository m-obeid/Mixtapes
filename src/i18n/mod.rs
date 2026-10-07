//! Translations of the app's own text.
//!
//! The English text in the source is the message id. Translators work on
//! po/<language>.po, build.rs compiles every language named in po/LINGUAS into
//! the binary, and `init` picks one at startup. Nothing is installed or read
//! from disk, so Flatpak, a distro package and the Windows build behave alike.
//!
//! Call sites use the macros, which po/update.sh teaches xgettext to read:
//! - `tr!("Sign In")`
//! - `tr!("Now using {name}", name)` or `tr!("Now using {name}", name = account.name)`
//! - `trn!("{n} song", "{n} songs", count)`, where `{n}` is the count
//! - `trc!("verb", "Play")` when the same English word needs two translations
//! - `tr_noop!("Never")` in a const table, with `i18n::gettext` where it is shown
//!   (`trc_noop!` and `i18n::pgettext` when it needs a context)
//!
//! Text YouTube sends (shelf titles, "Album", "Single") is not ours to translate.

mod plural;
mod po;

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;

use plural::Plural;

use crate::paths::Paths;

/// One `name` or `name = value` argument of the macros.
#[macro_export]
macro_rules! tr_arg {
    ($name:ident) => {
        (stringify!($name), &$name as &dyn std::fmt::Display)
    };
    ($name:ident = $value:expr) => {
        (stringify!($name), &$value as &dyn std::fmt::Display)
    };
}

/// Translate a message, filling `{name}` placeholders from the arguments.
#[macro_export]
macro_rules! tr {
    ($msgid:literal) => {
        $crate::i18n::gettext($msgid)
    };
    ($msgid:literal, $($name:ident $(= $value:expr)?),+ $(,)?) => {
        $crate::i18n::format(&$crate::i18n::gettext($msgid), &[$($crate::tr_arg!($name $(= $value)?)),+])
    };
}

/// Translate a message with a singular and a plural form. `{n}` is the count.
#[macro_export]
macro_rules! trn {
    ($msgid:literal, $plural:literal, $n:expr $(, $name:ident $(= $value:expr)?)* $(,)?) => {{
        let n = ($n) as u64;
        $crate::i18n::format(&$crate::i18n::ngettext($msgid, $plural, n), &[("n", &n as &dyn std::fmt::Display) $(, $crate::tr_arg!($name $(= $value)?))*])
    }};
}

/// Translate a message under a context, for English text that reads the same
/// in two places and translates differently.
#[macro_export]
macro_rules! trc {
    ($context:literal, $msgid:literal) => {
        $crate::i18n::pgettext($context, $msgid)
    };
    ($context:literal, $msgid:literal, $($name:ident $(= $value:expr)?),+ $(,)?) => {
        $crate::i18n::format(&$crate::i18n::pgettext($context, $msgid), &[$($crate::tr_arg!($name $(= $value)?)),+])
    };
}

/// Mark a message for translators where no lookup runs yet, as in a const
/// table. The text is unchanged. Pass it to `i18n::gettext` where it is shown.
#[macro_export]
macro_rules! tr_noop {
    ($msgid:literal) => {
        $msgid
    };
}

/// `tr_noop!` for a message with a context. Gives (context, msgid) for `i18n::pgettext`.
#[macro_export]
macro_rules! trc_noop {
    ($context:literal, $msgid:literal) => {
        ($context, $msgid)
    };
}

/// The pref holding a language code from `languages`. Empty or absent follows the system.
pub const LANGUAGE_PREF: &str = "language";

/// One language as build.rs wrote it into the binary.
pub struct RawCatalog {
    pub lang: &'static str,
    pub plural_forms: &'static str,
    /// Sorted by key. A key is the msgid, or msgctxt, U+0004 and msgid.
    pub entries: &'static [(&'static str, &'static [&'static str])],
}

include!(concat!(env!("OUT_DIR"), "/catalogs.rs"));

struct Catalog {
    plural: Plural,
    entries: HashMap<&'static str, &'static [&'static str]>,
}

impl Catalog {
    fn load(raw: &RawCatalog) -> Catalog {
        let plural = Plural::parse(raw.plural_forms).unwrap_or_else(|| {
            if !raw.plural_forms.is_empty() {
                tracing::warn!(lang = raw.lang, rule = raw.plural_forms, "unreadable Plural-Forms, using the English rule");
            }
            Plural::default()
        });
        Catalog { plural, entries: raw.entries.iter().copied().collect() }
    }

    fn get(&self, key: &str) -> Option<&'static str> {
        self.entries.get(key).and_then(|forms| forms.first()).copied()
    }

    fn get_plural(&self, key: &str, n: u64) -> Option<&'static str> {
        let forms = self.entries.get(key)?;
        // A rule pointing past the forms the translator wrote falls back to the last one.
        forms.get(self.plural.index(n)).or(forms.last()).copied()
    }
}

/// None while the app runs in English, which is also the state before `init`.
static ACTIVE: OnceLock<Option<Catalog>> = OnceLock::new();

fn active() -> Option<&'static Catalog> {
    ACTIVE.get().and_then(Option::as_ref)
}

/// Pick the language. Call from main before GTK starts and before any thread exists.
pub fn init(paths: &Paths) {
    let prefs = paths.read_prefs();
    let chosen = prefs.get(LANGUAGE_PREF).and_then(|v| v.as_str()).unwrap_or_default();
    if !chosen.is_empty() {
        // GTK and libadwaita translate their own text (the About window, the
        // shortcuts) from this variable, and glib reads it for the list below.
        // SAFETY: called from main before any other thread exists.
        unsafe { std::env::set_var("LANGUAGE", chosen) };
    }
    let names = glib::language_names();
    let raw = pick(names.iter().map(|name| name.as_str()), CATALOGS);
    match raw {
        Some(raw) => tracing::info!(lang = raw.lang, strings = raw.entries.len(), "translation loaded"),
        None => tracing::debug!(?names, "no translation for the system languages"),
    }
    let _ = ACTIVE.set(raw.map(Catalog::load));
}

/// The first catalog matching the listener's languages, in their order of
/// preference. glib already lists "de_DE.UTF-8" as "de_DE" and "de" too.
/// English ahead of a translated language keeps the app in English.
fn pick<'a>(names: impl Iterator<Item = &'a str>, catalogs: &'static [RawCatalog]) -> Option<&'static RawCatalog> {
    for name in names {
        if let Some(raw) = catalogs.iter().find(|raw| raw.lang == name) {
            return Some(raw);
        }
        if matches!(name, "C" | "POSIX" | "en") {
            return None;
        }
    }
    None
}

/// The languages a listener is able to pick, as (code, name in that language), sorted by name.
/// English is the source text and joins the list once a translation exists. Empty until then.
pub fn languages() -> Vec<(&'static str, String)> {
    if CATALOGS.is_empty() {
        return Vec::new();
    }
    // Translators: the name of your language in your language, for example "Deutsch". Do not translate it as the word "English".
    let (context, msgid) = trc_noop!("language-name", "English");
    let name_key = po::key(Some(context), msgid);
    let mut languages: Vec<(&'static str, String)> = CATALOGS
        .iter()
        .map(|raw| {
            let name = raw.entries.binary_search_by(|(key, _)| (*key).cmp(name_key.as_str())).ok().and_then(|at| raw.entries[at].1.first().copied());
            (raw.lang, name.unwrap_or(raw.lang).to_owned())
        })
        .collect();
    languages.push(("en", msgid.to_owned()));
    languages.sort_by_key(|(_, name)| name.to_lowercase());
    languages
}

/// The translation of a message. `tr!` is the usual way in.
pub fn gettext(msgid: &str) -> String {
    active().and_then(|catalog| catalog.get(msgid)).unwrap_or(msgid).to_owned()
}

/// The translation of a message that needs a context to be told apart. No call site uses `trc!` yet.
#[allow(dead_code)]
pub fn pgettext(context: &str, msgid: &str) -> String {
    active().and_then(|catalog| catalog.get(&po::key(Some(context), msgid))).unwrap_or(msgid).to_owned()
}

/// The translation of a message in the form the count `n` calls for.
pub fn ngettext(msgid: &str, msgid_plural: &str, n: u64) -> String {
    let english = if n == 1 { msgid } else { msgid_plural };
    active().and_then(|catalog| catalog.get_plural(msgid, n)).unwrap_or(english).to_owned()
}

/// Fill `{name}` placeholders. `{{` and `}}` are literal braces, and a name
/// with no argument stays as written.
pub fn format(template: &str, args: &[(&str, &dyn Display)]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(at) = rest.find(['{', '}']) {
        out.push_str(&rest[..at]);
        let brace = rest.as_bytes()[at];
        rest = &rest[at + 1..];
        if rest.as_bytes().first() == Some(&brace) {
            out.push(brace as char);
            rest = &rest[1..];
            continue;
        }
        let value = (brace == b'{').then(|| rest.find('}')).flatten().and_then(|end| {
            let found = args.iter().find(|(name, _)| *name == &rest[..end])?;
            Some((end, found.1))
        });
        match value {
            Some((end, value)) => {
                let _ = write!(out, "{value}");
                rest = &rest[end + 1..];
            }
            None => out.push(brace as char),
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests;
