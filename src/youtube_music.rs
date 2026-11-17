//! YouTube Music catalog and account access, with playback extraction delegated separately.
//!
//! Calls belong on a worker. The caller owns the imported file; this session keeps
//! its path for extraction and holds catalog authentication cookies only in memory.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rustypipe::client::{RustyPipe, RustyPipeQuery};
use rustypipe::error::{Error, ExtractionError};
use rustypipe::model::paginator::{ContinuationEndpoint, Paginator};
use rustypipe::model::{MusicPlaylistItem, Thumbnail, TrackItem, TrackType};

use crate::audio::Location;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const LARGEST_COOKIE_FILE_BYTES: u64 = 1024 * 1024;
const LARGEST_ARTWORK_BYTES: usize = 16 * 1024 * 1024;
const LONGEST_ARTWORK_ADDRESS: usize = 8192;

#[derive(Clone, Debug, PartialEq)]
pub struct Song {
    pub identifier: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_seconds: u32,
    pub artwork_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Playlist {
    pub identifier: String,
    pub title: String,
    pub description: String,
    pub artwork_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SongPage {
    pub songs: Vec<Song>,
    pub next: Option<SongContinuation>,
}

#[derive(Clone, Debug)]
pub struct PlaylistPage {
    pub playlists: Vec<Playlist>,
    pub next: Option<PlaylistContinuation>,
}

/// Tokens are endpoint-specific and may only be used by their originating session.
#[derive(Clone)]
struct Continuation {
    token: String,
    visitor_data: Option<String>,
    endpoint: ContinuationEndpoint,
    authenticated: bool,
    owner: Arc<()>,
}

impl fmt::Debug for Continuation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Continuation { .. }")
    }
}

#[derive(Clone, Debug)]
pub struct SongContinuation(Continuation);

#[derive(Clone, Debug)]
pub struct PlaylistContinuation(Continuation);

#[derive(Clone)]
pub struct Session {
    client: RustyPipe,
    artwork_client: reqwest::Client,
    authenticated: bool,
    cookie_file: Option<PathBuf>,
    identity: Arc<()>,
}

impl fmt::Debug for Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Session")
            .field("authenticated", &self.authenticated)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct ResolvedAudio {
    pub location: Location,
    pub duration_seconds: f64,
    pub gain_db: f64,
    pub expires_at: SystemTime,
}

impl fmt::Debug for ResolvedAudio {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Location contains a signed URL, so even diagnostic formatting omits it.
        formatter
            .debug_struct("ResolvedAudio")
            .field("duration_seconds", &self.duration_seconds)
            .field("gain_db", &self.gain_db)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// No upstream message, file path, cookie or request address crosses this boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Initialization,
    CookieFileUnreadable,
    InvalidCookieFile,
    CredentialsRejected,
    Network,
    Unavailable,
    UnsupportedAudio,
    ExtractorUnavailable,
    Extraction,
    InvalidResponse,
    InvalidInput,
    InvalidContinuation,
    InvalidArtworkAddress,
    ArtworkTooLarge,
}

impl Failure {
    pub fn credentials_rejected(&self) -> bool {
        matches!(self, Self::CredentialsRejected | Self::InvalidCookieFile)
    }

