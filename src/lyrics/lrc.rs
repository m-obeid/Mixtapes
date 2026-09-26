//! LRC text (`[mm:ss.xx] words`) and the clean-up the LRC sources need: production credits NetEase prepends, store watermarks Apple prepends, and the parallel translation and romanization tracks NetEase ships.

use std::sync::LazyLock;

use regex::Regex;

use super::model::LyricLine;

/// Store watermarks Apple prepends to some tracks. Matched only against the opening lines, so a lyric that mentions buying something mid-song is untouched.
const LYRIC_WATERMARKS: [&str; 4] = ["purchase your tracks", "lyrics licensed", "lyrics provided by", "unauthorized reproduction"];

/// How many opening lines may be watermarks.
const WATERMARK_LIMIT: usize = 2;

const CREDIT_KEYWORDS: [&str; 19] =
    ["作词", "作曲", "编曲", "編曲", "制作人", "製作人", "出品人", "混音", "母带", "監製", "监制", "演唱", "和声", "录音", "lyricist", "composer", "arranger", "producer", "mixed by"];

/// How far a secondary line's stamp may sit from the main line's, in seconds.
const SECONDARY_TOLERANCE: f64 = 0.45;

static TIMESTAMP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[(\d+):(\d{1,2})(?:[.:](\d{1,3}))?\]").unwrap());

/// Which field a parallel LRC track fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Secondary {
    Romanization,
    Translation,
}

/// Parse LRC into lines sorted by start time.
///
/// A line may carry several stamps, one per repeat. Instrumental gaps arrive as stamps with no text and are kept, so the active line still advances through them. Metadata tags such as `[ar:...]` carry no numeric stamp and are skipped.
pub fn parse_lrc_text(lrc: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    for raw_line in lrc.lines() {
        let mut rest = raw_line;
        let mut starts = Vec::new();
        while let Some(stamp) = TIMESTAMP_RE.captures(rest) {
            let minutes: f64 = stamp[1].parse().unwrap_or(0.0);
            let seconds: f64 = stamp[2].parse().unwrap_or(0.0);
            let fraction: f64 = stamp.get(3).and_then(|f| format!("0.{}", f.as_str()).parse().ok()).unwrap_or(0.0);
            starts.push(minutes * 60.0 + seconds + fraction);
            rest = &rest[stamp[0].len()..];
        }
        let text = rest.trim();
        out.extend(starts.into_iter().map(|start| LyricLine::new(Some(start), text)));
    }
    out.sort_by(|a, b| a.start.unwrap_or(0.0).total_cmp(&b.start.unwrap_or(0.0)));
    out
}

/// Unsynced lyrics: one line per non-blank row of text.
pub fn plain_lines(text: &str) -> Vec<LyricLine> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| LyricLine::new(None, l)).collect()
}

/// Merge a parallel LRC track into `lines`, answering how many lines got a value.
///
/// NetEase stamps a track's translation and romanization against the same clock as the main lyric, but with their own line counts: credits and empty interludes are often in one and not the other. Index position is useless, so the match is the nearest secondary line within the tolerance.
pub fn attach_secondary_lrc(lines: &mut [LyricLine], lrc: &str, key: Secondary) -> usize {
    let secondary: Vec<LyricLine> = parse_lrc_text(lrc).into_iter().filter(|l| !l.text.trim().is_empty()).collect();
    if lines.is_empty() || secondary.is_empty() {
        return 0;
    }
    let starts: Vec<f64> = secondary.iter().filter_map(|l| l.start).collect();
    let mut matched = 0;
    for line in lines.iter_mut() {
        let Some(start) = line.start else { continue };
        let at = starts.partition_point(|s| *s < start);
        let mut best: Option<&LyricLine> = None;
        let mut best_delta = SECONDARY_TOLERANCE;
        for j in [at.checked_sub(1), Some(at), Some(at + 1)].into_iter().flatten().filter(|j| *j < secondary.len()) {
            let delta = (starts[j] - start).abs();
            if delta <= best_delta {
                best = Some(&secondary[j]);
                best_delta = delta;
            }
        }
        let Some(best) = best else { continue };
        let text = best.text.trim();
        // A "translation" identical to the original is padding: NetEase does this for lines already in the target language.
        if text.is_empty() || text == line.text.trim() {
            continue;
        }
        match key {
            Secondary::Romanization => line.romanization = Some(text.to_owned()),
            Secondary::Translation => line.translation = Some(text.to_owned()),
        }
        matched += 1;
    }
    matched
}

/// Drop the production credits NetEase and a few others prepend, such as `Lyricist: X` and its Chinese forms. Only contiguously from the start, so a credit-shaped lyric in the middle of the song stays put.
pub fn strip_leading_credits(lines: Vec<LyricLine>) -> Vec<LyricLine> {
    let is_credit = |line: &LyricLine| {
        let text = line.text.trim();
        let lowered = text.to_lowercase();
        (text.contains(':') || text.contains('：')) && CREDIT_KEYWORDS.iter().any(|kw| lowered.contains(kw))
    };
    let first_lyric = lines.iter().position(|l| !is_credit(l)).unwrap_or(lines.len());
    lines.into_iter().skip(first_lyric).collect()
}

