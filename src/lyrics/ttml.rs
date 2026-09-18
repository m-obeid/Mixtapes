//! TTML lyrics, the format Apple Music, BetterLyrics and BiniLyrics serve.
//!
//! Each `<p>` is a line. Word-level `<span begin end>` children become `parts` for the karaoke sweep. The second line comes from three places, all optional:
//!
//! - `<span ttm:role="x-bg">`: background vocals nested in the line they answer, with word spans of their own.
//! - `ttm:role="x-translation"` and `x-roman` inline spans.
//! - Apple's `<iTunesMetadata>` translation and transliteration blocks, matched to a line by its `itunes:key`.
//!
//! Attributes are looked up by local name, because the namespace URIs differ between the Apple, BetterLyrics and BiniLyrics deployments.

use std::collections::HashMap;

use roxmltree::{Document, Node, ParsingOptions};

use super::model::{Align, LyricLine, LyricPart};

/// Parse a TTML time expression to seconds: bare seconds ("24.111"), M:SS.sss ("1:03.364") or H:MM:SS.sss. None when it is anything else, so the caller degrades to unsynced lines.
pub fn time_to_seconds(value: &str) -> Option<f64> {
    let parts: Vec<&str> = value.split(':').collect();
    let whole = |s: &str| s.trim().parse::<i64>().ok().map(|v| v as f64);
    let real = |s: &str| s.trim().parse::<f64>().ok();
    match parts.as_slice() {
        [seconds] => real(seconds),
        [minutes, seconds] => Some(whole(minutes)? * 60.0 + real(seconds)?),
        [hours, minutes, seconds] => Some(whole(hours)? * 3600.0 + whole(minutes)? * 60.0 + real(seconds)?),
        _ => None,
    }
}

/// What one `<p>`, or one background span, holds.
struct ParsedLine<'a, 'input> {
    parts: Vec<LyricPart>,
    text: String,
    backgrounds: Vec<Node<'a, 'input>>,
    translation: Option<String>,
    romanization: Option<String>,
}

/// An attribute by local name, whatever its namespace.
fn attr<'a>(node: Node<'a, '_>, local: &str) -> Option<&'a str> {
    node.attributes().find(|a| a.name() == local).map(|a| a.value())
}

/// A timing attribute, which TTML never namespaces.
fn time_attr(node: Node, name: &str) -> Option<f64> {
    node.attribute(name).and_then(time_to_seconds)
}

/// All text under a node, whitespace-collapsed.
fn deep_text(node: Node) -> String {
    let raw: String = node.descendants().filter(|n| n.is_text()).filter_map(|n| n.text()).collect();
    collapse(&raw)
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True when the source put whitespace right after a span. Japanese and Chinese TTML has none between syllables, and adding it puts gaps in the lyric.
fn has_space_after(node: Node) -> bool {
    node.tail().and_then(|t| t.chars().next()).is_some_and(char::is_whitespace)
}

/// Walk the children of a `<p>` or of an `x-bg` span, which share a shape.
///
/// Roles are checked before timing: a background or translation span carries `begin` and `end` too and would otherwise read as one more word of the lead vocal. Text between spans is kept whichever branch its neighbour took, so removing a background span does not swallow the space after it.
fn parse_line<'a, 'input>(elem: Node<'a, 'input>) -> ParsedLine<'a, 'input> {
    let mut line = ParsedLine { parts: Vec::new(), text: String::new(), backgrounds: Vec::new(), translation: None, romanization: None };
    let mut chunks = String::new();
    for child in elem.children() {
        if child.is_text() {
            chunks.push_str(child.text().unwrap_or_default());
            continue;
        }
        if !child.is_element() {
            continue;
        }
        let is_span = child.tag_name().name() == "span";
        let role = if is_span { attr(child, "role").unwrap_or_default() } else { "" };
        if role == "x-bg" {
            line.backgrounds.push(child);
        } else if role.starts_with("x-translation") {
            line.translation = Some(deep_text(child));
        } else if role.starts_with("x-roman") || role.starts_with("x-translit") {
            line.romanization = Some(deep_text(child));
        } else if is_span {
            let word = deep_text(child);
            if !word.is_empty() {
                line.parts.push(LyricPart { start: time_attr(child, "begin"), end: time_attr(child, "end"), text: word.clone(), space_after: has_space_after(child) });
                chunks.push_str(&word);
            }
        } else if let Some(text) = child.text() {
            chunks.push_str(text);
        }
    }
    line.text = collapse(&chunks);
    line
}

/// Apple's per-line translations and transliterations from the `<iTunesMetadata>` head, keyed by the `itunes:key` of the `<p>` they belong to.
///
/// Only the first block of each kind is read. A track with several translation languages gets the one listed first, which is the one Apple's own client shows by default.
fn side_texts(root: Node) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut translations = HashMap::new();
    let mut transliterations = HashMap::new();
    for elem in root.descendants().filter(Node::is_element) {
        let target = match elem.tag_name().name() {
            "translation" => &mut translations,
            "transliteration" => &mut transliterations,
            _ => continue,
        };
        if !target.is_empty() {
            continue;
        }
        for text in elem.descendants().filter(|n| n.is_element() && n.tag_name().name() == "text") {
            let value = deep_text(text);
            if let Some(key) = attr(text, "for").filter(|k| !k.is_empty())
                && !value.is_empty()
            {
                target.insert(key.to_owned(), value);
            }
        }
    }
    (translations, transliterations)
}

