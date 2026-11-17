//! The YouTube Music source: worker reads, cookie-file connection and rows.

use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use slint::{ComponentHandle, ModelRc, VecModel};

use super::{interface_id, list_place, post, Application};
use crate::background::Worker;
use crate::covers::{self, Pixels};
use crate::effects::YouTubeMusicRequest;
use crate::text;
use crate::user_interface::{self, YouTubeMusic};
use crate::youtube_music::{Failure, PlaylistPage, Session, SongPage};
use crate::youtube_music_state::YouTubeMusicState;

pub(super) struct YouTubeMusicController {
    pub(super) state: RefCell<YouTubeMusicState>,
    pub(super) session: Arc<RwLock<Session>>,
    pub(super) generation: Arc<AtomicU64>,
    pub(super) runtime: Arc<tokio::runtime::Runtime>,
    worker: Worker,
    songs: Rc<VecModel<user_interface::YouTubeMusicSong>>,
    playlists: Rc<VecModel<user_interface::YouTubeMusicPlaylist>>,
}

impl YouTubeMusicController {
    pub(super) fn new() -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("janis youtube network")
            .enable_all()
            .build()
            .map_err(|error| format!("start YouTube Music runtime: {error}"))?;
        Ok(Self {
            state: RefCell::new(YouTubeMusicState::default()),
            session: Arc::new(RwLock::new(
                Session::anonymous().map_err(|error| error.to_string())?,
            )),
            generation: Arc::new(AtomicU64::new(0)),
            runtime: Arc::new(runtime),
            worker: Worker::new("janis youtube music")?,
            songs: Rc::new(VecModel::default()),
            playlists: Rc::new(VecModel::default()),
        })
    }

    pub(super) fn session(&self) -> Session {
        self.session
            .read()
            .expect("YouTube Music session lock poisoned")
            .clone()
    }

    fn replace_session(&self, session: Session) {
        let mut current = self
            .session
            .write()
            .expect("YouTube Music session lock poisoned");
        *current = session;
        self.generation.fetch_add(1, Ordering::Release);
    }
}

pub(super) struct PlaybackResolver {
    pub(super) session: Arc<RwLock<Session>>,
    pub(super) generation: Arc<AtomicU64>,
}

impl crate::audio::SourceResolver for PlaybackResolver {
    fn resolve<'a>(
        &'a self,
        identifier: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<crate::audio::RemoteFile, String>> + Send + 'a>> {
        Box::pin(async move {
            let (video_identifier, overview) = identifier
                .strip_prefix("youtube-music://")
                .map(|identifier| (identifier, false))
                .or_else(|| {
                    identifier
                        .strip_prefix("youtube-music-overview://")
                        .map(|identifier| (identifier, true))
                })
                .ok_or_else(|| Failure::InvalidInput.translation_key().to_owned())?;
            let (session, generation) = {
                let session = self
                    .session
                    .read()
                    .expect("YouTube Music session lock poisoned");
                (session.clone(), self.generation.load(Ordering::Acquire))
            };
            let resolved = session
                .resolve(video_identifier)
                .await
                .map_err(|error| error.translation_key().to_owned())?;
            if self.generation.load(Ordering::Acquire) != generation {
                return Err(Failure::CredentialsRejected.translation_key().to_owned());
            }
            if !overview {
                let identifier = identifier.to_owned();
                let expires_at = resolved.expires_at;
                post(move |application| {
                    if application.youtube_music.generation.load(Ordering::Acquire) == generation {
                        application
                            .player
                            .borrow_mut()
                            .source_resolved(&identifier, expires_at);
                    }
                });
            }
            match resolved.location {
                crate::audio::Location::Remote(remote) => Ok(remote),
                _ => Err(Failure::UnsupportedAudio.translation_key().to_owned()),
            }
        })
    }
}

enum ReadOutcome {
    Songs(SongPage),
    Playlists(PlaylistPage),
}

