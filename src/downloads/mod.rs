//! Offline downloads: what is on disk, and the queue that puts it there.
//!
//! Port of downloads.py. A job is a track plus the album it came from. The
//! manager fills in whatever metadata the row was missing, hands the audio to
//! yt-dlp, writes tags and cover art, moves the file into the music folder and
//! records it in the shared library. Three downloads run at once, as in Python.
//!
//! Everything here runs on the tokio runtime. The GTK thread reads `Store`
//! answers straight away (they are in memory) and hears about progress through
//! the event channel.

pub mod m3u;
pub mod naming;
pub mod store;
pub mod tags;

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::model::{HttpAuth, Track};
use crate::net::NetHandle;
use crate::net::stream::{find_executable, write_netscape_cookies};
use crate::paths::Paths;

use naming::{codec_for, disambiguated_name, download_dir, file_name, preferred_format};
use store::{Entry, Store};
use tags::Tags;

/// How many downloads run at once, like the Python thread pool.
const WORKERS: usize = 3;

/// Scratch directories older than this are crash debris, not a running job.
const STALE_TMP: Duration = Duration::from_secs(3600);

/// What the header button and the pages listen to.
#[derive(Debug, Clone)]
pub enum Event {
    /// A track joined the queue.
    Queued { video_id: String },
    /// How far the file itself is, 0 to 1.
    Progress { video_id: String, fraction: f64 },
    /// One track finished, for better or worse.
    Item { video_id: String, ok: bool, message: String },
    /// Queue position, for the header pie.
    Advanced { done: usize, total: usize, title: String },
    /// A download was deleted from disk.
    Removed { video_id: String },
    /// The queue drained.
    Idle { downloaded: usize },
}

/// What every download in one run shares: the chosen format, the cookie jar
/// written once for the whole queue, and the session it belongs to.
struct Run {
    format: String,
    cookies: Option<PathBuf>,
    auth: Option<HttpAuth>,
}

/// One track waiting to be downloaded.
#[derive(Debug, Clone)]
struct Job {
    track: Track,
    album_title: String,
    album_id: String,
    track_number: Option<u32>,
}

/// A playlist to mirror as .m3u8 once its tracks land.
struct Mirror {
    id: String,
    title: String,
    tracks: Vec<Track>,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<Job>,
    in_flight: HashSet<String>,
    running: bool,
    done: usize,
    total: usize,
    succeeded: usize,
    mirrors: Vec<Mirror>,
}

pub struct Downloads {
    paths: Paths,
    net: NetHandle,
    store: Store,
    binary: PathBuf,
    queue: Mutex<Queue>,
    events: async_channel::Sender<Event>,
}

impl Downloads {
    /// Open the library and get a channel of what happens next.
    pub fn new(paths: Paths, net: NetHandle) -> (Arc<Self>, async_channel::Receiver<Event>) {
        let (events, receiver) = async_channel::unbounded();
        let store = Store::open(&paths.music_dir());
        let binary = find_executable("yt-dlp").unwrap_or_else(|| PathBuf::from("yt-dlp"));
        let downloads = Arc::new(Self { paths, net, store, binary, queue: Mutex::new(Queue::default()), events });
        downloads.sweep_scratch();
        (downloads, receiver)
    }

    // -- what is on disk --------------------------------------------------

    /// The download library itself, which also holds the shared history cache.
    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn is_downloaded(&self, video_id: &str) -> bool {
        self.store.is_downloaded(video_id)
    }

    /// The file to play instead of a stream. Works offline, needs no resolve.
    pub fn local_path(&self, video_id: &str) -> Option<PathBuf> {
        self.store.local_path(video_id)
    }

    pub fn downloaded_count(&self) -> usize {
        self.store.count()
    }

    /// Everything downloaded, newest first.
    pub fn all(&self) -> Vec<Entry> {
        self.store.all()
    }

    /// The cover of a downloaded track, kept beside the stream cache so rows
    /// render offline. Present only after a download or an extraction.
    pub fn cached_cover(&self, video_id: &str) -> Option<PathBuf> {
        let path = self.cover_file(video_id);
        path.is_file().then_some(path)
    }