    pub fn translation_key(&self) -> &'static str {
        match self {
            Self::Initialization => "youtubeMusic.error.initialization",
            Self::CookieFileUnreadable => "youtubeMusic.error.cookieFile",
            Self::InvalidCookieFile => "youtubeMusic.error.invalidCookies",
            Self::CredentialsRejected => "youtubeMusic.status.credentialsRejected",
            Self::Network => "youtubeMusic.error.network",
            Self::Unavailable => "youtubeMusic.error.unavailable",
            Self::UnsupportedAudio => "youtubeMusic.error.unsupportedAudio",
            Self::ExtractorUnavailable => "youtubeMusic.error.extractorUnavailable",
            Self::Extraction => "youtubeMusic.error.extraction",
            Self::InvalidResponse => "youtubeMusic.error.response",
            Self::InvalidInput => "youtubeMusic.error.input",
            Self::InvalidContinuation => "youtubeMusic.error.page",
            Self::InvalidArtworkAddress => "youtubeMusic.error.artworkAddress",
            Self::ArtworkTooLarge => "youtubeMusic.error.artworkSize",
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Initialization => "the YouTube Music client could not be initialized",
            Self::CookieFileUnreadable => "the imported cookie file could not be read",
            Self::InvalidCookieFile => "the file does not contain valid YouTube cookies",
            Self::CredentialsRejected => {
                "the YouTube session was rejected; export fresh cookies and reconnect"
            }
            Self::Network => "YouTube Music could not be reached; try again later",
            Self::Unavailable => "this content is unavailable for this session or region",
            Self::UnsupportedAudio => "this track has no supported unencrypted AAC/M4A audio",
            Self::ExtractorUnavailable => {
                "yt-dlp is not available; install it to play YouTube Music"
            }
            Self::Extraction => "yt-dlp could not resolve this track; update it and try again",
            Self::InvalidResponse => "YouTube Music returned an unsupported response",
            Self::InvalidInput => "the YouTube Music request is invalid",
            Self::InvalidContinuation => "this page belongs to a different YouTube Music session",
            Self::InvalidArtworkAddress => {
                "the artwork address is not a supported YouTube image address"
            }
            Self::ArtworkTooLarge => "the artwork exceeds the download limit",
        })
    }
}

impl std::error::Error for Failure {}

impl Session {
    pub fn anonymous() -> Result<Self, Failure> {
        let client = RustyPipe::builder()
            .no_storage()
            .no_reporter()
            .no_botguard()
            .timeout(REQUEST_TIMEOUT)
            .n_http_retries(0)
            .build()
            .map_err(|_| Failure::Initialization)?;
        let artwork_client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 3 || !is_artwork_address(attempt.url()) {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|_| Failure::Initialization)?;
        Ok(Self {
            client,
            artwork_client,
            authenticated: false,
            cookie_file: None,
            identity: Arc::new(()),
        })
    }

    /// Reads the file only on the worker that performs the account connection.
    /// An accepted cookie must also be able to read the music account library.
    pub async fn from_cookie_file(path: &Path) -> Result<Self, Failure> {
        let cookies = read_cookie_file(path)?;
        let mut session = Self::anonymous()?;
        session
            .client
            .user_auth_set_cookie_txt(&cookies)
            .await
            .map_err(upstream_failure)?;
        drop(cookies);
        session
            .client
            .query()
            .authenticated()
            .music_saved_tracks()
            .await
            .map_err(upstream_failure)?;
        session.authenticated = true;
        session.cookie_file = Some(path.to_path_buf());
        Ok(session)
    }