impl Application {
    pub(super) fn connect_youtube_music(self: &Rc<Self>) {
        let view = self.window.global::<YouTubeMusic>();
        view.set_songs(ModelRc::from(self.youtube_music.songs.clone()));
        view.set_playlists(ModelRc::from(self.youtube_music.playlists.clone()));
        view.on_search_submitted(self.action(|application| {
            let query = application.window.global::<YouTubeMusic>().get_query();
            let effects = application.youtube_music.state.borrow_mut().search(&query);
            application.run(effects);
            application.refresh_youtube_music();
        }));
        view.on_view_selected(self.handler(|application, view: slint::SharedString| {
            let effects = if view.as_str() == "search" {
                let query = application.window.global::<YouTubeMusic>().get_query();
                application.youtube_music.state.borrow_mut().search(&query)
            } else {
                application
                    .youtube_music
                    .state
                    .borrow_mut()
                    .select_view(&view)
            };
            application.run(effects);
            application.refresh_youtube_music();
        }));
        view.on_playlist_clicked(self.handler(|application, index: i32| {
            if let Some(index) = list_place(index) {
                let effects = application
                    .youtube_music
                    .state
                    .borrow_mut()
                    .open_playlist(index);
                application.run(effects);
                application.refresh_youtube_music();
            }
        }));
        view.on_more_clicked(self.action(|application| {
            let effects = application.youtube_music.state.borrow_mut().more();
            application.run(effects);
            application.refresh_youtube_music();
        }));
        view.on_song_clicked(self.handler(|application, index: i32| {
            let tracks = application.youtube_music.state.borrow().queue();
            if let Some(index) = list_place(index).filter(|index| *index < tracks.len()) {
                application.play_queue(tracks, index);
            }
        }));
        view.on_disconnect_clicked(self.action(|application| {
            let effects = application.youtube_music.state.borrow_mut().disconnect();
            application.run(effects);
            application.refresh_youtube_music();
        }));
        let weak = Rc::downgrade(self);
        view.on_connect_clicked(move || {
            let weak = weak.clone();
            let title = weak.upgrade().map(|application| {
                application
                    .translations
                    .borrow()
                    .text("youtubeMusic.connect")
            });
            let Some(title) = title else {
                return;
            };
            let spawned = slint::spawn_local(async move {
                let selected = rfd::AsyncFileDialog::new()
                    .set_title(title)
                    .pick_file()
                    .await;
                if let (Some(application), Some(selected)) = (weak.upgrade(), selected) {
                    application.connect_youtube_cookie_file(selected.path().to_path_buf());
                }
            });
            if let Err(error) = spawned {
                log::error!("open YouTube cookie picker: {error}");
            }
        });
    }

    pub(super) fn connect_youtube_cookie_file(&self, path: PathBuf) {
        let effects = self.youtube_music.state.borrow_mut().connect(path);
        self.run(effects);
        self.refresh_youtube_music();
    }

    pub(super) fn connect_youtube_account(&self, cookie_file: PathBuf, epoch: u64) {
        let runtime = Arc::clone(&self.youtube_music.runtime);
        self.youtube_music.worker.run(move || {
            let outcome = runtime.block_on(Session::from_cookie_file(&cookie_file));
            post(move |application| {
                if !application.youtube_music.state.borrow().is_current(epoch) {
                    return;
                }
                if let Ok(session) = &outcome {
                    if application.player.borrow().current().is_some_and(|track| {
                        crate::youtube_music_state::video_identifier(track).is_some()
                    }) {
                        application.engine.stop();
                    }
                    application.youtube_music.replace_session(session.clone());
                } else if let Err(failure) = &outcome {
                    log::warn!("YouTube Music connection: {failure}");
                }
                let effects = application
                    .youtube_music
                    .state
                    .borrow_mut()
                    .connection_finished(epoch, outcome.map(|_| ()));
                application.run(effects);
                application.refresh_youtube_music();
            });
        });
    }

    pub(super) fn disconnect_youtube_account(&self) {
        if self
            .player
            .borrow()
            .current()
            .is_some_and(|track| crate::youtube_music_state::video_identifier(track).is_some())
        {
            self.engine.stop();
        }
        match Session::anonymous() {
            Ok(session) => self.youtube_music.replace_session(session),
            Err(error) => log::error!("reset YouTube Music connection: {error}"),
        }
    }