    /// Pull the cover out of the file itself, for songs downloaded before this
    /// cache existed or by the Python app. Reads a file, so keep it off the
    /// GTK thread.
    pub fn extract_cover(&self, video_id: &str) -> Option<PathBuf> {
        if let Some(path) = self.cached_cover(video_id) {
            return Some(path);
        }
        let source = self.store.local_path(video_id)?;
        let bytes = tags::embedded_cover(&source)?;
        self.write_cover(video_id, &bytes)
    }

    fn cover_file(&self, video_id: &str) -> PathBuf {
        self.paths.cache_dir.join("local_covers").join(format!("{video_id}.jpg"))
    }

    fn write_cover(&self, video_id: &str, bytes: &[u8]) -> Option<PathBuf> {
        let path = self.cover_file(video_id);
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, bytes).ok()?;
        Some(path)
    }

    // -- the queue --------------------------------------------------------

    pub fn is_queued(&self, video_id: &str) -> bool {
        let queue = self.queue.lock().unwrap();
        queue.in_flight.contains(video_id) || queue.waiting.iter().any(|job| job.track.video_id.0 == video_id)
    }

    /// Queue position while something is downloading.
    pub fn progress(&self) -> Option<(usize, usize)> {
        let queue = self.queue.lock().unwrap();
        queue.running.then_some((queue.done, queue.total))
    }

    /// Queue tracks and start working. Already downloaded rows are skipped,
    /// so pressing Download All twice costs nothing.
    pub fn queue_tracks(self: &Arc<Self>, tracks: Vec<Track>, _album_title: &str, album_id: &str) {
        let numbered = tracks.len() > 1;
        for (index, track) in tracks.into_iter().enumerate() {
            let number = numbered.then_some(index as u32 + 1);
            self.queue_one(track, album_id, number);
        }
        self.start();
    }

    fn queue_one(self: &Arc<Self>, track: Track, album_id: &str, track_number: Option<u32>) {
        let video_id = track.video_id.0.clone();
        if video_id.is_empty() || self.is_downloaded(&video_id) {
            return;
        }
        {
            let mut queue = self.queue.lock().unwrap();
            if queue.in_flight.contains(&video_id) || queue.waiting.iter().any(|job| job.track.video_id.0 == video_id) {
                return;
            }
            // The album is the track's own. A playlist title never becomes one,
            // or every playlist would turn into a folder full of albums.
            let album = track.album.as_ref();
            let job = Job {
                album_title: album.map(|a| a.name.clone()).unwrap_or_default(),
                album_id: album.and_then(|a| a.id.clone()).unwrap_or_else(|| album_id.to_owned()),
                track_number,
                track,
            };
            queue.waiting.push_back(job);
            queue.total += 1;
        }
        self.emit(Event::Queued { video_id });
    }

    /// Remember a playlist so its .m3u8 mirror is rewritten as tracks land.
    pub fn register_playlist(&self, playlist_id: &str, title: &str, tracks: Vec<Track>) {
        if title.is_empty() {
            return;
        }
        let mut queue = self.queue.lock().unwrap();
        match queue.mirrors.iter_mut().find(|m| m.id == playlist_id) {
            Some(existing) => {
                existing.title = title.to_owned();
                let known: HashSet<String> = existing.tracks.iter().map(|t| t.video_id.0.clone()).collect();
                existing.tracks.extend(tracks.into_iter().filter(|t| !known.contains(&t.video_id.0)));
            }
            None => queue.mirrors.push(Mirror { id: playlist_id.to_owned(), title: title.to_owned(), tracks }),
        }
    }

    /// Take a track out of the queue. Returns false when it already started.
    pub fn cancel(&self, video_id: &str) -> bool {
        let removed = {
            let mut queue = self.queue.lock().unwrap();
            let before = queue.waiting.len();
            queue.waiting.retain(|job| job.track.video_id.0 != video_id);
            let removed = queue.waiting.len() < before;
            if removed {
                queue.total = queue.total.saturating_sub(1).max(queue.done);
            }
            removed
        };
        if removed {
            self.emit(Event::Item { video_id: video_id.to_owned(), ok: false, message: "Cancelled".to_owned() });
        }
        removed
    }

    /// Delete the file and forget the track. Empty folders go with it.
    pub fn delete(&self, video_id: &str) -> bool {
        let Some(path) = self.store.local_path(video_id) else {
            self.store.forget(video_id);
            return false;
        };
        if let Err(err) = std::fs::remove_file(&path) {
            tracing::warn!(%err, path = %path.display(), "could not delete download");
        }
        if let Some(dir) = path.parent() {
            prune_empty(&self.paths, dir);
        }
        self.store.forget(video_id);
        let _ = std::fs::remove_file(self.cover_file(video_id));
        m3u::prune(&self.paths);
        self.emit(Event::Removed { video_id: video_id.to_owned() });
        true
    }

    /// Move every download to match the current folder preference.
    ///
    /// For when the layout setting changes. Returns how many moved and how
    /// many could not, which is what the settings page reports.
    pub fn migrate_layout(&self) -> (usize, usize) {
        relayout(&self.paths, &self.store)
    }

    // -- running the queue ------------------------------------------------

    fn start(self: &Arc<Self>) {
        {
            let mut queue = self.queue.lock().unwrap();
            if queue.running || queue.waiting.is_empty() {
                return;
            }
            queue.running = true;
            queue.done = 0;
            queue.succeeded = 0;
            queue.total = queue.waiting.len();
        }
        let this = self.clone();
        self.net.spawn(async move { this.work().await });
    }

    async fn work(self: Arc<Self>) {
        let auth = self.net.client().media_auth();
        let cookies = match &auth {
            Some(auth) => write_netscape_cookies(&self.paths.cache_dir, &auth.cookie).await.ok(),
            None => None,
        };
        let run = Arc::new(Run { format: preferred_format(&self.paths), cookies, auth });
        let mut running = tokio::task::JoinSet::new();
        loop {
            while running.len() < WORKERS {
                let Some(job) = self.take_job() else { break };
                let this = self.clone();
                let run = run.clone();
                running.spawn(async move {
                    let video_id = job.track.video_id.0.clone();
                    let title = job.track.title.clone();
                    let outcome = this.fetch(job, &run).await;
                    (video_id, title, outcome)
                });
            }
            let Some(finished) = running.join_next().await else { break };
            let (video_id, title, outcome) = match finished {
                Ok(result) => result,
                Err(err) => {
                    tracing::warn!(%err, "download task died");
                    continue;
                }
            };
            let (ok, message) = match outcome {
                Ok(message) => (true, message),
                Err(message) => (false, message),
            };
            let (done, total) = {
                let mut queue = self.queue.lock().unwrap();
                queue.in_flight.remove(&video_id);
                queue.done += 1;
                if ok {
                    queue.succeeded += 1;
                }
                (queue.done, queue.total)
            };
            if ok {
                self.refresh_mirrors(&video_id);
            } else {
                tracing::warn!(video_id, message, "download failed");
            }
            self.emit(Event::Advanced { done, total, title });
            self.emit(Event::Item { video_id, ok, message });
        }
        if let Some(path) = &run.cookies {
            let _ = tokio::fs::remove_file(path).await;
        }
        let downloaded = {
            let mut queue = self.queue.lock().unwrap();
            queue.running = false;
            queue.total = 0;
            queue.done = 0;
            queue.mirrors.clear();
            std::mem::take(&mut queue.succeeded)
        };
        self.emit(Event::Idle { downloaded });
    }

    fn take_job(&self) -> Option<Job> {
        let mut queue = self.queue.lock().unwrap();
        let job = queue.waiting.pop_front()?;
        queue.in_flight.insert(job.track.video_id.0.clone());
        Some(job)
    }

    /// One track, start to finish. The message is what the row shows.
    async fn fetch(&self, job: Job, run: &Run) -> Result<String, String> {
        let video_id = job.track.video_id.0.clone();
        let details = self.describe(job).await;
        let dir = download_dir(&self.paths, &details.artist, &details.album);
        let extension = run.format.as_str();
        let mut path = dir.join(file_name(&dir, &details.title, extension));

        if path.exists() {
            match self.store.owner_of(&path) {
                Some(owner) if owner != video_id => path = dir.join(disambiguated_name(&dir, &details.title, extension, &video_id)),
                _ => {}
            }
            if path.exists() {
                self.record(&video_id, &details, &path, &run.format);
                return Ok("Already downloaded".to_owned());
            }
        }
        std::fs::create_dir_all(&dir).map_err(|err| format!("folder: {err}"))?;

        let scratch = self.scratch_dir(&video_id);
        let result = self.fetch_into(&scratch, &video_id, &details, run, &path).await;
        let _ = std::fs::remove_dir_all(&scratch);
        let final_path = result?;

        self.record(&video_id, &details, &final_path, &run.format);
        Ok("Downloaded".to_owned())
    }

    async fn fetch_into(&self, scratch: &Path, video_id: &str, details: &Details, run: &Run, target: &Path) -> Result<PathBuf, String> {
        std::fs::create_dir_all(scratch).map_err(|err| format!("scratch: {err}"))?;
        self.run_ytdlp(scratch, video_id, run).await?;

        let produced = std::fs::read_dir(scratch)
            .map_err(|err| format!("scratch: {err}"))?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.file_stem().and_then(|s| s.to_str()) == Some("audio"))
            .ok_or_else(|| "no audio file came out".to_owned())?;

        let cover = match details.thumbnail.is_empty() {
            true => None,
            false => crate::ui::cover::fetch_cover_bytes(self.net.client().http(), run.auth.as_ref(), &details.thumbnail, None).await,
        };
        tags::write(&produced, &details.tags(video_id), cover.as_deref());
        if let Some(bytes) = &cover {
            self.write_cover(video_id, bytes);
        }

        // The extension follows what yt-dlp actually produced, not what was asked for.
        let target = match produced.extension().and_then(|e| e.to_str()) {
            Some(extension) if Some(extension) != target.extension().and_then(|e| e.to_str()) => target.with_extension(extension),
            _ => target.to_path_buf(),
        };
        move_file(&produced, &target).map_err(|err| format!("move: {err}"))?;
        Ok(target)
    }

    async fn run_ytdlp(&self, scratch: &Path, video_id: &str, run: &Run) -> Result<(), String> {
        let mut command = Command::new(&self.binary);
        // Same token the resolver needs: an uploaded song is served to the
        // web_music client alone, and that client is gated behind one.
        if let Some(token) = self.net.tokens().for_video(video_id).await {
            command.arg("--extractor-args").arg(crate::net::potoken::extractor_arg(&token));
        }
        command
            .arg("--no-playlist")
            .arg("--no-warnings")
            .arg("--newline")
            .args(["--js-runtimes", "node"])
            .args(["-f", "bestaudio/best"])
            .arg("-x")
            .args(["--audio-format", codec_for(&run.format)])
            .args(["--audio-quality", "0"])
            .args(["--progress-template", "MIXTAPES %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s"])
            .arg("-o")
            .arg(scratch.join("audio.%(ext)s"))
            .arg(format!("https://music.youtube.com/watch?v={video_id}"));
        if let Some(path) = &run.cookies {
            command.arg("--cookies").arg(path);
        }
        if let Some(auth) = &run.auth {
            command.args(["--user-agent", &auth.user_agent]);
        }
        command.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());

        tracing::debug!(argv = ?command.as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>(), "yt-dlp invocation");
        let mut child = command.spawn().map_err(|err| format!("yt-dlp: {err}"))?;
        if let Some(stdout) = child.stdout.take() {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(fraction) = parse_progress(&line) {
                    self.emit(Event::Progress { video_id: video_id.to_owned(), fraction });
                }
            }
        }
        let output = child.wait_with_output().await.map_err(|err| format!("yt-dlp: {err}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let reason = stderr.lines().rev().find(|l| l.contains("ERROR")).unwrap_or("yt-dlp failed");
            return Err(reason.trim_start_matches("ERROR: ").chars().take(80).collect());
        }
        Ok(())
    }

    /// Fill in what the row did not carry: album, artists, cover, track number.
    ///
    /// A song opened from search has no album, and without one the file would
    /// land in the wrong folder with no album tag.
    async fn describe(&self, job: Job) -> Details {
        let api = self.net.client().api();
        let track = &job.track;
        let mut details = Details {
            title: track.title.clone(),
            artist: join_artists(track),
            artist_id: track.artists.first().and_then(|a| a.id.clone()).unwrap_or_default(),
            album: job.album_title.clone(),
            album_id: job.album_id.clone(),
            thumbnail: track.thumb.clone().unwrap_or_default(),
            duration_seconds: track.duration_seconds,
            track_number: job.track_number,
            like_status: track.like_status,
            ..Details::default()
        };

        // An uploaded track carries its own tags. The watch panel answers with
        // whatever catalogue song YouTube matched it to, which is a different
        // name and album.
        let uploaded = track.entity_id.is_some();
        if !uploaded && (details.album.is_empty() || details.thumbnail.is_empty() || details.artist.is_empty()) {
            if let Ok(watch) = crate::net::playlists::get_watch_playlist(&api, Some(&track.video_id.0), None, 1, false).await {
                if let Some(first) = watch.tracks.first().map(|w| &w.track) {
                    if details.album.is_empty() {
                        if let Some(album) = &first.album {
                            details.album = album.name.clone();
                            details.album_id = details.album_id.clone().or_else_empty(album.id.clone());
                        }
                    }
                    if details.artist.is_empty() {
                        details.artist = join_artists(first);
                        details.artist_id = first.artists.first().and_then(|a| a.id.clone()).unwrap_or_default();
                    }
                    if details.title.is_empty() {
                        details.title = first.title.clone();
                    }
                    if details.thumbnail.is_empty() {
                        details.thumbnail = first.thumb.clone().unwrap_or_default();
                    }
                    details.duration_seconds = details.duration_seconds.or(first.duration_seconds);
                }
            }
        }

        if details.album_id.starts_with("MPRE") {
            if let Ok(album) = crate::net::playlists::get_album(&api, &details.album_id).await {
                details.album_artist = album.author.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ");
                details.year = album.year.clone().unwrap_or_default();
                details.track_total = album.track_count;
                if details.album.is_empty() {
                    details.album = album.title.clone();
                }
                if details.thumbnail.is_empty() {
                    details.thumbnail = album.thumbnails.last().cloned().unwrap_or_default();
                }
                let position = album.tracks.iter().position(|t| t.video_id == track.video_id).or_else(|| album.tracks.iter().position(|t| t.title.eq_ignore_ascii_case(&details.title)));
                if let Some(index) = position {
                    details.track_number = Some(index as u32 + 1);
                }
            }
        }

        if details.artist.is_empty() {
            details.artist = "Unknown Artist".to_owned();
        }
        details
    }

    fn record(&self, video_id: &str, details: &Details, path: &Path, format: &str) {
        let file_size = std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);
        self.store.add(&Entry {
            video_id: video_id.to_owned(),
            title: details.title.clone(),
            artist: details.artist.clone(),
            artist_id: details.artist_id.clone(),
            album: details.album.clone(),
            album_id: details.album_id.clone(),
            track_number: details.track_number,
            duration_seconds: details.duration_seconds,
            file_path: path.to_owned(),
            thumbnail_url: details.thumbnail.clone(),
            downloaded_at: timestamp(),
            file_size,
            format: format.to_owned(),
            like_status: details.like_status,
        });
    }

    /// Rewrite the mirror of every playlist holding this track.
    fn refresh_mirrors(&self, video_id: &str) {
        let mirrors: Vec<(String, String, Vec<Track>)> = {
            let queue = self.queue.lock().unwrap();
            queue
                .mirrors
                .iter()
                .filter(|m| m.tracks.iter().any(|t| t.video_id.0 == video_id))
                .map(|m| (m.id.clone(), m.title.clone(), m.tracks.clone()))
                .collect()
        };
        for (id, title, tracks) in mirrors {
            m3u::write(&self.paths, &self.store, &id, &title, &tracks);
        }
    }

    fn scratch_dir(&self, video_id: &str) -> PathBuf {
        self.paths.cache_dir.join("downloads").join(video_id)
    }

    /// Clear scratch directories a crash left behind, never one in use.
    fn sweep_scratch(&self) {
        let root = self.paths.cache_dir.join("downloads");
        let Ok(entries) = std::fs::read_dir(&root) else { return };
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|m| SystemTime::now().duration_since(m).unwrap_or_default() > STALE_TMP)
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send_blocking(event);
    }
}

