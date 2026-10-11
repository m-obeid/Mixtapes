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
//! Text YouTube sends reaches the app in English. `youtube` translates the
//! wording it uses for everyone ("Liked Music", "58 songs") where it is shown.

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
    lang: &'static str,
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
        Catalog { lang: raw.lang, plural, entries: raw.entries.iter().copied().collect() }
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
    // MIXTAPES_LANGUAGE is for testing a language without touching the pref.
    let forced = std::env::var("MIXTAPES_LANGUAGE").unwrap_or_default();
    let chosen = if forced.is_empty() { prefs.get(LANGUAGE_PREF).and_then(|v| v.as_str()).unwrap_or_default() } else { forced.as_str() };
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
///
/// The pseudolocale is for testing. A debug build lists it, and `MIXTAPES_LANGUAGE=en_XA` picks it in any build.
pub fn languages() -> Vec<(&'static str, String)> {
    let listed: Vec<&RawCatalog> = CATALOGS.iter().filter(|raw| raw.lang != po::PSEUDO || cfg!(debug_assertions)).collect();
    if listed.iter().all(|raw| raw.lang == po::PSEUDO) {
        return Vec::new();
    }
    // Translators: the name of your language in your language, for example "Deutsch". Do not translate it as the word "English".
    let (context, msgid) = trc_noop!("language-name", "English");
    let name_key = po::key(Some(context), msgid);
    let mut languages: Vec<(&'static str, String)> = listed
        .into_iter()
        .map(|raw| {
            let name = raw.entries.binary_search_by(|(key, _)| (*key).cmp(name_key.as_str())).ok().and_then(|at| raw.entries[at].1.first().copied());
            (raw.lang, name.unwrap_or(raw.lang).to_owned())
        })
        .collect();
    languages.push(("en", msgid.to_owned()));
    languages.sort_by_key(|(_, name)| name.to_lowercase());
    languages
}

/// The translation of a text that came from elsewhere, when the catalog has one.
pub fn translated(text: &str) -> Option<String> {
    active().and_then(|catalog| catalog.get(text)).map(str::to_owned)
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

/// The active language as YouTube names it ("de", "pt-BR"), for requests whose text is only shown.
/// None while the app runs in English.
pub fn youtube_language() -> Option<String> {
    // The pseudolocale is no language YouTube knows.
    active().filter(|catalog| catalog.lang != po::PSEUDO).map(|catalog| catalog.lang.replace('_', "-"))
}

/// Text YouTube words the same way for everyone, which the app reads in English and shows translated.
/// Listed here so po/update.sh puts it in the template. `youtube` looks it up.
#[allow(dead_code)]
const YOUTUBE_TEXT: &[&str] = &[
    // The lists every library has.
    tr_noop!("Liked Music"),
    tr_noop!("Sounds from Shorts"),
    tr_noop!("Episodes for Later"),
    tr_noop!("Auto playlist"),
    tr_noop!("Your queued episodes"),
    // Home shelves.
    tr_noop!("From your library"),
    tr_noop!("New releases"),
    tr_noop!("Forgotten favorites"),
    tr_noop!("Albums for you"),
    tr_noop!("Your daily discover"),
    tr_noop!("Fresh finds, old favorites"),
    tr_noop!("Mixed for you"),
    tr_noop!("Featured playlists for you"),
    tr_noop!("From the community"),
    tr_noop!("Long listens"),
    tr_noop!("Recommended albums"),
    tr_noop!("Recommended music videos"),
    tr_noop!("Music videos for you"),
    tr_noop!("Covers and remixes"),
    tr_noop!("Keep listening"),
    tr_noop!("Trending community playlists"),
    tr_noop!("Live performances"),
    tr_noop!("Trending songs for you"),
    tr_noop!("Listen together"),
    // Shelves of a mood or genre page.
    tr_noop!("Community playlists"),
    tr_noop!("Featured playlists"),
    tr_noop!("Music videos"),
    // The small line over a shelf title. YouTube sends these in capitals.
    tr_noop!("Station"),
    // A mood YouTube translates itself. An entry here replaces its wording, for a language where that one is off.
    tr_noop!("Feel good"),
];

/// Translate a line YouTube sent in English: its parts between bullets, each a
/// known text ("Auto playlist") or a count ("58 songs"). A part with no
/// translation, a name for one, stays as it came.
pub fn youtube(text: &str) -> String {
    if active().is_none() {
        return text.to_owned();
    }
    text.split(" \u{2022} ").map(|part| youtube_count(part.trim()).unwrap_or_else(|| gettext(part))).collect::<Vec<_>>().join(" \u{2022} ")
}

/// "58 songs" or "1.2M subscribers" in the active language. The count stays as YouTube wrote it.
fn youtube_count(text: &str) -> Option<String> {
    let (count, unit) = text.split_once(' ')?;
    if !count.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    // A short count like "12K" is many. The plural form follows the number where it is a plain one.
    let n = count.replace([',', '.'], "").parse::<u64>().unwrap_or(1000);
    Some(match unit.to_ascii_lowercase().as_str() {
        // Translators: {count} is a number as YouTube words it, like "1,204" or "12K". The same goes for the counts below.
        "song" | "songs" => trn!("{count} song", "{count} songs", n, count),
        "track" | "tracks" => trn!("{count} track", "{count} tracks", n, count),
        "episode" | "episodes" => trn!("{count} episode", "{count} episodes", n, count),
        "video" | "videos" => trn!("{count} video", "{count} videos", n, count),
        "view" | "views" => trn!("{count} view", "{count} views", n, count),
        "play" | "plays" => trn!("{count} play", "{count} plays", n, count),
        "subscriber" | "subscribers" => trn!("{count} subscriber", "{count} subscribers", n, count),
        "monthly listeners" => trn!("{count} monthly listener", "{count} monthly listeners", n, count),
        _ => return None,
    })
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