/// The `xml:id` of the first voice declared in the head.
///
/// Apple tags every line with `ttm:agent`. Lines of the first agent stay on the leading edge in Apple's own client, and everything else, the second singer and the group parts, goes to the opposite one.
fn primary_agent<'a>(root: Node<'a, '_>) -> Option<&'a str> {
    root.descendants().filter(|n| n.is_element() && n.tag_name().name() == "agent").find_map(|n| attr(n, "id").filter(|id| !id.is_empty()))
}

/// Turn a TTML document into normalized lines. Empty when it does not parse.
pub fn ttml_to_lines(ttml: &str) -> Vec<LyricLine> {
    let options = ParsingOptions { allow_dtd: true, ..ParsingOptions::default() };
    let doc = match Document::parse_with_options(ttml, options) {
        Ok(doc) => doc,
        Err(err) => {
            tracing::debug!(%err, "TTML parse failed");
            return Vec::new();
        }
    };
    let root = doc.root_element();
    let (meta_translations, meta_transliterations) = side_texts(root);
    let primary = primary_agent(root);

    let mut lines = Vec::new();
    for elem in root.descendants().filter(|n| n.is_element() && n.tag_name().name() == "p") {
        let parsed = parse_line(elem);
        if parsed.text.is_empty() {
            continue;
        }
        let mut line = LyricLine::new(time_attr(elem, "begin"), parsed.text);
        line.end = time_attr(elem, "end");
        // Parts are only worth keeping when at least one is timed. Otherwise the view falls back to line-level rendering.
        if parsed.parts.iter().any(|p| p.start.is_some()) {
            line.parts = parsed.parts;
        }

        let mut bg_parts = Vec::new();
        let mut bg_texts = Vec::new();
        for bg in parsed.backgrounds {
            let sub = parse_line(bg);
            if sub.text.is_empty() {
                continue;
            }
            if sub.parts.is_empty() {
                // A background group with no word spans still has its own begin and end, so it is kept as one timed chunk.
                bg_parts.push(LyricPart { start: time_attr(bg, "begin"), end: time_attr(bg, "end"), text: sub.text.clone(), space_after: true });
            } else {
                bg_parts.extend(sub.parts);
            }
            bg_texts.push(sub.text);
        }
        if !bg_texts.is_empty() {
            line.bg_text = Some(bg_texts.join(" "));
            if bg_parts.iter().any(|p| p.start.is_some()) {
                line.bg = bg_parts;
            }
        }

        // Duets: lines of any voice but the first sit against the opposite edge.
        if let (Some(primary), Some(agent)) = (primary, attr(elem, "agent").filter(|a| !a.is_empty()))
            && agent != primary
        {
            line.align = Some(Align::End);
        }

        let key = attr(elem, "key");
        let from_meta = |map: &HashMap<String, String>| key.and_then(|k| map.get(k)).cloned();
        line.translation = parsed.translation.filter(|t| !t.is_empty()).or_else(|| from_meta(&meta_translations)).filter(|t| *t != line.text);
        line.romanization = parsed.romanization.filter(|r| !r.is_empty()).or_else(|| from_meta(&meta_transliterations)).filter(|r| *r != line.text);
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata" xmlns:itunes="http://music.apple.com/lyric-ttml-internal" xmlns:xml="http://www.w3.org/XML/1998/namespace">"#;

    fn doc(head: &str, body: &str) -> String {
        format!("{HEAD}<head><metadata>{head}</metadata></head><body><div>{body}</div></body></tt>")
    }

    #[test]
    fn time_expressions() {
        assert_eq!(time_to_seconds("24.111"), Some(24.111));
        assert_eq!(time_to_seconds("1:03.364"), Some(63.364));
        assert_eq!(time_to_seconds("1:02:03.5"), Some(3723.5));
        assert_eq!(time_to_seconds("12"), Some(12.0));
        assert_eq!(time_to_seconds(""), None);
        assert_eq!(time_to_seconds("12.5s"), None);
        assert_eq!(time_to_seconds("a:b"), None);
        assert_eq!(time_to_seconds("1:2:3:4"), None);
    }

    #[test]
    fn word_spans_become_parts_with_their_spacing() {
        let lines = ttml_to_lines(&doc("", r#"<p begin="10.0" end="12.5"><span begin="10.0" end="10.5">Hello</span> <span begin="10.5" end="11.0">wor</span><span begin="11.0" end="12.5">ld</span></p>"#));
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert_eq!((line.start, line.end), (Some(10.0), Some(12.5)));
        assert_eq!(line.text, "Hello world");
        let parts: Vec<(&str, bool)> = line.parts.iter().map(|p| (p.text.as_str(), p.space_after)).collect();
        assert_eq!(parts, [("Hello", true), ("wor", false), ("ld", false)]);
        assert_eq!(line.parts[1].start, Some(10.5));
        assert_eq!(line.parts[2].end, Some(12.5));
    }

    #[test]
    fn japanese_syllables_get_no_spaces() {
        let lines = ttml_to_lines(&doc("", r#"<p begin="1" end="3"><span begin="1" end="2">千本</span><span begin="2" end="3">桜</span></p>"#));
        assert_eq!(lines[0].text, "千本桜");
        assert!(lines[0].parts.iter().all(|p| !p.space_after));
    }

    #[test]
    fn a_line_without_spans_is_line_synced() {
        let lines = ttml_to_lines(&doc("", r#"<p begin="1:05.250" end="1:08">Just  a
            line</p><p begin="2:00"></p><p>no timing</p>"#));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "Just a line");
        assert_eq!(lines[0].start, Some(65.25));
        assert!(lines[0].parts.is_empty());
        assert_eq!(lines[1].start, None);
        assert_eq!(lines[1].end, None);
    }

    #[test]
    fn untimed_spans_do_not_count_as_parts() {
        let lines = ttml_to_lines(&doc("", r#"<p begin="1" end="2"><span>plain</span> <span>words</span></p>"#));
        assert_eq!(lines[0].text, "plain words");
        assert!(lines[0].parts.is_empty());
    }

    #[test]
    fn background_vocals_leave_the_lead_line() {
        let body = r#"<p begin="1" end="5"><span begin="1" end="2">Lead</span> <span begin="2" end="3">vocal</span> <span ttm:role="x-bg" begin="3" end="5"><span begin="3" end="4">(ooh</span> <span begin="4" end="5">ooh)</span></span></p>"#;
        let lines = ttml_to_lines(&doc("", body));
        let line = &lines[0];
        assert_eq!(line.text, "Lead vocal");
        assert_eq!(line.parts.len(), 2);
        assert_eq!(line.bg_text.as_deref(), Some("(ooh ooh)"));
        assert_eq!(line.bg.len(), 2);
        assert_eq!(line.bg[1].start, Some(4.0));
        assert!(line.bg[0].space_after);
    }

    #[test]
    fn a_background_group_without_words_is_one_timed_chunk() {
        let lines = ttml_to_lines(&doc("", r#"<p begin="1" end="5"><span begin="1" end="2">Lead</span><span ttm:role="x-bg" begin="3" end="5">yeah yeah</span></p>"#));
        let line = &lines[0];
        assert_eq!(line.text, "Lead");
        assert_eq!(line.bg_text.as_deref(), Some("yeah yeah"));
        assert_eq!(line.bg, [LyricPart { start: Some(3.0), end: Some(5.0), text: "yeah yeah".into(), space_after: true }]);
    }

    #[test]
    fn inline_translation_and_romanization_spans() {
        let body = r#"<p begin="1" end="2"><span begin="1" end="2">千本桜</span><span ttm:role="x-translation" xml:lang="en">A thousand cherry trees</span><span ttm:role="x-roman">senbonzakura</span></p>"#;
        let line = &ttml_to_lines(&doc("", body))[0];
        assert_eq!(line.text, "千本桜");
        assert_eq!(line.translation.as_deref(), Some("A thousand cherry trees"));
        assert_eq!(line.romanization.as_deref(), Some("senbonzakura"));
    }

    #[test]
    fn itunes_metadata_fills_the_second_line_by_key() {
        let head = r#"<iTunesMetadata xmlns="http://music.apple.com/lyric-ttml-internal">
            <translations><translation xml:lang="es"><text for="L1">Hasta el amanecer</text><text for="L2">same</text></translation>
            <translation xml:lang="fr"><text for="L1">Jusqu'à l'aube</text></translation></translations>
            <transliterations><transliteration xml:lang="ja-Latn"><text for="L1"><span begin="1" end="2">yoake</span> <span begin="2" end="3">made</span></text></transliteration></transliterations>
        </iTunesMetadata>"#;
        let body = r#"<p begin="1" end="3" itunes:key="L1">夜明けまで</p><p begin="3" end="4" itunes:key="L2">same</p><p begin="4" end="5" itunes:key="L3">none</p>"#;
        let lines = ttml_to_lines(&doc(head, body));
        assert_eq!(lines[0].translation.as_deref(), Some("Hasta el amanecer"), "the first block wins");
        assert_eq!(lines[0].romanization.as_deref(), Some("yoake made"));
        assert_eq!(lines[1].translation, None, "a translation equal to the line is dropped");
        assert_eq!(lines[2].translation, None);
    }

    #[test]
    fn the_second_voice_sits_against_the_other_edge() {
        let head = r#"<ttm:agent type="person" xml:id="v1"/><ttm:agent type="person" xml:id="v2"/><ttm:agent type="group" xml:id="v1000"/>"#;
        let body = r#"<p begin="1" end="2" ttm:agent="v1">first</p><p begin="2" end="3" ttm:agent="v2">second</p><p begin="3" end="4" ttm:agent="v1000">both</p><p begin="4" end="5">nobody</p>"#;
        let aligned: Vec<bool> = ttml_to_lines(&doc(head, body)).iter().map(LyricLine::opposite_voice).collect();
        assert_eq!(aligned, [false, true, true, false]);
    }

    #[test]
    fn broken_documents_parse_to_nothing() {
        assert!(ttml_to_lines("").is_empty());
        assert!(ttml_to_lines("<tt><body><p>unclosed").is_empty());
        assert!(ttml_to_lines("not xml at all").is_empty());
    }

    #[test]
    fn a_bare_document_without_namespaces_still_reads() {
        let lines = ttml_to_lines(r#"<tt><body><div><p begin="0:01" end="0:02" agent="v2"><span begin="0:01" end="0:02" role="x-bg">echo</span>word</p></div></body></tt>"#);
        assert_eq!(lines[0].text, "word");
        assert_eq!(lines[0].bg_text.as_deref(), Some("echo"));
        assert_eq!(lines[0].start, Some(1.0));
    }
}