    pub async fn search(&self, query: &str) -> Result<SongPage, Failure> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(SongPage {
                songs: Vec::new(),
                next: None,
            });
        }
        let result = self
            .query(false)
            .music_search_tracks(query)
            .await
            .map_err(upstream_failure)?;
        Ok(self.song_page(result.items))
    }

    pub async fn saved_songs(&self) -> Result<SongPage, Failure> {
        self.require_account()?;
        let page = self
            .query(true)
            .music_saved_tracks()
            .await
            .map_err(upstream_failure)?;
        Ok(self.song_page(page))
    }

    pub async fn saved_playlists(&self) -> Result<PlaylistPage, Failure> {
        self.require_account()?;
        let page = self
            .query(true)
            .music_saved_playlists()
            .await
            .map_err(upstream_failure)?;
        Ok(self.playlist_page(page))
    }

    pub async fn playlist(&self, identifier: &str) -> Result<SongPage, Failure> {
        if !is_playlist_identifier(identifier) {
            return Err(Failure::InvalidInput);
        }
        let playlist = self
            .query(self.authenticated)
            .music_playlist(identifier)
            .await
            .map_err(upstream_failure)?;
        Ok(self.song_page(playlist.tracks))
    }

    pub async fn more_songs(&self, continuation: SongContinuation) -> Result<SongPage, Failure> {
        let continuation = continuation.0;
        self.check_continuation(&continuation)?;
        let page = self
            .query(continuation.authenticated)
            .continuation::<TrackItem, _>(
                continuation.token,
                continuation.endpoint,
                continuation.visitor_data.as_deref(),
            )
            .await
            .map_err(upstream_failure)?;
        Ok(self.song_page(page))
    }

    pub async fn more_playlists(
        &self,
        continuation: PlaylistContinuation,
    ) -> Result<PlaylistPage, Failure> {
        let continuation = continuation.0;
        self.check_continuation(&continuation)?;
        let page = self
            .query(continuation.authenticated)
            .continuation::<MusicPlaylistItem, _>(
                continuation.token,
                continuation.endpoint,
                continuation.visitor_data.as_deref(),
            )
            .await
            .map_err(upstream_failure)?;
        Ok(self.playlist_page(page))
    }

    pub async fn resolve(&self, identifier: &str) -> Result<ResolvedAudio, Failure> {
        crate::youtube_extraction::resolve(identifier, self.cookie_file.as_deref()).await
    }

    /// Downloads encoded bytes; the existing cover decoder applies pixel limits.
    /// This separate client never receives the account's authentication cookies.
    pub async fn artwork(&self, url: &str) -> Result<Vec<u8>, Failure> {
        let address = artwork_address(url)?;
        let mut response = self
            .artwork_client
            .get(address)
            .send()
            .await
            .map_err(|_| Failure::Network)?;
        if !response.status().is_success() {
            return Err(Failure::Unavailable);
        }
        if response
            .content_length()
            .is_some_and(|length| length > LARGEST_ARTWORK_BYTES as u64)
        {
            return Err(Failure::ArtworkTooLarge);
        }
        let mut bytes = Vec::with_capacity(response.content_length().unwrap_or(0) as usize);
        while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Network)? {
            if chunk.len() > LARGEST_ARTWORK_BYTES.saturating_sub(bytes.len()) {
                return Err(Failure::ArtworkTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            return Err(Failure::InvalidResponse);
        }
        Ok(bytes)
    }

    fn query(&self, authenticated: bool) -> RustyPipeQuery {
        if authenticated {
            self.client.query().authenticated()
        } else {
            self.client.query().unauthenticated()
        }
    }

    fn require_account(&self) -> Result<(), Failure> {
        if self.authenticated {
            Ok(())
        } else {
            Err(Failure::CredentialsRejected)
        }
    }

    fn check_continuation(&self, continuation: &Continuation) -> Result<(), Failure> {
        if !Arc::ptr_eq(&self.identity, &continuation.owner) {
            return Err(Failure::InvalidContinuation);
        }
        if continuation.authenticated {
            self.require_account()?;
        }
        Ok(())
    }

    fn continuation<T>(&self, page: &mut Paginator<T>) -> Option<Continuation> {
        page.ctoken.take().map(|token| Continuation {
            token,
            visitor_data: page.visitor_data.take(),
            endpoint: page.endpoint,
            authenticated: page.authenticated,
            owner: Arc::clone(&self.identity),
        })
    }

    fn song_page(&self, mut page: Paginator<TrackItem>) -> SongPage {
        let next = self.continuation(&mut page).map(SongContinuation);
        SongPage {
            songs: page.items.into_iter().filter_map(song).collect(),
            next,
        }
    }

    fn playlist_page(&self, mut page: Paginator<MusicPlaylistItem>) -> PlaylistPage {
        let next = self.continuation(&mut page).map(PlaylistContinuation);
        PlaylistPage {
            playlists: page.items.into_iter().filter_map(playlist).collect(),
            next,
        }
    }
}

fn read_cookie_file(path: &Path) -> Result<String, Failure> {
    if !path
        .metadata()
        .map_err(|_| Failure::CookieFileUnreadable)?
        .is_file()
    {
        return Err(Failure::InvalidCookieFile);
    }
    let file = std::fs::File::open(path).map_err(|_| Failure::CookieFileUnreadable)?;
    if !file
        .metadata()
        .map_err(|_| Failure::CookieFileUnreadable)?
        .is_file()
    {
        return Err(Failure::InvalidCookieFile);
    }
    let mut bytes = Vec::new();
    file.take(LARGEST_COOKIE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure::CookieFileUnreadable)?;
    if bytes.len() as u64 > LARGEST_COOKIE_FILE_BYTES {
        return Err(Failure::InvalidCookieFile);
    }
    String::from_utf8(bytes).map_err(|_| Failure::InvalidCookieFile)
}