/// Port of migrate_folder_structure: move every download to match the current
/// folder preference. Files whose destination is taken stay where they are.
pub fn relayout(paths: &Paths, store: &Store) -> (usize, usize) {
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut errors = 0;
    for entry in store.all() {
        let old_path = entry.file_path.clone();
        if !old_path.exists() {
            continue;
        }
        let artist = if entry.artist.is_empty() { "Unknown Artist".to_owned() } else { entry.artist.clone() };
        let dir = download_dir(paths, &artist, &entry.album);
        let Some(name) = old_path.file_name().and_then(|n| n.to_str()) else { continue };
        let mut new_path = dir.join(name);
        if new_path == old_path {
            continue;
        }
        if new_path.exists() {
            let stem = old_path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
            let extension = old_path.extension().and_then(|e| e.to_str()).unwrap_or("");
            new_path = dir.join(format!("{stem} [{}].{extension}", entry.video_id));
            if new_path.exists() {
                tracing::warn!(path = %old_path.display(), "migration skipped, destination taken");
                errors += 1;
                continue;
            }
        }
        if let Err(err) = std::fs::create_dir_all(&dir).and_then(|_| move_file(&old_path, &new_path)) {
            tracing::warn!(%err, path = %old_path.display(), "migration failed");
            errors += 1;
            continue;
        }
        store.moved(&entry.video_id, &new_path);
        if let Some(parent) = old_path.parent() {
            prune_empty(paths, parent);
        }
        moves.push((old_path, new_path));
    }
    if !moves.is_empty() {
    m3u::repoint(paths, &moves);
    }
    (moves.len(), errors)
}

