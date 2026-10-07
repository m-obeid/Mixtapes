//! Reads gettext PO files. build.rs includes this file too, so it uses std only.
//!
//! `compile` is what turns a translator's file into the table the binary
//! carries: fuzzy and unfinished entries are dropped, and so is any entry whose
//! placeholders do not match the English text, since a typo there would put
//! `{nmae}` on screen. A dropped entry shows in English.

#![allow(dead_code)]

use std::collections::BTreeSet;

/// Joins a msgctxt to its msgid in a lookup key, as in a compiled .mo file.
pub const CONTEXT_SEPARATOR: char = '\u{4}';

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Entry {
    pub context: Option<String>,
    pub id: String,
    pub id_plural: Option<String>,
    /// One string, or one per plural form.
    pub strings: Vec<String>,
    pub fuzzy: bool,
}

/// What a PO file contributes to the binary.
#[derive(Debug, Default, PartialEq)]
pub struct Compiled {
    /// The Plural-Forms header value. Empty when the file has none.
    pub plural_forms: String,
    /// Sorted by key, so the generated table can be searched.
    pub entries: Vec<(String, Vec<String>)>,
    /// One line per entry left out, for the build log.
    pub skipped: Vec<String>,
}

pub fn key(context: Option<&str>, id: &str) -> String {
    match context {
        Some(context) => format!("{context}{CONTEXT_SEPARATOR}{id}"),
        None => id.to_owned(),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    None,
    Context,
    Id,
    IdPlural,
    Str(usize),
}

pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    let mut entry = Entry::default();
    let mut field = Field::None;
    // True once the entry being read has a msgid, which is what makes it an entry.
    let mut started = false;
    // Flags sit above the entry they belong to, so they wait for its msgid.
    let mut fuzzy = false;

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let fail = |what: &str| format!("line {}: {what}", index + 1);
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix('#') {
            if let Some(flags) = comment.strip_prefix(',') {
                // A flag line after a msgstr opens the next entry.
                if matches!(field, Field::Str(_)) {
                    flush(&mut entries, &mut entry, &mut started);
                    field = Field::None;
                }
                fuzzy |= flags.split(',').any(|flag| flag.trim() == "fuzzy");
            }
            continue;
        }
        let (keyword, rest) = match line.find(|c: char| c == '"' || c.is_whitespace()) {
            Some(at) if !line.starts_with('"') => (&line[..at], line[at..].trim()),
            _ => ("", line),
        };
        let value = unquote(rest).map_err(|what| fail(&what))?;
        let next = match keyword {
            "" => {
                match field {
                    Field::None => return Err(fail("a string with no keyword before it")),
                    Field::Context => entry.context.get_or_insert_with(String::new).push_str(&value),
                    Field::Id => entry.id.push_str(&value),
                    Field::IdPlural => entry.id_plural.get_or_insert_with(String::new).push_str(&value),
                    Field::Str(n) => entry.strings[n].push_str(&value),
                }
                continue;
            }
            "msgctxt" => Field::Context,
            "msgid" => Field::Id,
            "msgid_plural" => Field::IdPlural,
            "msgstr" => Field::Str(0),
            other => match other.strip_prefix("msgstr[").and_then(|n| n.strip_suffix(']')).and_then(|n| n.parse::<usize>().ok()) {
                Some(n) => Field::Str(n),
                None => return Err(fail(&format!("unknown keyword {other}"))),
            },
        };
        // msgctxt or msgid after a msgstr starts the next entry.
        if matches!(next, Field::Context | Field::Id) && matches!(field, Field::Str(_)) {
            flush(&mut entries, &mut entry, &mut started);
        }
        match next {
            Field::Context => entry.context = Some(value),
            Field::Id => {
                entry.id = value;
                entry.fuzzy = std::mem::take(&mut fuzzy);
                started = true;
            }
            Field::IdPlural => entry.id_plural = Some(value),
            Field::Str(n) => {
                if !started {
                    return Err(fail("msgstr without a msgid"));
                }
                if n != entry.strings.len() {
                    return Err(fail("plural forms out of order"));
                }
                entry.strings.push(value);
            }
            Field::None => {}
        }
        field = next;
    }
    flush(&mut entries, &mut entry, &mut started);
    Ok(entries)
}

fn flush(entries: &mut Vec<Entry>, entry: &mut Entry, started: &mut bool) {
    let done = std::mem::take(entry);
    if std::mem::take(started) {
        entries.push(done);
    }
}

