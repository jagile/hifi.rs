use crate::{
    player::{
        controls::{Action, Controls},
        error::Error,
        notification::{BroadcastReceiver, BroadcastSender, Notification},
    },
    state::{
        app::{SafePlayerState, SkipDirection},
        ClockValue, StatusValue,
    },
    REFRESH_RESOLUTION,
};
use flume::Receiver;
use futures::prelude::*;
use gst::{
    bus::BusStream, prelude::*, ClockTime, Element, MessageView, SeekFlags, State as GstState,
    StateChangeSuccess, Structure,
};
use gstreamer as gst;
use hifirs_qobuz_api::client::{self, api::Client, AudioQuality, UrlType};
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio::{select, sync::RwLock};

#[macro_use]
pub mod controls;
pub mod error;
pub mod notification;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A player handles playing media to a device.
#[derive(Debug, Clone)]
pub struct Player {
    /// Used to broadcast the player state out to other components.
    playbin: Element,
    /// Qobuz client
    client: Client,
    /// The app state to save player inforamtion into.
    state: SafePlayerState,
    /// Player controls that can be exported to control the player externally.
    controls: Controls,
    /// Should the player quit when it reaches EOS?
    quit_when_done: bool,
    /// Broadcasts notifications from the player
    notify_sender: BroadcastSender,
    /// Receives notifications from the player. For use in other modules.
    notify_receiver: BroadcastReceiver,
    /// Receives the about-to-finish signal that alerts the player a new track should be loaded.
    about_to_finish_rx: Receiver<bool>,
}

const USER_AGENTS: &[&str] = &["Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/114.0.0.0 Safari/537.36", "Mozilla/5.0 (Macintosh; Intel Mac OS X 13_4) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/114.0.0.0 Safari/537.36"];

type SafePlayer = Arc<RwLock<Player>>;

#[instrument]
pub async fn new(client: Client, state: SafePlayerState, quit_when_done: bool) -> Result<Player> {
    gst::init()?;

    let playbin = gst::ElementFactory::make("playbin3").build()?;

    playbin.set_property_from_str("flags", "audio+buffering");

    playbin.connect("source-setup", false, |value| {
        let element = &value[1].get::<gst::Element>().unwrap();

        if element.name().contains("souphttpsrc") {
            debug!("new source, changing settings");
            let ua = if rand::random() {
                USER_AGENTS[0]
            } else {
                USER_AGENTS[1]
            };
            element.set_property("user-agent", ua);
            element.set_property("compress", false);
            element.set_property("retries", 10);
            element.set_property("timeout", 30_u32);
            element.set_property(
                "extra-headers",
                Structure::from_str("a-structure, DNT=1, Pragma=no-cache, Cache-Control=no-cache")
                    .expect("failed to make structure from string"),
            )
        }

        None
    });

    let (about_to_finish_tx, about_to_finish_rx) = flume::bounded::<bool>(1);
    let (mut notify_sender, notify_receiver) = async_broadcast::broadcast(10);
    notify_sender.set_overflow(true);

    // Connects to the `about-to-finish` signal so the player
    // can setup the next track to play. Enables gapless playback.
    playbin.connect("about-to-finish", false, move |_| {
        debug!("about to finish");
        about_to_finish_tx
            .send(true)
            .expect("failed to send about to finish message");

        None
    });

    let controls = Controls::new();

    let player = Player {
        client,
        playbin,
        controls,
        state,
        quit_when_done,
        notify_sender,
        notify_receiver,
        about_to_finish_rx,
    };

    Ok(player)
}