/// Drop a store watermark from the top of a lyric. Apple serves some tracks with "(Purchase your tracks today)" first, stamped like any other line.
pub fn strip_leading_watermarks(mut lines: Vec<LyricLine>) -> Vec<LyricLine> {
    let is_watermark = |line: &LyricLine| {
        let lowered = line.text.trim().to_lowercase();
        let text = lowered.trim_matches(|c| matches!(c, '(' | ')' | '[' | ']'));
        LYRIC_WATERMARKS.iter().any(|mark| text.contains(mark))
    };
    let marked = lines.iter().take(WATERMARK_LIMIT).take_while(|l| is_watermark(l)).count();
    lines.drain(..marked);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[LyricLine]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn lrc_lines_are_parsed_and_sorted() {
        let lines = parse_lrc_text("[ar:Someone]\n[ti:Song]\n[00:12.50] second\n[00:01.5]first\r\n[01:02:250]third\n[00:20]\nno stamp here\n");
        assert_eq!(texts(&lines), ["first", "second", "", "third"]);
        assert_eq!(lines[0].start, Some(1.5));
        assert_eq!(lines[1].start, Some(12.5));
        assert_eq!(lines[2].start, Some(20.0));
        assert_eq!(lines[3].start, Some(62.25));
    }

    #[test]
    fn a_repeated_line_lands_at_every_stamp() {
        let lines = parse_lrc_text("[00:40.00][00:10.00] chorus\n[00:20.00] verse");
        assert_eq!(texts(&lines), ["chorus", "verse", "chorus"]);
        assert_eq!(lines[2].start, Some(40.0));
    }

    #[test]
    fn nothing_in_nothing_out() {
        assert!(parse_lrc_text("").is_empty());
        assert!(parse_lrc_text("just words").is_empty());
    }

    #[test]
    fn plain_text_drops_blank_rows() {
        assert_eq!(texts(&plain_lines("one\n\n  two  \r\n\n")), ["one", "two"]);
        assert!(plain_lines("one").iter().all(|l| l.start.is_none()));
    }

    #[test]
    fn a_secondary_track_matches_by_time_not_by_index() {
        let mut lines = parse_lrc_text("[00:10.00] 千本桜\n[00:20.00] 夜ニ紛レ\n[00:30.00] hello\n[00:40.00] 君ノ声");
        // One credit line more than the main track, stamps rounded differently, one line missing.
        let roma = "[00:00.00] credit\n[00:10.30] senbonzakura\n[00:19.70] yoru ni magire\n[00:30.00] hello\n[00:41.00] too far";
        assert_eq!(attach_secondary_lrc(&mut lines, roma, Secondary::Romanization), 2);
        assert_eq!(lines[0].romanization.as_deref(), Some("senbonzakura"));
        assert_eq!(lines[1].romanization.as_deref(), Some("yoru ni magire"));
        assert_eq!(lines[2].romanization, None, "identical text is padding");
        assert_eq!(lines[3].romanization, None, "outside the tolerance");

        assert_eq!(attach_secondary_lrc(&mut lines, "[00:10.00] a thousand cherry trees", Secondary::Translation), 1);
        assert_eq!(lines[0].translation.as_deref(), Some("a thousand cherry trees"));
        assert_eq!(attach_secondary_lrc(&mut lines, "", Secondary::Translation), 0);
        assert_eq!(attach_secondary_lrc(&mut [], roma, Secondary::Translation), 0);
    }

    #[test]
    fn the_nearest_secondary_line_wins() {
        let mut lines = parse_lrc_text("[00:10.00] a");
        attach_secondary_lrc(&mut lines, "[00:09.70] far\n[00:10.10] near", Secondary::Translation);
        assert_eq!(lines[0].translation.as_deref(), Some("near"));
    }

    #[test]
    fn credits_go_only_from_the_top() {
        let lines = parse_lrc_text("[00:00.00] 作词 : X\n[00:01.00] Composer: Y\n[00:02.00] first line\n[00:03.00] Producer: not a credit here");
        assert_eq!(texts(&strip_leading_credits(lines)), ["first line", "Producer: not a credit here"]);
        assert!(strip_leading_credits(parse_lrc_text("[00:00.00] 作曲：X")).is_empty());
    }

    #[test]
    fn watermarks_go_only_from_the_top_two() {
        let lines = vec![LyricLine::new(Some(0.0), "(Purchase your tracks today)"), LyricLine::new(Some(1.0), "[Lyrics licensed by X]"), LyricLine::new(Some(2.0), "lyrics provided by nobody"), LyricLine::new(Some(3.0), "real")];
        assert_eq!(texts(&strip_leading_watermarks(lines)), ["lyrics provided by nobody", "real"]);
        let clean = vec![LyricLine::new(Some(0.0), "real"), LyricLine::new(Some(1.0), "purchase your tracks")];
        assert_eq!(strip_leading_watermarks(clean).len(), 2);
    }
}