/// The text between the quotes of one PO string, escapes resolved.
fn unquote(text: &str) -> Result<String, String> {
    let inner = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')).filter(|_| text.len() >= 2).ok_or_else(|| format!("expected a quoted string, found {text}"))?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => return Err("a string ends in a lone backslash".to_owned()),
        }
    }
    Ok(out)
}

/// The names inside `{}` in a message. `{{` and `}}` are literal braces.
pub fn placeholders(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next();
            continue;
        }
        let name: String = chars.by_ref().take_while(|c| *c != '}').collect();
        names.insert(name);
    }
    names
}

pub fn compile(entries: &[Entry]) -> Compiled {
    let mut compiled = Compiled::default();
    for entry in entries {
        if entry.id.is_empty() && entry.context.is_none() {
            // The header: "Name: value" lines in the msgstr.
            let header = entry.strings.first().map(String::as_str).unwrap_or_default();
            compiled.plural_forms = header.lines().find_map(|line| line.strip_prefix("Plural-Forms:")).unwrap_or_default().trim().to_owned();
            continue;
        }
        if entry.fuzzy || entry.strings.is_empty() || entry.strings.iter().any(String::is_empty) {
            continue;
        }
        let mut allowed = placeholders(&entry.id);
        if let Some(plural) = &entry.id_plural {
            allowed.extend(placeholders(plural));
        }
        if let Some(bad) = entry.strings.iter().flat_map(|s| placeholders(s)).find(|name| !allowed.contains(name)) {
            compiled.skipped.push(format!("\"{}\": unknown placeholder {{{bad}}}", entry.id));
            continue;
        }
        compiled.entries.push((key(entry.context.as_deref(), &entry.id), entry.strings.clone()));
    }
    compiled.entries.sort();
    compiled.entries.dedup_by(|a, b| a.0 == b.0);
    compiled
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# A translator comment.
msgid ""
msgstr ""
"Language: de\n"
"Plural-Forms: nplurals=2; plural=(n != 1);\n"

#: src/ui/window.rs:10
msgid "Home"
msgstr "Startseite"

#. Translators: a verb
#: src/ui/player_bar.rs:3
msgctxt "verb"
msgid "Play"
msgstr "Abspielen"

#, rust-format
msgid "{n} song"
msgid_plural "{n} songs"
msgstr[0] "{n} Titel"
msgstr[1] "{n} Titel"

msgid ""
"Two "
"lines\n"
msgstr ""
"Zwei \"Zeilen\"\n"

#, fuzzy
msgid "Guess"
msgstr "Geraten"

msgid "Untranslated"
msgstr ""

#, rust-format
msgid "Added to {name}"
msgstr "Zu {nmae} hinzugefügt"

#~ msgid "Obsolete"
#~ msgstr "Veraltet"
"#;

    #[test]
    fn parses_every_entry_shape() {
        let entries = parse(SAMPLE).unwrap();
        assert_eq!(entries.len(), 8);
        assert_eq!(entries[1].id, "Home");
        assert_eq!(entries[2].context.as_deref(), Some("verb"));
        assert_eq!(entries[3].id_plural.as_deref(), Some("{n} songs"));
        assert_eq!(entries[3].strings.len(), 2);
        assert_eq!(entries[4].id, "Two lines\n");
        assert_eq!(entries[4].strings[0], "Zwei \"Zeilen\"\n");
        assert!(entries[5].fuzzy);
        assert!(!entries[6].fuzzy);
    }

    #[test]
    fn compile_keeps_only_usable_entries() {
        let compiled = compile(&parse(SAMPLE).unwrap());
        assert_eq!(compiled.plural_forms, "nplurals=2; plural=(n != 1);");
        let keys: Vec<&str> = compiled.entries.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["Home", "Two lines\n", "verb\u{4}Play", "{n} song"]);
        assert_eq!(compiled.skipped.len(), 1);
        assert!(compiled.skipped[0].contains("{nmae}"));
    }

    #[test]
    fn rejects_broken_files() {
        assert!(parse("msgstr \"orphan\"").is_err());
        assert!(parse("msgid \"open").is_err());
        assert!(parse("msgfoo \"x\"").is_err());
    }

    #[test]
    fn placeholders_skip_escaped_braces() {
        assert_eq!(placeholders("{a} and {{b}} and {c}").into_iter().collect::<Vec<_>>(), ["a", "c"]);
    }
}