    pub(super) fn read_youtube_music(&self, request: YouTubeMusicRequest, epoch: u64) {
        let session = self.youtube_music.session();
        let runtime = Arc::clone(&self.youtube_music.runtime);
        let next_songs = self.youtube_music.state.borrow().next_songs.clone();
        let next_playlists = self.youtube_music.state.borrow().next_playlists.clone();
        self.youtube_music.worker.run(move || {
            let append = matches!(
                request,
                YouTubeMusicRequest::MoreSongs | YouTubeMusicRequest::MorePlaylists
            );
            let playlists = matches!(
                request,
                YouTubeMusicRequest::SavedPlaylists | YouTubeMusicRequest::MorePlaylists
            );
            let outcome = runtime.block_on(async {
                match request {
                    YouTubeMusicRequest::Search(query) => {
                        session.search(&query).await.map(ReadOutcome::Songs)
                    }
                    YouTubeMusicRequest::SavedSongs => {
                        session.saved_songs().await.map(ReadOutcome::Songs)
                    }
                    YouTubeMusicRequest::SavedPlaylists => {
                        session.saved_playlists().await.map(ReadOutcome::Playlists)
                    }
                    YouTubeMusicRequest::Playlist(identifier) => {
                        session.playlist(&identifier).await.map(ReadOutcome::Songs)
                    }
                    YouTubeMusicRequest::MoreSongs => match next_songs {
                        Some(next) => session.more_songs(next).await.map(ReadOutcome::Songs),
                        None => Err(Failure::InvalidContinuation),
                    },
                    YouTubeMusicRequest::MorePlaylists => match next_playlists {
                        Some(next) => session
                            .more_playlists(next)
                            .await
                            .map(ReadOutcome::Playlists),
                        None => Err(Failure::InvalidContinuation),
                    },
                }
            });
            post(move |application| {
                if !application.youtube_music.state.borrow().is_current(epoch) {
                    return;
                }
                if let Err(failure) = &outcome {
                    log::warn!("YouTube Music read: {failure}");
                }
                let effects = match outcome {
                    Ok(ReadOutcome::Songs(page)) => application
                        .youtube_music
                        .state
                        .borrow_mut()
                        .songs_finished(epoch, Ok(page), append),
                    Ok(ReadOutcome::Playlists(page)) => application
                        .youtube_music
                        .state
                        .borrow_mut()
                        .playlists_finished(epoch, Ok(page), append),
                    Err(failure) if playlists => application
                        .youtube_music
                        .state
                        .borrow_mut()
                        .playlists_finished(epoch, Err(failure), append),
                    Err(failure) => application.youtube_music.state.borrow_mut().songs_finished(
                        epoch,
                        Err(failure),
                        append,
                    ),
                };
                application.run(effects);
                application.refresh_youtube_music();
                #[cfg(debug_assertions)]
                if std::env::var_os("JANIS_YOUTUBE_AUTOPLAY").is_some()
                    && application.player.borrow().mode() == crate::audio::Mode::Idle
                {
                    let tracks = application.youtube_music.state.borrow().queue();
                    if !tracks.is_empty() {
                        application.engine.set_volume(0.0);
                        application.play_queue(tracks, 0);
                    }
                }
            });
        });
    }

    pub(super) fn refresh_youtube_music(&self) {
        let state = self.youtube_music.state.borrow();
        let view = self.window.global::<YouTubeMusic>();
        view.set_view(state.view.identifier().into());
        view.set_connected(state.connected);
        view.set_busy(state.busy);
        view.set_loading(state.loading);
        view.set_status_failed(state.failed);
        view.set_has_more(state.has_more());
        view.set_status(self.translations.borrow().text(state.status_key).into());
        view.set_playlist_title(state.playlist_title.clone().into());
        self.youtube_music.songs.set_vec(
            state
                .songs
                .iter()
                .enumerate()
                .map(|(index, row)| user_interface::YouTubeMusicSong {
                    track_id: interface_id(row.track.id),
                    number: (index + 1).to_string().into(),
                    title: row.song.title.clone().into(),
                    artist: row.song.artist.clone().into(),
                    album: row.song.album.clone().unwrap_or_default().into(),
                    duration: text::format_time(f64::from(row.song.duration_seconds)).into(),
                    gradient_index: super::gradient_index(&row.track),
                })
                .collect::<Vec<_>>(),
        );
        self.youtube_music.playlists.set_vec(
            state
                .playlists
                .iter()
                .map(|row| user_interface::YouTubeMusicPlaylist {
                    identifier: row.playlist.identifier.clone().into(),
                    title: row.playlist.title.clone().into(),
                    description: row.playlist.description.clone().into(),
                    track_id: interface_id(row.cover_id),
                    gradient_index: text::art_gradient_index(&row.playlist.identifier) as i32,
                })
                .collect::<Vec<_>>(),
        );
    }
}

pub(super) fn source_picture(
    session: &Session,
    runtime: &tokio::runtime::Runtime,
    address: Option<&str>,
    longest_side: u32,
) -> Result<Option<Pixels>, String> {
    address
        .map(|address| {
            let bytes = runtime
                .block_on(session.artwork(address))
                .map_err(|error| error.to_string())?;
            covers::picture_cover(bytes, longest_side)
        })
        .transpose()
}
