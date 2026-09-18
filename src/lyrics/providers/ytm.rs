//! YouTube Music's own lyrics, port of ytmusicapi's `get_lyrics` as `_fetch_lyrics_ytm` called it.
//!
//! The watch panel names the lyrics page of a video. The web client only gets plain text for it. The Android client gets line timestamps, so that is asked first, the way `get_lyrics(timestamps=True)` did under `as_mobile`.

use std::time::Duration;

use serde_json::{Value, json};

use crate::lyrics::lrc::plain_lines;
use crate::lyrics::model::{LyricLine, LyricsResult};
use crate::net::browse::{Browse, Response};
use crate::net::ytmusic::NetError;

const SOURCE: &str = "YouTube Music";
const API: &str = "https://music.youtube.com/youtubei/v1/";
const ORIGIN: &str = "https://music.youtube.com";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:88.0) Gecko/20100101 Firefox/88.0";
/// The client ytmusicapi's as_mobile() switches to.
const MOBILE_CLIENT: &str = "ANDROID_MUSIC";
const MOBILE_VERSION: &str = "7.21.50";
const TIMEOUT: Duration = Duration::from_secs(8);

const WATCH_TABS: &str = "/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer/tabs";
const TIMED_LYRICS: &str = "/contents/elementRenderer/newElement/type/componentType/model/timedLyricsModel/lyricsData";
const DESCRIPTION_SHELF: &str = "/contents/sectionListRenderer/contents/0/musicDescriptionShelfRenderer";

/// InnerTube as the Android client. The crate client writes its own WEB_REMIX context over whatever the body carries, so this posts by hand.
///
/// The request is anonymous on purpose. Lyrics are the same for every account, and InnerTube answers 400 when a browser session's cookie and SAPISIDHASH arrive with the Android client.
pub struct MobileClient {
    http: reqwest::Client,
}

impl MobileClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

impl Browse for MobileClient {
    fn post<'a>(&'a self, endpoint: &'a str, mut body: Value) -> Response<'a> {
        Box::pin(async move {
            body["context"] = json!({"client": {"clientName": MOBILE_CLIENT, "clientVersion": MOBILE_VERSION, "hl": "en"}, "user": {}});
            // SOCS=CAI is the consent cookie the crate client adds to every request.
            let request = self.http.post(format!("{API}{endpoint}?alt=json")).timeout(TIMEOUT).header("User-Agent", USER_AGENT).header("Accept", "*/*").header("Origin", ORIGIN).header("Cookie", "SOCS=CAI").json(&body);
            let response = request.send().await?;
            let status = response.status();
            if !status.is_success() {
                return Err(NetError::Http { status: status.as_u16(), message: "the Android client was refused".into() });
            }
            Ok(response.json::<Value>().await?)
        })
    }
}

/// The browse id of a video's lyrics page, from its watch panel. None when the Lyrics tab is greyed out.
pub fn lyrics_browse_id(watch: &Value) -> Option<String> {
    let tab = watch.pointer(WATCH_TABS)?.get(1)?.get("tabRenderer")?;
    if tab.get("unselectable").is_some() {
        return None;
    }
    tab.pointer("/endpoint/browseEndpoint/browseId").and_then(Value::as_str).map(str::to_owned)
}

/// A lyrics page as a result: timed lines from the Android shape, plain text from the web one.
///
/// The source is the credit YouTube shows ("Source: Musixmatch") with its prefix removed, as the Python app labelled it.
pub fn parse_lyrics(response: &Value) -> Option<LyricsResult> {
    if let Some(data) = response.pointer(TIMED_LYRICS) {
        let rows = data.get("timedLyricsData")?.as_array()?;
        let lines: Vec<LyricLine> = rows
            .iter()
            .filter_map(|row| {
                let text = row.get("lyricLine").and_then(Value::as_str).filter(|t| !t.is_empty())?;
                Some(LyricLine::new(milliseconds(row.pointer("/cueRange/startTimeMilliseconds")), text.trim()))
            })
            .collect();
        if lines.is_empty() {
            return None;
        }
        // Reported synced as a whole, like the Python app did for this shape.
        return Some(LyricsResult { lines, synced: true, source: credit(data.get("sourceMessage")), user_choice: false });
    }
    let shelf = response.pointer(DESCRIPTION_SHELF)?;
    let lines = plain_lines(shelf.pointer("/description/runs/0/text").and_then(Value::as_str)?);
    if lines.is_empty() {
        return None;
    }
    Some(LyricsResult { lines, synced: false, source: credit(shelf.pointer("/footer/runs/0/text")), user_choice: false })
}

