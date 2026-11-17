//! YouTube Music navigation, account transitions and stale-result rejection.
//! The controller performs every read; this module only decides and holds data.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::effects::{Effect, YouTubeMusicRequest};
use crate::library::Track;
use crate::youtube_music::{
    Failure, Playlist, PlaylistContinuation, PlaylistPage, Song, SongContinuation, SongPage,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    Search,
    Saved,
    Playlists,
    Playlist,
}

impl View {
    pub fn identifier(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Saved => "saved",
            Self::Playlists => "playlists",
            Self::Playlist => "playlist",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SongRow {
    pub song: Song,
    pub track: Track,
}

#[derive(Clone, Debug)]
pub struct PlaylistRow {
    pub playlist: Playlist,
    pub cover_id: i64,
}

pub struct YouTubeMusicState {
    pub query: String,
    pub view: View,
    pub connected: bool,
    pub busy: bool,
    pub loading: bool,
    pub failed: bool,
    pub status_key: &'static str,
    pub playlist_title: String,
    pub songs: Vec<SongRow>,
    pub playlists: Vec<PlaylistRow>,
    pub next_songs: Option<SongContinuation>,
    pub next_playlists: Option<PlaylistContinuation>,
    epoch: u64,
    pending_cookie_file: Option<PathBuf>,
    playlist_identifier: String,
    registered: HashMap<String, SongRow>,
    cover_urls: HashMap<i64, String>,
    next_identifier: i32,
}

impl Default for YouTubeMusicState {
    fn default() -> Self {
        Self {
            query: String::new(),
            view: View::Search,
            connected: false,
            busy: false,
            loading: false,
            failed: false,
            status_key: "youtubeMusic.status.disconnected",
            playlist_title: String::new(),
            songs: Vec::new(),
            playlists: Vec::new(),
            next_songs: None,
            next_playlists: None,
            epoch: 0,
            pending_cookie_file: None,
            playlist_identifier: String::new(),
            registered: HashMap::new(),
            cover_urls: HashMap::new(),
            next_identifier: -2,
        }
    }
}

impl YouTubeMusicState {
    pub fn is_current(&self, epoch: u64) -> bool {
        self.epoch == epoch
    }

    pub fn connect(&mut self, cookie_file: PathBuf) -> Vec<Effect> {
        self.epoch += 1;
        self.busy = true;
        self.loading = false;
        self.failed = false;
        self.status_key = "youtubeMusic.status.connecting";
        self.pending_cookie_file = Some(cookie_file.clone());
        vec![Effect::ConnectYouTubeMusic {
            cookie_file,
            epoch: self.epoch,
        }]
    }

    pub fn connection_finished(&mut self, epoch: u64, outcome: Result<(), Failure>) -> Vec<Effect> {
        if !self.is_current(epoch) {
            return Vec::new();
        }
        self.busy = false;
        match outcome {
            Ok(()) => {
                self.connected = true;
                self.failed = false;
                self.status_key = "youtubeMusic.status.connected";
                let mut effects = vec![Effect::PersistYouTubeMusicCookieFile(
                    self.pending_cookie_file.take(),
                )];
                effects.extend(self.select_view("saved"));
                effects
            }
            Err(failure) => {
                self.pending_cookie_file = None;
                self.failure(failure)
            }
        }
    }

    pub fn disconnect(&mut self) -> Vec<Effect> {
        self.epoch += 1;
        self.connected = false;
        self.busy = false;
        self.loading = false;
        self.failed = false;
        self.status_key = "youtubeMusic.status.disconnected";
        self.pending_cookie_file = None;
        self.view = View::Search;
        self.songs.clear();
        self.playlists.clear();
        self.next_songs = None;
        self.next_playlists = None;
        self.playlist_title.clear();
        self.playlist_identifier.clear();
        self.registered.clear();
        self.cover_urls.clear();
        // Never reuse transient ids: an old thumbnail may still be in the cache.
        vec![
            Effect::DisconnectYouTubeMusic,
            Effect::PersistYouTubeMusicCookieFile(None),
        ]
    }

    pub fn search(&mut self, query: &str) -> Vec<Effect> {
        if self.busy {
            return Vec::new();
        }
        self.query = query.trim().to_owned();
        self.view = View::Search;
        if self.query.is_empty() {
            self.epoch += 1;
            self.loading = false;
            self.songs.clear();
            self.next_songs = None;
            return Vec::new();
        }
        self.begin(YouTubeMusicRequest::Search(self.query.clone()), false)
    }

    pub fn select_view(&mut self, identifier: &str) -> Vec<Effect> {
        if self.busy {
            return Vec::new();
        }
        let request = match identifier {
            "search" => return self.search(&self.query.clone()),
            "saved" => {
                self.view = View::Saved;
                YouTubeMusicRequest::SavedSongs
            }
            "playlists" => {
                self.view = View::Playlists;
                YouTubeMusicRequest::SavedPlaylists
            }
            _ => return Vec::new(),
        };
        if !self.connected {
            self.epoch += 1;
            self.loading = false;
            self.songs.clear();
            self.playlists.clear();
            self.status_key = "youtubeMusic.status.disconnected";
            return Vec::new();
        }
        self.begin(request, false)
    }

    pub fn open_playlist(&mut self, index: usize) -> Vec<Effect> {
        if self.busy || !self.connected {
            return Vec::new();
        }
        let Some(row) = self.playlists.get(index) else {
            return Vec::new();
        };
        self.playlist_title = row.playlist.title.clone();
        self.playlist_identifier = row.playlist.identifier.clone();
        self.view = View::Playlist;
        self.begin(
            YouTubeMusicRequest::Playlist(self.playlist_identifier.clone()),
            false,
        )
    }

    pub fn more(&mut self) -> Vec<Effect> {
        if self.busy || self.loading {
            return Vec::new();
        }
        if self.view == View::Playlists && self.next_playlists.is_some() {
            self.begin(YouTubeMusicRequest::MorePlaylists, true)
        } else if self.view != View::Playlists && self.next_songs.is_some() {
            self.begin(YouTubeMusicRequest::MoreSongs, true)
        } else {
            Vec::new()
        }
    }

    pub fn has_more(&self) -> bool {
        if self.view == View::Playlists {
            self.next_playlists.is_some()
        } else {
            self.next_songs.is_some()
        }
    }

    fn begin(&mut self, request: YouTubeMusicRequest, append: bool) -> Vec<Effect> {
        self.epoch += 1;
        self.loading = true;
        self.failed = false;
        self.status_key = "youtubeMusic.status.loading";
        if !append {
            self.next_songs = None;
            self.next_playlists = None;
            if self.view == View::Playlists {
                self.playlists.clear();
            } else {
                self.songs.clear();
            }
        }
        vec![Effect::ReadYouTubeMusic {
            request,
            epoch: self.epoch,
        }]
    }

    pub fn songs_finished(
        &mut self,
        epoch: u64,
        outcome: Result<SongPage, Failure>,
        append: bool,
    ) -> Vec<Effect> {
        if !self.is_current(epoch) {
            return Vec::new();
        }
        self.loading = false;
        match outcome {
            Ok(page) => {
                if !append {
                    self.songs.clear();
                }
                for song in page.songs {
                    let row = self.register(song);
                    if !self
                        .songs
                        .iter()
                        .any(|shown| shown.track.id == row.track.id)
                    {
                        self.songs.push(row);
                    }
                }
                self.next_songs = page.next;
                self.succeeded();
                Vec::new()
            }
            Err(failure) => self.failure(failure),
        }
    }

    pub fn playlists_finished(
        &mut self,
        epoch: u64,
        outcome: Result<PlaylistPage, Failure>,
        append: bool,
    ) -> Vec<Effect> {
        if !self.is_current(epoch) {
            return Vec::new();
        }
        self.loading = false;
        match outcome {
            Ok(page) => {
                if !append {
                    self.playlists.clear();
                }
                for playlist in page.playlists {
                    if self
                        .playlists
                        .iter()
                        .any(|shown| shown.playlist.identifier == playlist.identifier)
                    {
                        continue;
                    }
                    let cover_id = self.allocate_identifier();
                    if let Some(url) = &playlist.artwork_url {
                        self.cover_urls.insert(cover_id, url.clone());
                    }
                    self.playlists.push(PlaylistRow { playlist, cover_id });
                }
                self.next_playlists = page.next;
                self.succeeded();
                Vec::new()
            }
            Err(failure) => self.failure(failure),
        }
    }

    fn succeeded(&mut self) {
        self.failed = false;
        self.status_key = if self.connected {
            "youtubeMusic.status.connected"
        } else {
            "youtubeMusic.status.disconnected"
        };
    }

    fn failure(&mut self, failure: Failure) -> Vec<Effect> {
        self.failed = true;
        let rejected = failure.credentials_rejected();
        self.status_key = if rejected {
            "youtubeMusic.status.credentialsRejected"
        } else {
            "youtubeMusic.status.failed"
        };
        let mut effects = Vec::new();
        if rejected {
            self.connected = false;
            self.songs.clear();
            self.playlists.clear();
            self.next_songs = None;
            self.next_playlists = None;
            effects.push(Effect::DisconnectYouTubeMusic);
        }
        effects.push(Effect::NotifyError {
            key: failure.translation_key(),
            parameters: Vec::new(),
            dedupe_key: Some("youtube-music"),
        });
        effects
    }

    fn allocate_identifier(&mut self) -> i64 {
        let identifier = self.next_identifier;
        self.next_identifier = self
            .next_identifier
            .checked_sub(1)
            .expect("transient track identifiers fit the interface");
        i64::from(identifier)
    }

    fn register(&mut self, song: Song) -> SongRow {
        let identifier = self
            .registered
            .get(&song.identifier)
            .map(|row| row.track.id)
            .unwrap_or_else(|| self.allocate_identifier());
        if let Some(url) = &song.artwork_url {
            self.cover_urls.insert(identifier, url.clone());
        }
        let row = SongRow {
            track: song_track(&song, identifier),
            song,
        };
        self.registered
            .insert(row.song.identifier.clone(), row.clone());
        row
    }

    pub fn cover_url(&self, track_id: i64) -> Option<&str> {
        self.cover_urls.get(&track_id).map(String::as_str)
    }
    pub fn queue(&self) -> Vec<Track> {
        self.songs.iter().map(|row| row.track.clone()).collect()
    }
    pub fn visible_track_id(&self, index: usize) -> Option<i64> {
        if self.view == View::Playlists {
            self.playlists.get(index).map(|row| row.cover_id)
        } else {
            self.songs.get(index).map(|row| row.track.id)
        }
    }
    pub fn visible_length(&self) -> usize {
        if self.view == View::Playlists {
            self.playlists.len()
        } else {
            self.songs.len()
        }
    }
}

/// Transient source songs use a stable provider address, never a signed media URL.
pub fn song_track(song: &Song, identifier: i64) -> Track {
    Track {
        id: identifier,
        folder_id: None,
        server_id: None,
        artwork_item: None,
        server_libraries: Vec::new(),
        path: format!("youtube-music://{}", song.identifier),
        title: song.title.clone(),
        artist: (!song.artist.is_empty()).then(|| song.artist.clone()),
        album: song.album.clone(),
        composer: None,
        duration_secs: f64::from(song.duration_seconds),
        format: "AAC".into(),
        sample_rate: None,
        bit_depth: None,
        channels: None,
        lossless: false,
        added_at: 0,
        track_number: None,
        track_total: None,
        disc_number: None,
        disc_total: None,
        year: None,
        genre: None,
        album_artist: None,
        gain_db: 0.0,
    }
}

pub fn video_identifier(track: &Track) -> Option<&str> {
    track.path.strip_prefix("youtube-music://")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn song(identifier: &str) -> Song {
        Song {
            identifier: identifier.into(),
            title: "Song".into(),
            artist: "Artist".into(),
            album: None,
            duration_seconds: 100,
            artwork_url: None,
        }
    }
    fn epoch(effects: &[Effect]) -> u64 {
        match &effects[0] {
            Effect::ReadYouTubeMusic { epoch, .. } => *epoch,
            _ => panic!("a read effect"),
        }
    }
    #[test]
    fn an_old_search_cannot_replace_a_new_query_or_a_disconnected_account() {
        let mut state = YouTubeMusicState::default();
        let old = epoch(&state.search("old"));
        let current = epoch(&state.search("new"));
        state.songs_finished(
            old,
            Ok(SongPage {
                songs: vec![song("old")],
                next: None,
            }),
            false,
        );
        assert!(state.songs.is_empty());
        state.songs_finished(
            current,
            Ok(SongPage {
                songs: vec![song("new")],
                next: None,
            }),
            false,
        );
        assert_eq!(state.songs[0].song.identifier, "new");
        state.disconnect();
        state.songs_finished(
            current,
            Ok(SongPage {
                songs: vec![song("new")],
                next: None,
            }),
            false,
        );
        assert!(state.songs.is_empty());
        assert!(!state.connected);
    }
    #[test]
    fn account_views_do_not_fetch_without_a_connection() {
        let mut state = YouTubeMusicState::default();
        assert!(state.select_view("saved").is_empty());
        assert!(state.select_view("playlists").is_empty());
        assert_eq!(state.view, View::Playlists);
        assert!(!state.loading);
    }
    #[test]
    fn duplicate_page_songs_keep_one_stable_identity_and_disconnect_never_reuses_it() {
        let mut state = YouTubeMusicState::default();
        let request = epoch(&state.search("song"));
        state.songs_finished(
            request,
            Ok(SongPage {
                songs: vec![song("one"), song("one")],
                next: None,
            }),
            false,
        );
        let identifier = state.songs[0].track.id;
        assert_eq!(state.songs.len(), 1);
        let request = epoch(&state.search("again"));
        state.songs_finished(
            request,
            Ok(SongPage {
                songs: vec![song("one")],
                next: None,
            }),
            false,
        );
        assert_eq!(state.songs[0].track.id, identifier);
        state.disconnect();
        let request = epoch(&state.search("again"));
        state.songs_finished(
            request,
            Ok(SongPage {
                songs: vec![song("one")],
                next: None,
            }),
            false,
        );
        assert!(state.songs[0].track.id < identifier);
        assert_eq!(state.songs[0].track.path, "youtube-music://one");
    }
    #[test]
    fn clearing_a_query_invalidates_its_pending_read() {
        let mut state = YouTubeMusicState::default();
        let old = epoch(&state.search("pending"));
        state.search("  ");
        state.songs_finished(
            old,
            Ok(SongPage {
                songs: vec![song("one")],
                next: None,
            }),
            false,
        );
        assert!(state.songs.is_empty());
        assert_eq!(state.query, "");
    }
}