impl Player {
    /// Play the player.
    #[instrument(skip(self))]
    pub async fn play(&self, wait: bool) -> Result<()> {
        self.set_player_state(gst::State::Playing, wait).await?;
        Ok(())
    }
    /// Pause the player.
    #[instrument(skip(self))]
    pub async fn pause(&self, wait: bool) -> Result<()> {
        self.set_player_state(gst::State::Paused, wait).await?;
        Ok(())
    }
    /// Ready the player.
    #[instrument(skip(self))]
    pub async fn ready(&self, wait: bool) -> Result<()> {
        self.set_player_state(gst::State::Ready, wait).await?;
        Ok(())
    }
    /// Stop the player.
    #[instrument(skip(self))]
    pub async fn stop(&self, wait: bool) -> Result<()> {
        self.set_player_state(gst::State::Null, wait).await?;
        Ok(())
    }
    /// Sets the player to a specific state.
    #[instrument(skip(self))]
    pub async fn set_player_state(&self, state: gst::State, wait: bool) -> Result<()> {
        let ret = self.playbin.set_state(state)?;

        match ret {
            StateChangeSuccess::Success => {
                debug!("*** successful state change ***");
            }
            StateChangeSuccess::Async => {
                debug!("*** async state change ***");
            }
            StateChangeSuccess::NoPreroll => {
                debug!("*** stream is live ***");
                self.state.write().await.set_live(true);
            }
        }

        if wait {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            while self.current_state() != state.into() {
                debug!(
                    "waiting for player to change to {}",
                    self.current_state().as_str()
                );
                interval.tick().await;
            }
        }

        Ok(())
    }
    /// Toggle play and pause.
    #[instrument(skip(self))]
    pub async fn play_pause(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if self.is_playing() {
            state.set_target_status(GstState::Paused);
            self.pause(false).await?;
        } else if self.is_paused() || self.is_ready() {
            state.set_target_status(GstState::Playing);
            self.play(false).await?;
        }

        Ok(())
    }
    /// Retreive the current player state.
    #[instrument(skip(self))]
    pub fn state(&self) -> SafePlayerState {
        self.state.clone()
    }
    /// Is the player paused?
    #[instrument(skip(self))]
    pub fn is_paused(&self) -> bool {
        self.playbin.current_state() == gst::State::Paused
    }
    /// Is the player playing?
    #[instrument(skip(self))]
    pub fn is_playing(&self) -> bool {
        self.playbin.current_state() == gst::State::Playing
    }
    /// Is the player ready?
    #[instrument(skip(self))]
    pub fn is_ready(&self) -> bool {
        self.playbin.current_state() == gst::State::Ready
    }
    /// Current player state
    #[instrument(skip(self))]
    pub fn current_state(&self) -> StatusValue {
        self.playbin.current_state().into()
    }
    /// Current track position.
    #[instrument(skip(self))]
    pub fn position(&self) -> Option<ClockValue> {
        self.playbin
            .query_position::<ClockTime>()
            .map(|position| position.into())
    }
    /// Current track duraiton.
    #[instrument(skip(self))]
    pub fn duration(&self) -> Option<ClockValue> {
        self.playbin
            .query_duration::<ClockTime>()
            .map(|duration| duration.into())
    }
    /// Seek to a specified time in the current track.
    #[instrument(skip(self))]
    pub async fn seek(&self, time: ClockValue, flags: Option<SeekFlags>) -> Result<()> {
        let flags = if let Some(flags) = flags {
            flags
        } else {
            SeekFlags::FLUSH | SeekFlags::TRICKMODE_KEY_UNITS
        };

        self.playbin.seek_simple(flags, time.inner_clocktime())?;
        Ok(())
    }
    /// Load the previous player state and seek to the last known position.
    #[instrument(skip(self))]
    pub async fn resume(&mut self, autoplay: bool) -> Result<()> {
        let mut state = self.state.write().await;

        if state.load_last_state().await {
            state.set_resume(true);

            if autoplay {
                state.set_target_status(GstState::Playing);
            } else {
                state.set_target_status(GstState::Paused);
            }

            self.notify_sender
                .broadcast(Notification::CurrentTrackList {
                    list: state.track_list(),
                })
                .await?;

            if let Some(track) = state.current_track() {
                self.notify_sender
                    .broadcast(Notification::CurrentTrack {
                        track: track.clone(),
                    })
                    .await?;

                if let Some(url) = track.track_url {
                    self.playbin.set_property("uri", url.url);

                    self.ready(true).await?;
                    self.pause(true).await?;

                    let position = state.position();

                    self.seek(position.clone(), None).await?;

                    return Ok(());
                } else {
                    return Err(Error::Resume);
                }
            } else {
                return Err(Error::Resume);
            }
        }

        Ok(())
    }
    /// Retreive controls for the player.
    #[instrument(skip(self))]
    pub fn controls(&self) -> Controls {
        self.controls.clone()
    }
    /// Jump forward in the currently playing track +10 seconds.
    #[instrument(skip(self))]
    pub async fn jump_forward(&self) -> Result<()> {
        if let (Some(current_position), Some(duration)) = (
            self.playbin.query_position::<ClockTime>(),
            self.playbin.query_duration::<ClockTime>(),
        ) {
            let ten_seconds = ClockTime::from_seconds(10);
            let next_position = current_position + ten_seconds;

            if next_position < duration {
                self.seek(next_position.into(), None).await?;
            } else {
                self.seek(duration.into(), None).await?;
            }
        }

        Ok(())
    }
    /// Jump forward in the currently playing track -10 seconds.
    #[instrument(skip(self))]
    pub async fn jump_backward(&self) -> Result<()> {
        if let Some(current_position) = self.playbin.query_position::<ClockTime>() {
            if current_position.seconds() < 10 {
                self.seek(ClockTime::default().into(), None).await?;
            } else {
                let ten_seconds = ClockTime::from_seconds(10);
                let seek_position = current_position - ten_seconds;

                self.seek(seek_position.into(), None).await?;
            }
        }

        Ok(())
    }
    /// Skip to the next, previous or specific track in the playlist.
    #[instrument(skip(self))]
    pub async fn skip(&self, direction: SkipDirection, num: Option<usize>) -> Result<()> {
        // Typical previous skip functionality where if,
        // the track is greater than 1 second into playing,
        // then it goes to the beginning. If triggered again
        // within a second after playing, it will skip to the previous track.
        if direction == SkipDirection::Backward {
            if let Some(current_position) = self.playbin.query_position::<ClockTime>() {
                let one_second = ClockTime::from_seconds(1);

                if current_position > one_second && num.is_none() {
                    debug!("current track position >1s, seeking to start of track");

                    let zero_clock: ClockValue = ClockTime::default().into();

                    self.seek(zero_clock.clone(), None).await?;
                    let mut state = self.state.write().await;
                    state.set_position(zero_clock.clone());

                    return Ok(());
                }
            }
        }

        let mut state = self.state.write().await;
        if let Some(next_track_to_play) = state.skip_track(num, direction.clone()).await {
            if let Some(track_url) = &next_track_to_play.track_url {
                debug!("skipping {direction} to next track");

                self.ready(true).await?;

                self.playbin
                    .set_property("uri", Some(track_url.url.clone()));

                self.set_player_state(state.status().into(), true).await?;
                drop(state);

                self.notify_sender
                    .broadcast(Notification::CurrentTrackList {
                        list: self.state.read().await.track_list(),
                    })
                    .await?;
                self.notify_sender
                    .broadcast(Notification::CurrentTrack {
                        track: next_track_to_play,
                    })
                    .await?;
            }
        }

        Ok(())
    }
    /// Skip to a specific track in the current playlist
    /// by its index in the list.
    #[instrument(skip(self))]
    pub async fn skip_to(&self, index: usize) -> Result<()> {
        let state = self.state.read().await;

        if let Some(current_index) = state.current_track_index() {
            drop(state);
            if index > current_index {
                debug!(
                    "skipping forward from track {} to track {}",
                    current_index, index
                );
                self.skip(SkipDirection::Forward, Some(index)).await?;
            } else {
                debug!(
                    "skipping backward from track {} to track {}",
                    current_index, index
                );
                self.skip(SkipDirection::Backward, Some(index)).await?;
            }
        }

        Ok(())
    }
    /// Skip to a specific track in the current playlist, by the
    /// track id.
    #[instrument(skip(self))]
    pub async fn skip_to_by_id(&self, track_id: usize) -> Result<()> {
        if let Some(track_number) = self.state.read().await.track_index(track_id) {
            self.skip_to(track_number).await?;
        }

        Ok(())
    }
    /// Plays a single track.
    #[instrument(skip(self))]
    pub async fn play_track(&self, track_id: i32, quality: Option<AudioQuality>) -> Result<()> {
        if !self.is_ready() {
            self.ready(true).await?;
        }

        if let (Some(track_list_track), Some(tracklist)) =
            self.state.write().await.play_track(track_id, quality).await
        {
            if let Some(track_url) = &track_list_track.track_url {
                self.playbin
                    .set_property("uri", Some(track_url.url.as_str()));

                if !self.is_playing() {
                    self.play(false).await?;
                }

                self.notify_sender
                    .broadcast(Notification::CurrentTrackList { list: tracklist })
                    .await?;

                self.notify_sender
                    .broadcast(Notification::CurrentTrack {
                        track: track_list_track.clone(),
                    })
                    .await?;
            }
        }

        Ok(())
    }
    /// Plays a full album.
    #[instrument(skip(self))]
    pub async fn play_album(&self, album_id: String, quality: Option<AudioQuality>) -> Result<()> {
        if !self.is_ready() {
            self.ready(true).await?;
        }

        if let (Some(track), Some(tracklist)) =
            self.state.write().await.play_album(album_id, quality).await
        {
            if let Some(track_url) = &track.track_url {
                self.playbin
                    .set_property("uri", Some(track_url.url.clone()));

                if !self.is_playing() {
                    self.play(false).await?;
                }

                self.notify_sender
                    .broadcast(Notification::CurrentTrackList {
                        list: tracklist.clone(),
                    })
                    .await?;

                self.notify_sender
                    .broadcast(Notification::CurrentTrack {
                        track: track.clone(),
                    })
                    .await?;
            }
        }

        Ok(())
    }
    /// Play an item from Qobuz web uri
    #[instrument(skip(self))]
    pub async fn play_uri(&self, uri: String, quality: Option<AudioQuality>) -> Result<()> {
        let quality = if let Some(quality) = quality {
            quality
        } else {
            self.client.quality()
        };

        match client::parse_url(uri.as_str()) {
            Ok(url) => match url {
                UrlType::Album { id } => {
                    self.play_album(id, Some(quality)).await?;
                }
                UrlType::Playlist { id } => {
                    self.play_playlist(id, Some(quality)).await?;
                }
                UrlType::Track { id } => {
                    self.play_track(id, Some(quality)).await?;
                }
            },
            Err(err) => {
                return Err(Error::FailedToPlay {
                    message: format!("Failed to play item. {err}"),
                })
            }
        }

        Ok(())
    }
    /// Plays all tracks in a playlist.
    #[instrument(skip(self))]
    pub async fn play_playlist(
        &self,
        playlist_id: i64,
        quality: Option<AudioQuality>,
    ) -> Result<()> {
        if !self.is_ready() {
            self.ready(true).await?;
        }

        let quality = if let Some(quality) = quality {
            quality
        } else {
            self.client.quality()
        };

        if let (Some(first_track), Some(tracklist)) = self
            .state
            .write()
            .await
            .play_playlist(playlist_id, quality)
            .await
        {
            if let Some(t) = &first_track.track_url {
                self.playbin.set_property("uri", Some(t.url.as_str()));

                if !self.is_playing() {
                    self.play(false).await?;
                }

                self.notify_sender
                    .broadcast(Notification::CurrentTrackList {
                        list: tracklist.clone(),
                    })
                    .await?;

                self.notify_sender
                    .broadcast(Notification::CurrentTrack {
                        track: first_track.clone(),
                    })
                    .await?;
            }
        }

        Ok(())
    }
    /// In response to the about-to-finish signal,
    /// prepare the next track by downloading the stream url.
    #[instrument(skip(self))]
    async fn prep_next_track(&self) -> Result<()> {
        let mut state = self.state.write().await;

        if let Some(next_track) = state.skip_track(None, SkipDirection::Forward).await {
            drop(state);

            debug!("received new track, adding to player");
            if let Some(next_playlist_track_url) = &next_track.track_url {
                self.playbin.set_property("instant-uri", false);
                self.playbin
                    .set_property("uri", Some(next_playlist_track_url.url.clone()));
            }
        } else {
            debug!("no more tracks left");
        }

        Ok(())
    }
    /// Get Gstreamer message stream
    #[instrument(skip(self))]
    pub async fn message_stream(&self) -> BusStream {
        self.playbin.bus().unwrap().stream()
    }
    /// Get a notification channel receiver
    #[instrument(skip(self))]
    pub fn notify_receiver(&self) -> BroadcastReceiver {
        self.notify_receiver.clone()
    }
    /// Consume the player and return a thread/async safe version.
    #[instrument(skip(self))]
    pub fn safe(self) -> SafePlayer {
        Arc::new(RwLock::new(self))
    }
}