/// Remove folders left empty by a move or a deletion, stopping at the music folder.
fn prune_empty(paths: &Paths, start: &Path) {
    let music = paths.music_dir();
    let mut dir = start.to_path_buf();
    while dir.starts_with(&music) && dir != music {
        if std::fs::remove_dir(&dir).is_err() {
            break;
        }
        let Some(parent) = dir.parent() else { break };
        dir = parent.to_path_buf();
    }
}

/// Everything known about a track by the time it is written to disk.
#[derive(Debug, Default, Clone)]
struct Details {
    title: String,
    artist: String,
    artist_id: String,
    album: String,
    album_id: String,
    album_artist: String,
    year: String,
    thumbnail: String,
    duration_seconds: Option<u32>,
    track_number: Option<u32>,
    track_total: Option<u32>,
    like_status: crate::model::LikeStatus,
}

impl Details {
    fn tags(&self, video_id: &str) -> Tags {
        Tags {
            title: self.title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            album_artist: self.album_artist.clone(),
            track_number: self.track_number,
            track_total: self.track_total,
            year: self.year.clone(),
            video_id: video_id.to_owned(),
            album_id: self.album_id.clone(),
        }
    }
}

/// Keeps the first non-empty of two strings, for filling gaps in metadata.
trait OrElseEmpty {
    fn or_else_empty(self, other: Option<String>) -> String;
}