fn upstream_failure(error: Error) -> Failure {
    match error {
        Error::Auth(_) | Error::HttpStatus(401 | 403, _) => Failure::CredentialsRejected,
        Error::Http(_) | Error::HttpStatus(_, _) => Failure::Network,
        Error::Extraction(
            ExtractionError::Unavailable { .. } | ExtractionError::NotFound { .. },
        ) => Failure::Unavailable,
        _ => Failure::InvalidResponse,
    }
}

fn is_song_identifier(identifier: &str) -> bool {
    identifier.len() == 11 && is_identifier(identifier)
}

fn is_playlist_identifier(identifier: &str) -> bool {
    !identifier.is_empty() && identifier.len() <= 256 && is_identifier(identifier)
}

fn is_identifier(identifier: &str) -> bool {
    identifier
        .bytes()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, b'-' | b'_'))
}

fn song(track: TrackItem) -> Option<Song> {
    if !is_song_identifier(&track.id)
        || track.name.trim().is_empty()
        || track.track_type == TrackType::Episode
        || track.unavailable
    {
        return None;
    }
    let duration_seconds = track.duration.filter(|duration| *duration > 0)?;
    let artist = track
        .artists
        .into_iter()
        .map(|artist| artist.name)
        .filter(|artist| !artist.trim().is_empty())
        .reduce(|mut combined, artist| {
            combined.push_str(", ");
            combined.push_str(&artist);
            combined
        })?;
    Some(Song {
        identifier: track.id,
        title: track.name,
        artist,
        album: track
            .album
            .map(|album| album.name)
            .filter(|name| !name.trim().is_empty()),
        duration_seconds,
        artwork_url: thumbnail(track.cover),
    })
}

fn playlist(playlist: MusicPlaylistItem) -> Option<Playlist> {
    if !is_playlist_identifier(&playlist.id)
        || playlist.name.trim().is_empty()
        || playlist.is_podcast
    {
        return None;
    }
    Some(Playlist {
        identifier: playlist.id,
        title: playlist.name,
        // The account-list endpoint supplies the creator, not a full description.
        description: playlist
            .channel
            .map(|channel| channel.name)
            .unwrap_or_default(),
        artwork_url: thumbnail(playlist.thumbnail),
    })
}

fn thumbnail(thumbnails: Vec<Thumbnail>) -> Option<String> {
    thumbnails
        .into_iter()
        .filter(|thumbnail| artwork_address(&thumbnail.url).is_ok())
        .max_by_key(|thumbnail| u64::from(thumbnail.width) * u64::from(thumbnail.height))
        .map(|thumbnail| thumbnail.url)
}

fn artwork_address(address: &str) -> Result<reqwest::Url, Failure> {
    if address.len() > LONGEST_ARTWORK_ADDRESS {
        return Err(Failure::InvalidArtworkAddress);
    }
    let address = if address.starts_with("//") {
        reqwest::Url::parse(&format!("https:{address}"))
    } else {
        reqwest::Url::parse(address)
    }
    .map_err(|_| Failure::InvalidArtworkAddress)?;
    if is_artwork_address(&address) {
        Ok(address)
    } else {
        Err(Failure::InvalidArtworkAddress)
    }
}

fn is_artwork_address(address: &reqwest::Url) -> bool {
    address.scheme() == "https"
        && address.port_or_known_default() == Some(443)
        && address.username().is_empty()
        && address.password().is_none()
        && address.host_str().is_some_and(|host| {
            ["ytimg.com", "ggpht.com", "googleusercontent.com"]
                .into_iter()
                .any(|domain| is_domain(host, domain))
        })
}

