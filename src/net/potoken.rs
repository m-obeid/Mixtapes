//! PO tokens for the `web_music` client.
//!
//! YouTube now hands that client's audio formats over only with a GVS PO
//! token bound to the video id. Uploaded songs are served to no other client,
//! so without a token yt-dlp finds no formats at all and calls the track
//! unavailable. `rustypipe-botguard` mints one in a few milliseconds once it
//! has a snapshot, and a token stays good for a couple of hours, so they are
//! kept until they expire.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::process::Command;

use super::stream::find_executable;

/// Dropped this long before the stated expiry, so a token never goes stale
/// between being handed out and being used.
const EARLY: Duration = Duration::from_secs(300);

struct Token {
    value: String,
    expires: SystemTime,
}

pub struct PoTokens {
    binary: Option<PathBuf>,
    minted: Mutex<HashMap<String, Token>>,
}

impl PoTokens {
    pub fn new() -> Self {
        let binary = find_executable("rustypipe-botguard");
        match &binary {
            Some(path) => tracing::info!(path = %path.display(), "po tokens available"),
            None => tracing::warn!("rustypipe-botguard not found; uploaded songs and gated formats will not play"),
        }
        Self { binary, minted: Mutex::new(HashMap::new()) }
    }

    /// A token for this video, minting one when there is none to reuse.
    pub async fn for_video(&self, video_id: &str) -> Option<String> {
        if let Some(token) = self.cached(video_id) {
            return Some(token);
        }
        let binary = self.binary.as_ref()?;
        let output = Command::new(binary).arg(video_id).output().await.ok()?;
        if !output.status.success() {
            tracing::warn!(video_id, "po token minting failed");
            return None;
        }
        let (value, expires) = parse(&String::from_utf8_lossy(&output.stdout))?;
        tracing::debug!(video_id, "po token minted");
        self.minted.lock().unwrap().insert(video_id.to_owned(), Token { value: value.clone(), expires });
        Some(value)
    }

    fn cached(&self, video_id: &str) -> Option<String> {
        let mut minted = self.minted.lock().unwrap();
        match minted.get(video_id) {
            Some(token) if token.expires > SystemTime::now() => Some(token.value.clone()),
            Some(_) => {
                minted.remove(video_id);
                None
            }
            None => None,
        }
    }
}

/// The tool prints `<token> valid_until=<unix seconds> from_snapshot=<bool>`.
fn parse(output: &str) -> Option<(String, SystemTime)> {
    let line = output.lines().rev().find(|line| line.contains("valid_until="))?;
    let mut parts = line.split_whitespace();
    let value = parts.next()?.to_owned();
    let seconds: u64 = parts.find_map(|part| part.strip_prefix("valid_until="))?.parse().ok()?;
    let expires = (UNIX_EPOCH + Duration::from_secs(seconds)).checked_sub(EARLY)?;
    (!value.is_empty()).then_some((value, expires))
}

/// The extractor argument yt-dlp takes for a minted token.
pub fn extractor_arg(token: &str) -> String {
    format!("youtube:po_token=web_music.gvs+{token}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_and_its_expiry_come_off_the_last_line() {
        let output = "some chatter\nMlkHsFxhgjaq7Afy valid_until=1789420443 from_snapshot=true\n";
        let (value, expires) = parse(output).expect("parsed");
        assert_eq!(value, "MlkHsFxhgjaq7Afy");
        let stated = UNIX_EPOCH + Duration::from_secs(1789420443);
        assert_eq!(stated.duration_since(expires).unwrap(), EARLY, "tokens are dropped before they actually expire");
    }

    #[test]
    fn output_without_an_expiry_is_no_token() {
        assert!(parse("").is_none());
        assert!(parse("rustypipe-botguard: challenge failed\n").is_none());
    }

    #[test]
    fn the_argument_names_the_client_the_token_is_for() {
        assert_eq!(extractor_arg("abc"), "youtube:po_token=web_music.gvs+abc");
    }
}