impl OrElseEmpty for String {
    fn or_else_empty(self, other: Option<String>) -> String {
        if self.is_empty() { other.unwrap_or_default() } else { self }
    }
}

fn join_artists(track: &Track) -> String {
    let joined = track.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ");
    if joined.is_empty() { track.artist.clone() } else { joined }
}

/// Local time, the format Python wrote into downloaded_at.
pub(crate) fn timestamp() -> String {
    glib::DateTime::now_local().ok().and_then(|t| t.format("%Y-%m-%dT%H:%M:%S").ok()).map(|s| s.to_string()).unwrap_or_default()
}

/// yt-dlp progress lines, as asked for by the progress template.
fn parse_progress(line: &str) -> Option<f64> {
    let rest = line.strip_prefix("MIXTAPES ")?;
    let mut parts = rest.split_whitespace();
    let done: f64 = parts.next()?.parse().ok()?;
    let total = parts.next().and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0);
    let estimate = parts.next().and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0);
    let total = total.or(estimate)?;
    Some((done / total).clamp(0.0, 1.0))
}

/// Rename, falling back to a copy when the music folder is another filesystem.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(from, to)?;
            std::fs::remove_file(from)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_lines_become_a_fraction() {
        assert_eq!(parse_progress("MIXTAPES 50 100 NA"), Some(0.5));
        assert_eq!(parse_progress("MIXTAPES 25 NA 100"), Some(0.25));
        assert_eq!(parse_progress("MIXTAPES 200 100 NA"), Some(1.0), "a resumed file cannot exceed the whole");
        assert_eq!(parse_progress("MIXTAPES 10 NA NA"), None);
        assert_eq!(parse_progress("[download] 12.3% of 4MiB"), None);
    }

    #[test]
    fn artists_join_the_way_the_row_shows_them() {
        let mut track = Track { artist: "Fallback".into(), ..Track::default() };
        assert_eq!(join_artists(&track), "Fallback");
        track.artists = vec![crate::model::Person { name: "A".into(), id: None }, crate::model::Person { name: "B".into(), id: None }];
        assert_eq!(join_artists(&track), "A, B");
    }

    /// A layout change moves files and rewrites the mirrors that point at them.
    #[test]
    fn changing_the_layout_moves_files_and_fixes_the_mirrors() {
        let home = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_tests(home.path());
        paths.update_prefs(|p| {
            p.insert("download_folder_structure".into(), "flat".into());
            p.insert("use_songs_subdir".into(), false.into());
        });
        let store = Store::open(&paths.music_dir());

        let old_dir = paths.music_dir().join("Artist/Album");
        std::fs::create_dir_all(&old_dir).unwrap();
        let old_path = old_dir.join("One.opus");
        std::fs::write(&old_path, b"audio").unwrap();
        store.add(&Entry { video_id: "a".into(), title: "One".into(), artist: "Artist".into(), album: "Album".into(), file_path: old_path.clone(), ..Default::default() });

        let mirrors = naming::playlists_dir(&paths);
        std::fs::create_dir_all(&mirrors).unwrap();
        let mirror = mirrors.join("Mix.m3u8");
        std::fs::write(&mirror, "#EXTM3U\n#EXTINF:1,Artist - One\n../Artist/Album/One.opus\n").unwrap();

        let (moved, errors) = relayout(&paths, &store);
        assert_eq!((moved, errors), (1, 0));
        let new_path = paths.music_dir().join("One.opus");
        assert!(new_path.exists(), "the file moved to the flat layout");
        assert!(!old_dir.exists(), "the folders it left behind are gone");
        assert_eq!(store.local_path("a"), Some(new_path));
        assert!(std::fs::read_to_string(&mirror).unwrap().contains("../One.opus"));
    }

    /// The second copy of a name keeps its own file rather than overwriting.
    #[test]
    fn a_taken_destination_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_tests(home.path());
        paths.update_prefs(|p| {
            p.insert("download_folder_structure".into(), "flat".into());
        });
        let store = Store::open(&paths.music_dir());
        let old_dir = paths.music_dir().join("Artist");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("One.opus"), b"audio").unwrap();
        std::fs::write(paths.music_dir().join("One.opus"), b"other").unwrap();
        store.add(&Entry { video_id: "a".into(), title: "One".into(), artist: "Artist".into(), file_path: old_dir.join("One.opus"), ..Default::default() });

        let (moved, errors) = relayout(&paths, &store);
        assert_eq!((moved, errors), (1, 0));
        assert!(paths.music_dir().join("One.opus").exists());
        assert!(paths.music_dir().join("One [a].opus").exists(), "the mover keeps both");
        assert_eq!(std::fs::read(paths.music_dir().join("One.opus")).unwrap(), b"other");
    }

    #[test]
    fn a_move_across_filesystems_still_lands() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.opus");
        let to = dir.path().join("b.opus");
        std::fs::write(&from, b"audio").unwrap();
        move_file(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(std::fs::read(&to).unwrap(), b"audio");
    }
}