fn is_domain(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> TrackItem {
        serde_json::from_value(serde_json::json!({
            "id": "dQw4w9WgXcQ",
            "name": "A song",
            "duration": 213,
            "cover": [
                {"url": "https://lh3.googleusercontent.com/small", "width": 60, "height": 60},
                {"url": "https://lh3.googleusercontent.com/large", "width": 600, "height": 600}
            ],
            "artists": [{"id": "artist-one", "name": "First artist"}, {"id": null, "name": "Second artist"}],
            "artist_id": "artist-one",
            "album": {"id": "album-one", "name": "The album"},
            "view_count": null,
            "track_type": "track",
            "track_nr": null,
            "by_va": false,
            "unavailable": false
        }))
        .expect("the sample is a complete RustyPipe track")
    }

    #[test]
    fn song_mapping_preserves_artists_album_duration_and_largest_cover() {
        let mapped = song(track()).expect("the sample is a playable song row");
        assert_eq!(mapped.identifier, "dQw4w9WgXcQ");
        assert_eq!(mapped.title, "A song");
        assert_eq!(mapped.artist, "First artist, Second artist");
        assert_eq!(mapped.album.as_deref(), Some("The album"));
        assert_eq!(mapped.duration_seconds, 213);
        assert_eq!(
            mapped.artwork_url.as_deref(),
            Some("https://lh3.googleusercontent.com/large")
        );
    }

    #[test]
    fn incomplete_and_non_song_rows_are_not_offered_for_playback() {
        let mut unavailable = track();
        unavailable.unavailable = true;
        assert!(song(unavailable).is_none());
        let mut unavailable = track();
        unavailable.id.clear();
        assert!(song(unavailable).is_none());
        let mut unavailable = track();
        unavailable.duration = Some(0);
        assert!(song(unavailable).is_none());
        let mut incomplete = track();
        incomplete.duration = None;
        assert!(song(incomplete).is_none());
        let mut incomplete = track();
        incomplete.name.clear();
        assert!(song(incomplete).is_none());
        let mut incomplete = track();
        incomplete.artists.clear();
        assert!(song(incomplete).is_none());
        let mut podcast = track();
        podcast.track_type = TrackType::Episode;
        assert!(song(podcast).is_none());
    }

    #[test]
    fn page_mapping_preserves_search_endpoint_visitor_and_account_scope_even_if_rows_are_filtered()
    {
        let session = Session::anonymous().expect("an in-memory client can be created");
        let mut page = Paginator::default();
        page.ctoken = Some("private-continuation".into());
        page.visitor_data = Some("private-visitor".into());
        page.endpoint = ContinuationEndpoint::MusicSearch;
        page.authenticated = false;
        let mut unavailable = track();
        unavailable.unavailable = true;
        page.items.push(unavailable);
        let page = session.song_page(page);
        assert!(page.songs.is_empty());
        let continuation = page
            .next
            .expect("a filtered page may still have more rows")
            .0;
        assert_eq!(continuation.endpoint, ContinuationEndpoint::MusicSearch);
        assert_eq!(continuation.token, "private-continuation");
        assert_eq!(
            continuation.visitor_data.as_deref(),
            Some("private-visitor")
        );
        assert!(!continuation.authenticated);
        assert!(session.clone().check_continuation(&continuation).is_ok());
        let other_session = Session::anonymous().expect("an independent client can be created");
        assert_eq!(
            other_session.check_continuation(&continuation),
            Err(Failure::InvalidContinuation)
        );
        let diagnostic = format!("{continuation:?}");
        assert!(!diagnostic.contains("private-continuation"));
        assert!(!diagnostic.contains("private-visitor"));
    }

    #[test]
    fn account_playlist_mapping_preserves_creator_cover_and_authenticated_continuation() {
        let session = Session::anonymous().expect("an in-memory client can be created");
        let item: MusicPlaylistItem = serde_json::from_value(serde_json::json!({
            "id": "PLsaved_music",
            "name": "Saved music",
            "thumbnail": [{"url": "https://i.ytimg.com/playlist.jpg", "width": 300, "height": 300}],
            "channel": {"id": "creator", "name": "The creator"},
            "track_count": 125,
            "from_ytm": true,
            "is_podcast": false
        }))
        .expect("the sample is a complete RustyPipe music playlist");
        let mut page = Paginator::default();
        page.items.push(item.clone());
        page.ctoken = Some("account-continuation".into());
        page.visitor_data = Some("account-visitor".into());
        page.endpoint = ContinuationEndpoint::MusicBrowse;
        page.authenticated = true;
        let page = session.playlist_page(page);
        assert_eq!(page.playlists[0].identifier, "PLsaved_music");
        assert_eq!(page.playlists[0].title, "Saved music");
        assert_eq!(page.playlists[0].description, "The creator");
        assert_eq!(
            page.playlists[0].artwork_url.as_deref(),
            Some("https://i.ytimg.com/playlist.jpg")
        );
        let continuation = page.next.expect("the account page continues").0;
        assert_eq!(continuation.endpoint, ContinuationEndpoint::MusicBrowse);
        assert_eq!(
            continuation.visitor_data.as_deref(),
            Some("account-visitor")
        );
        assert!(continuation.authenticated);
        assert_eq!(
            session.check_continuation(&continuation),
            Err(Failure::CredentialsRejected)
        );
        let mut podcast = item;
        podcast.is_podcast = true;
        assert!(playlist(podcast).is_none());
    }

    #[test]
    fn error_boundary_discards_upstream_secrets_but_preserves_credential_rejection() {
        let secret = "SAPISID=secret-cookie; https://example.com/?token=signed-address";
        let errors = [
            Error::Auth(rustypipe::error::AuthError::Other(secret.into())),
            Error::HttpStatus(403, secret.into()),
            Error::Http(secret.into()),
            Error::Extraction(ExtractionError::InvalidData(secret.into())),
        ];
        for error in errors {
            let failure = upstream_failure(error);
            assert!(!format!("{failure}: {failure:?}").contains("secret-cookie"));
            assert!(!format!("{failure}: {failure:?}").contains("signed-address"));
        }
        assert!(upstream_failure(Error::HttpStatus(401, secret.into())).credentials_rejected());
        assert!(!upstream_failure(Error::Http(secret.into())).credentials_rejected());
    }

    #[test]
    fn session_diagnostics_do_not_reveal_the_imported_cookie_path() {
        let mut session = Session::anonymous().expect("an in-memory client can be created");
        session.authenticated = true;
        session.cookie_file = Some(PathBuf::from("/private/secret-cookie-export.txt"));
        let diagnostic = format!("{session:?}");
        assert!(diagnostic.contains("authenticated: true"));
        assert!(!diagnostic.contains("private"));
        assert!(!diagnostic.contains("secret-cookie-export"));
    }

    #[test]
    fn artwork_addresses_cannot_target_local_hosts_or_escape_the_image_domain() {
        for address in [
            "http://i.ytimg.com/image.jpg",
            "https://localhost/image.jpg",
            "https://127.0.0.1/image.jpg",
            "https://evilytimg.com/image.jpg",
            "https://i.ytimg.com.example.com/image.jpg",
            "https://i.ytimg.com:444/image.jpg",
            "https://secret@i.ytimg.com/image.jpg",
            "file:///private/cookies.txt",
        ] {
            assert_eq!(
                artwork_address(address),
                Err(Failure::InvalidArtworkAddress)
            );
        }
        assert!(artwork_address("https://i.ytimg.com/vi/dQw4w9WgXcQ/default.jpg").is_ok());
        assert!(artwork_address("//lh3.googleusercontent.com/cover").is_ok());
        assert_eq!(
            artwork_address(&"x".repeat(LONGEST_ARTWORK_ADDRESS + 1)),
            Err(Failure::InvalidArtworkAddress)
        );
    }

    #[test]
    fn cookie_file_read_rejects_oversized_and_non_utf8_exports() {
        let directory = tempfile::tempdir().expect("a temporary directory is available");
        let path = directory.path().join("cookies.txt");
        std::fs::write(&path, vec![b'x'; LARGEST_COOKIE_FILE_BYTES as usize + 1])
            .expect("the oversized file can be written");
        assert_eq!(read_cookie_file(&path), Err(Failure::InvalidCookieFile));
        std::fs::write(&path, [0xff]).expect("the invalid text file can be written");
        assert_eq!(read_cookie_file(&path), Err(Failure::InvalidCookieFile));
    }

    #[cfg(unix)]
    #[test]
    fn cookie_import_rejects_a_named_pipe_without_waiting_for_a_writer() {
        use std::sync::mpsc;
        use std::time::Duration;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cookies.pipe");
        assert!(std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success());
        let (sender, receiver) = mpsc::channel();
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            let result = read_cookie_file(&reader_path);
            sender.send(result).unwrap();
        });
        let result = receiver.recv_timeout(Duration::from_secs(5));
        if result.is_err() {
            // Release the pre-fix blocked reader so this test never strands a thread.
            drop(std::fs::OpenOptions::new().write(true).open(&path).unwrap());
        }
        reader.join().unwrap();
        assert_eq!(result.unwrap(), Err(Failure::InvalidCookieFile));
    }
}