/// Inserts the most recent position into the state at a set interval.
#[instrument]
pub async fn clock_loop(safe_player: SafePlayer, safe_state: SafePlayerState) {
    let mut interval = tokio::time::interval(Duration::from_millis(REFRESH_RESOLUTION));
    let mut last_position = ClockValue::default();

    loop {
        interval.tick().await;

        if safe_player.read().await.current_state() == GstState::Playing.into() {
            if let Some(position) = safe_player.read().await.position() {
                if position != last_position {
                    last_position = position.clone();

                    let mut state = safe_state.write().await;
                    state.set_position(position.clone());
                    drop(state);

                    safe_player
                        .read()
                        .await
                        .notify_sender
                        .broadcast(Notification::Position { position })
                        .await
                        .expect("failed to send notification");
                }
            }
        }
    }
}

/// Handles messages from GStreamer, receives player actions from external controls
/// receives the about-to-finish event and takes necessary action.
#[instrument(skip(safe_player, safe_state, client))]
pub async fn player_loop(
    safe_player: SafePlayer,
    client: Client,
    safe_state: SafePlayerState,
) -> Result<()> {
    let p = safe_player.read().await;
    let action_rx = p.controls.action_receiver();
    let mut messages = p.message_stream().await;
    let mut about_to_finish = p.about_to_finish_rx.stream();
    let mut quitter = safe_state.read().await.quitter();
    let mut actions = action_rx.stream();

    let s = safe_state.clone();
    let p = safe_player.clone();
    let clock_handle = tokio::spawn(async { clock_loop(p, s).await });

    loop {
        select! {
            quit = quitter.recv() => {
                match quit {
                    Ok(quit) => {
                        if quit {
                            debug!("quitting player loop, exiting application");

                            clock_handle.abort();

                            let player = safe_player.read().await;

                            if player.is_playing() {
                                debug!("pausing player");
                                player.pause(true).await?;
                            }

                            if player.is_paused() {
                                debug!("readying player");
                                player.ready(true).await?;
                            }


                            if player.is_ready() {
                                debug!("stopping player");
                                player.stop(true).await?;
                            }

                            std::process::exit(0);
                        }
                    },
                    Err(_) => {
                        debug!("quitting player loop, with error");
                        break;
                    },
                }
            }
            Some(almost_done) = about_to_finish.next() => {
                if almost_done {
                    safe_player.read().await.prep_next_track().await?
                }
            }
            Some(action) = actions.next() => {
                let player = safe_player.read().await;

                match action {
                    Action::JumpBackward => player.jump_backward().await?,
                    Action::JumpForward => player.jump_forward().await?,
                    Action::Next => player.skip(SkipDirection::Forward,None).await?,
                    Action::Pause => player.pause(false).await?,
                    Action::Play => player.play(false).await?,
                    Action::PlayPause => player.play_pause().await?,
                    Action::Previous => player.skip(SkipDirection::Backward,None).await?,
                    Action::Stop => player.stop(false).await?,
                    Action::PlayAlbum { album_id } => {
                        player.play_album(album_id, None).await?;
                    },
                    Action::PlayTrack { track_id } => {
                        player.play_track(track_id, None).await?;
                    },
                    Action::PlayUri { uri } => player.play_uri(uri, Some(client.quality())).await?,
                    Action::PlayPlaylist { playlist_id } => {
                        player.play_playlist(playlist_id, Some(client.quality())).await?;
                    },
                    Action::Quit => safe_state.read().await.quit(),
                    Action::SkipTo { num } => player.skip_to(num).await?,
                    Action::SkipToById { track_id } => player.skip_to_by_id(track_id).await?
                }
            }
            Some(msg) = messages.next() => {
                match msg.view() {
                    MessageView::Eos(_) => {
                        debug!("END OF STREAM");
                        let player = safe_player.read().await;

                        if player.quit_when_done {
                            safe_state.read().await.quit();
                        } else {
                            let mut state = safe_state.write().await;
                            state.set_target_status(GstState::Paused);
                            drop(state);

                            player.skip(SkipDirection::Backward, Some(0)).await?;
                        }
                    },
                    MessageView::AsyncDone(msg) => {
                        debug!("ASYNC DONE");
                        let player = safe_player.read().await;

                        let position = if let Some(p)= msg.running_time() {
                            p.into()
                        } else if let Some(p) = player.position() {
                            p
                        } else {
                            ClockTime::default().into()
                        };

                        player.notify_sender.broadcast(Notification::Position { position }).await?;
                    }
                    MessageView::StreamStart(_) => {
                        debug!("stream start");
                        let player = safe_player.read().await;

                        if let Some(current_track) = safe_state.read().await.current_track() {
                            player.notify_sender
                                .broadcast(Notification::CurrentTrack { track: current_track })
                                .await?;
                        }

                        let list = safe_state.read().await.track_list();
                        player.notify_sender.broadcast(Notification::CurrentTrackList{ list }).await?;

                        if let Some(duration) = player.duration() {
                            debug!("setting track duration");
                            let mut state = safe_state.write().await;
                            state.set_duration(duration.clone());
                            drop(state);

                            player.notify_sender.broadcast(Notification::Duration { duration }).await?;
                        }
                    }
                    MessageView::Buffering(buffering) => {
                        if safe_state.read().await.live() {
                            debug!("stream is live, ignore buffering");
                            continue;
                        }
                        let player = safe_player.read().await;
                        let percent = buffering.percent();

                        let target_status = safe_state.read().await.target_status();
                        let is_buffering = safe_state.read().await.buffering();

                        if percent < 100 && !player.is_paused() && !is_buffering {
                            player.pause(false).await?;

                            safe_state.write().await.set_buffering(true);
                        } else if percent > 99 && is_buffering && player.is_paused() {
                            player.set_player_state(target_status.clone().into(), false).await?;
                            safe_state.write().await.set_buffering(false);
                        }

                        if percent.rem_euclid(5) == 0 {
                            debug!("buffering {}%", percent);
                            player.notify_sender.broadcast(Notification::Buffering { is_buffering: percent < 99, target_status, percent }).await?;
                        }
                    }
                    MessageView::StateChanged(state_changed) => {
                        let current_state = state_changed
                            .current()
                            .to_value()
                            .get::<GstState>()
                            .unwrap();

                        let player = safe_player.read().await;
                        let mut state = safe_state.write().await;

                        if state.status() != current_state.into() && state.target_status() == current_state.into() {
                            debug!("player state changed {:?}", current_state);
                            state.set_status(current_state.into());
                            drop(state);

                            player.notify_sender.broadcast(Notification::Status { status: current_state.into() }).await?;
                        }
                    }
                    MessageView::ClockLost(_) => {
                        debug!("clock lost, restarting playback");
                        let player = safe_player.read().await;

                        player.pause(true).await?;
                        player.play(true).await?;
                    }
                    MessageView::Error(err) => {
                        let player = safe_player.read().await;
                        player.notify_sender.broadcast(Notification::Error { error: err.into() }).await?;

                        player.ready(true).await?;
                        player.pause(true).await?;
                        player.play(true).await?;

                        debug!(
                            "Error from {:?}: {} ({:?})",
                            err.src().map(|s| s.path_string()),
                            err.error(),
                            err.debug()
                        );
                    }
                    _ => (),
                }

            }
        }
    }

    Ok(())
}

#[macro_export]
macro_rules! action {
    ($self:ident, $action:expr) => {
        if let Err(_) = $self.action_tx.send_async($action).await {
            error!("error sending action");
        }
    };
}

#[macro_export]
macro_rules! action_blocking {
    ($self:ident, $action:expr) => {
        if let Err(_) = $self.action_tx.send($action) {
            error!("error sending action");
        }
    };
}