fn credit(value: Option<&Value>) -> String {
    match value.and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(text) => text.replace("Source: ", ""),
        None => SOURCE.to_owned(),
    }
}

/// InnerTube sends these stamps as strings.
fn milliseconds(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .map(|ms| ms / 1000.0)
}

/// The body get_watch_playlist(videoId=..., limit=1) posts to `next`.
fn watch_body(video_id: &str) -> Value {
    json!({
        "enablePersistentPlaylistPanel": true,
        "isAudioOnly": true,
        "tunerSettingValue": "AUTOMIX_SETTING_NORMAL",
        "videoId": video_id,
        "playlistId": format!("RDAMVM{video_id}"),
        "watchEndpointMusicSupportedConfigs": {"watchEndpointMusicConfig": {"hasPersistentPlaylistPanel": true, "musicVideoType": "MUSIC_VIDEO_TYPE_ATV"}},
    })
}

pub async fn fetch(web: &dyn Browse, mobile: &dyn Browse, video_id: &str) -> Option<LyricsResult> {
    if video_id.is_empty() {
        return None;
    }
    let watch = match web.post("next", watch_body(video_id)).await {
        Ok(watch) => watch,
        Err(err) => {
            tracing::debug!(%err, "watch panel failed");
            return None;
        }
    };
    let browse_id = lyrics_browse_id(&watch)?;
    let body = json!({ "browseId": browse_id });
    let page = match mobile.post("browse", body.clone()).await {
        Ok(page) => page,
        Err(err) => {
            // Timed lyrics are a bonus. The web client still has the plain text.
            tracing::debug!(%err, "timed lyrics request failed, asking for plain text");
            web.post("browse", body).await.inspect_err(|err| tracing::debug!(%err, "lyrics page failed")).ok()?
        }
    };
    parse_lyrics(&page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Answers each endpoint from a script and records what was asked.
    struct Scripted {
        next: Option<Value>,
        browse: Option<Value>,
        asked: Mutex<Vec<(String, Value)>>,
    }

    impl Scripted {
        fn new(next: Option<Value>, browse: Option<Value>) -> Self {
            Self { next, browse, asked: Mutex::new(Vec::new()) }
        }
    }

    impl Browse for Scripted {
        fn post<'a>(&'a self, endpoint: &'a str, body: Value) -> Response<'a> {
            self.asked.lock().unwrap().push((endpoint.to_owned(), body));
            let answer = if endpoint == "next" { self.next.clone() } else { self.browse.clone() };
            Box::pin(async move { answer.ok_or_else(|| NetError::Message("scripted failure".into())) })
        }
    }

    fn watch(tab: Value) -> Value {
        json!({"contents": {"singleColumnMusicWatchNextResultsRenderer": {"tabbedRenderer": {"watchNextTabbedResultsRenderer": {"tabs": [{"tabRenderer": {}}, {"tabRenderer": tab}]}}}}})
    }

    fn lyrics_tab() -> Value {
        json!({"endpoint": {"browseEndpoint": {"browseId": "MPLYt_abc"}}})
    }

    fn timed_page() -> Value {
        json!({"contents": {"elementRenderer": {"newElement": {"type": {"componentType": {"model": {"timedLyricsModel": {"lyricsData": {
            "sourceMessage": "Source: Musixmatch",
            "timedLyricsData": [
                {"lyricLine": "♪", "cueRange": {"startTimeMilliseconds": "0", "endTimeMilliseconds": "9200", "metadata": {"id": "1"}}},
                {"lyricLine": " Is this the real life? ", "cueRange": {"startTimeMilliseconds": "9200", "endTimeMilliseconds": "12000", "metadata": {"id": "2"}}},
                {"lyricLine": "", "cueRange": {"startTimeMilliseconds": "12000"}}
            ]
        }}}}}}}}})
    }

    fn plain_page() -> Value {
        json!({"contents": {"sectionListRenderer": {"contents": [{"musicDescriptionShelfRenderer": {"description": {"runs": [{"text": "Is this the real life?\r\n\r\nIs this just fantasy?"}]}, "footer": {"runs": [{"text": "Source: LyricFind"}]}}}]}}})
    }

    #[test]
    fn the_lyrics_tab_names_the_page_unless_it_is_greyed_out() {
        assert_eq!(lyrics_browse_id(&watch(lyrics_tab())).as_deref(), Some("MPLYt_abc"));
        assert_eq!(lyrics_browse_id(&watch(json!({"unselectable": true}))), None);
        assert_eq!(lyrics_browse_id(&json!({})), None);
    }

    #[test]
    fn timed_lyrics_are_line_synced_and_credited() {
        let result = parse_lyrics(&timed_page()).unwrap();
        assert_eq!(result.source, "Musixmatch");
        assert!(result.synced);
        assert_eq!(result.rank(), 2);
        assert_eq!(result.lines.len(), 2);
        assert_eq!(result.lines[1].text, "Is this the real life?");
        assert_eq!(result.lines[1].start, Some(9.2));
    }

    #[test]
    fn plain_lyrics_are_split_into_lines() {
        let result = parse_lyrics(&plain_page()).unwrap();
        assert_eq!(result.source, "LyricFind");
        assert!(!result.synced);
        assert_eq!(result.lines.len(), 2);
        assert!(parse_lyrics(&json!({"contents": {}})).is_none());
    }

    #[test]
    fn a_timed_model_without_rows_is_nothing() {
        let empty = json!({"contents": {"elementRenderer": {"newElement": {"type": {"componentType": {"model": {"timedLyricsModel": {"lyricsData": {"sourceMessage": "x"}}}}}}}}});
        assert!(parse_lyrics(&empty).is_none());
    }

    #[test]
    fn a_missing_credit_falls_back_to_the_provider_name() {
        let mut page = plain_page();
        page["contents"]["sectionListRenderer"]["contents"][0]["musicDescriptionShelfRenderer"]["footer"] = Value::Null;
        assert_eq!(parse_lyrics(&page).unwrap().source, "YouTube Music");
    }

    #[tokio::test]
    async fn the_android_client_is_asked_for_the_page_the_watch_panel_names() {
        let web = Scripted::new(Some(watch(lyrics_tab())), Some(plain_page()));
        let mobile = Scripted::new(None, Some(timed_page()));
        let result = fetch(&web, &mobile, "vid123").await.unwrap();
        assert!(result.synced);
        let asked = web.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].1["videoId"], "vid123");
        assert_eq!(asked[0].1["playlistId"], "RDAMVMvid123");
        assert_eq!(mobile.asked.lock().unwrap()[0].1["browseId"], "MPLYt_abc");
    }

    #[tokio::test]
    async fn the_web_client_is_the_fallback_when_the_android_one_fails() {
        let web = Scripted::new(Some(watch(lyrics_tab())), Some(plain_page()));
        let mobile = Scripted::new(None, None);
        let result = fetch(&web, &mobile, "vid123").await.unwrap();
        assert!(!result.synced);
        assert_eq!(result.source, "LyricFind");
    }

    #[tokio::test]
    async fn no_lyrics_tab_means_no_request_for_lyrics() {
        let web = Scripted::new(Some(watch(json!({"unselectable": true}))), None);
        let mobile = Scripted::new(None, Some(timed_page()));
        assert!(fetch(&web, &mobile, "vid123").await.is_none());
        assert!(mobile.asked.lock().unwrap().is_empty());
        assert!(fetch(&web, &mobile, "").await.is_none());
        let broken = Scripted::new(None, None);
        assert!(fetch(&broken, &mobile, "vid123").await.is_none());
    }
}
