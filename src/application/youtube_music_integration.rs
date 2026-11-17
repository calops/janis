//! The Settings integration: complete account reads, serialized library imports,
//! and source-only removal. Browsing never uses the account's API session.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use slint::ComponentHandle;

use super::library_screens::LibrarySnapshot;
use super::{post, Application};
use crate::library::{self, YouTubeMusicTrack};
use crate::persistence;
use crate::user_interface::YouTubeMusicIntegration;
use crate::youtube_music::{Failure, Session};

impl Application {
    pub(super) fn connect_youtube_integration(self: &Rc<Self>) {
        let view = self.window.global::<YouTubeMusicIntegration>();
        view.on_sync_clicked(self.action(|application| {
            let effects = application.youtube_music.integration.borrow_mut().sync();
            application.run(effects);
            application.refresh_youtube_integration();
        }));
        view.on_disconnect_clicked(self.action(|application| {
            let effects = application
                .youtube_music
                .integration
                .borrow_mut()
                .disconnect();
            application.run(effects);
            application.refresh_youtube_integration();
        }));
        let weak = Rc::downgrade(self);
        view.on_connect_clicked(move || {
            if weak
                .upgrade()
                .is_none_or(|application| application.youtube_music.integration.borrow().busy)
            {
                return;
            }
            let weak = weak.clone();
            let Some(title) = weak.upgrade().map(|application| {
                application
                    .translations
                    .borrow()
                    .text("youtubeMusic.connect")
            }) else {
                return;
            };
            if let Err(error) = slint::spawn_local(async move {
                let selected = rfd::AsyncFileDialog::new()
                    .set_title(title)
                    .pick_file()
                    .await;
                if let (Some(application), Some(selected)) = (weak.upgrade(), selected) {
                    if application.youtube_music.integration.borrow().busy {
                        return;
                    }
                    let effects = application
                        .youtube_music
                        .integration
                        .borrow_mut()
                        .connect(selected.path().to_path_buf());
                    application.run(effects);
                    application.refresh_youtube_integration();
                }
            }) {
                log::error!("open YouTube cookie picker: {error}");
            }
        });
    }

    pub(super) fn sync_youtube_library(&self, cookie_file: PathBuf, epoch: u64) {
        self.youtube_music
            .sync_generation
            .store(epoch, Ordering::Release);
        let runtime = Arc::clone(&self.youtube_music.runtime);
        self.youtube_music.account_worker.run(move || {
            let outcome = runtime.block_on(async {
                Session::from_cookie_file(&cookie_file)
                    .await?
                    .saved_library()
                    .await
            });
            post(move |application| {
                application.youtube_library_read(cookie_file, epoch, outcome);
            });
        });
    }

    fn youtube_library_read(
        &self,
        cookie_file: PathBuf,
        epoch: u64,
        outcome: Result<Vec<YouTubeMusicTrack>, Failure>,
    ) {
        if !self.youtube_music.integration.borrow().is_current(epoch) {
            return;
        }
        let songs = match outcome {
            Ok(songs) => songs,
            Err(failure) => {
                let effects = self
                    .youtube_music
                    .integration
                    .borrow_mut()
                    .sync_finished(epoch, Err(failure));
                self.run(effects);
                self.refresh_youtube_integration();
                return;
            }
        };
        let database = Arc::clone(&self.database);
        let generation = Arc::clone(&self.youtube_music.sync_generation);
        self.library_worker.run(move || {
            // Disconnect can overtake a network read. It cannot overtake a
            // started import: its removal is queued on this same worker.
            if generation.load(Ordering::Acquire) != epoch {
                return;
            }
            let outcome = (|| {
                library::sync_youtube_music_tracks(&database, &songs, &cookie_file)?;
                let count = library::count_youtube_music_tracks(&database)?;
                let last_sync = persistence::youtube_music_last_sync(&database)?
                    .ok_or_else(|| "YouTube Music sync timestamp is missing".to_owned())?;
                Ok((LibrarySnapshot::read(&database)?, count, last_sync))
            })();
            post(move |application| {
                application.youtube_library_imported(cookie_file, epoch, outcome);
            });
        });
    }

    fn youtube_library_imported(
        &self,
        cookie_file: PathBuf,
        epoch: u64,
        outcome: Result<(LibrarySnapshot, usize, i64), String>,
    ) {
        if !self.youtube_music.integration.borrow().is_current(epoch) {
            return;
        }
        let result = match outcome {
            Ok((snapshot, count, last_sync)) => {
                {
                    let mut current = self
                        .youtube_music
                        .account_cookie_file
                        .write()
                        .expect("YouTube Music cookie path lock poisoned");
                    if current.as_ref() != Some(&cookie_file) {
                        *current = Some(cookie_file);
                        self.youtube_music
                            .generation
                            .fetch_add(1, Ordering::Release);
                    }
                }
                self.take_library(snapshot);
                Ok((count, last_sync))
            }
            Err(error) => {
                log::error!("YouTube Music library sync: {error}");
                self.notify_error("error.library", vec![("message", error)], None);
                Err(Failure::Library)
            }
        };
        let effects = self
            .youtube_music
            .integration
            .borrow_mut()
            .sync_finished(epoch, result);
        self.run(effects);
        self.refresh_everything();
    }

    pub(super) fn remove_youtube_library(&self) {
        self.youtube_music
            .sync_generation
            .fetch_add(1, Ordering::AcqRel);
        {
            let mut path = self
                .youtube_music
                .account_cookie_file
                .write()
                .expect("YouTube Music cookie path lock poisoned");
            *path = None;
            self.youtube_music
                .generation
                .fetch_add(1, Ordering::Release);
        }
        // Anonymous playback has no dependency on this connection.
        if self
            .player
            .borrow()
            .current()
            .is_some_and(|track| track.id > 0 && track.source == library::TrackSource::YouTubeMusic)
        {
            self.engine.stop();
        }
        let database = Arc::clone(&self.database);
        self.library_worker.run(move || {
            let outcome = library::remove_youtube_music_tracks(&database)
                .and_then(|()| LibrarySnapshot::read(&database));
            post(move |application| match outcome {
                Ok(snapshot) => {
                    application.take_library(snapshot);
                    application.refresh_everything();
                }
                Err(error) => {
                    log::error!("YouTube Music removal: {error}");
                    application.notify_error("error.library", vec![("message", error)], None);
                }
            });
        });
    }

    pub(super) fn refresh_youtube_integration(&self) {
        let state = self.youtube_music.integration.borrow();
        let translations = self.translations.borrow();
        let view = self.window.global::<YouTubeMusicIntegration>();
        view.set_connected(state.connected);
        view.set_configured(state.cookie_file.is_some());
        view.set_busy(state.busy);
        view.set_status_failed(state.failed);
        view.set_status(
            if state.status_key.is_empty() {
                String::new()
            } else {
                translations.text(state.status_key)
            }
            .into(),
        );
        view.set_summary(
            translations
                .count("youtubeMusic.summary", state.song_count)
                .into(),
        );
    }
}
