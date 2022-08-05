use crate::{
    get_player, mpris,
    qobuz::{client::Client, Album, PlaylistTrack, Track, TrackURL},
    state::{
        app::{AppKey, AppState, PlayerKey},
        AudioQuality, ClockValue, FloatValue, PlaylistValue, StatusValue,
    },
    REFRESH_RESOLUTION,
};
use futures::{executor, prelude::*};
use gst::{glib, ClockTime, Element, MessageView, SeekFlags, State as GstState};
use gstreamer::{self as gst, prelude::*};
use snafu::prelude::*;
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::{select, sync::broadcast::Receiver as BroadcastReceiver, sync::RwLock};

#[derive(Snafu, Debug)]
pub enum Error {
    #[snafu(display("Failed to retrieve a track url."))]
    TrackURL,
    #[snafu(display("Failed to seek."))]
    Seek,
    #[snafu(display("Sorry, could not resume previous session."))]
    Session,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A player handles playing media to a device.
#[derive(Debug, Clone)]
pub struct Player {
    /// Used to broadcast the player state out to other components.
    playbin: Element,
    /// List of tracks that will play.
    playlist: Arc<RwLock<PlaylistValue>>,
    /// List of tracks that have played.
    playlist_previous: Arc<RwLock<PlaylistValue>>,
    /// The app state to save player inforamtion into.
    state: AppState,
    /// Qobuz client
    client: Client,
}

pub fn new(state: AppState, client: Client) -> Player {
    gst::init().expect("Couldn't initialize Gstreamer");
    let playbin = gst::ElementFactory::make("playbin", None).unwrap();

    Player {
        playbin,
        playlist: Arc::new(RwLock::new(PlaylistValue::new())),
        playlist_previous: Arc::new(RwLock::new(PlaylistValue::new())),
        state,
        client,
    }
}

impl Player {
    /// Retreive the current app state from the player.
    pub fn app_state(&self) -> AppState {
        self.clone().state
    }
    /// Retreive the active playlist.
    pub fn playlist(&self) -> Arc<RwLock<PlaylistValue>> {
        self.playlist.clone()
    }
    /// Set the active playlist.
    pub fn set_playlist(&mut self, playlist: PlaylistValue) {
        self.playlist = Arc::new(RwLock::new(playlist));
    }
    /// Set the previous playlist.
    pub fn set_prev_playlist(&mut self, playlist: PlaylistValue) {
        self.playlist_previous = Arc::new(RwLock::new(playlist));
    }
    /// Play the player.
    pub fn play(&self) {
        self.playbin
            .set_state(gst::State::Playing)
            .expect("Unable to set the pipeline to the `Playing` state");
    }
    /// Pause the player.
    pub fn pause(&self) {
        self.playbin
            .set_state(gst::State::Paused)
            .expect("Unable to set the pipeline to the `Paused` state");
    }
    /// Toggle play and pause.
    pub fn play_pause(&self) {
        if self.is_playing() {
            self.pause();
        } else if self.is_paused() {
            self.play()
        }
    }
    /// Ready the player.
    pub fn ready(&self) {
        self.playbin
            .set_state(gst::State::Ready)
            .expect("Unable to set the pipeline to the `Ready` state");
    }
    /// Stop the player.
    pub fn stop(&self) {
        self.playbin
            .set_state(gst::State::Null)
            .expect("Unable to set the pipeline to the `Null` state");
    }
    /// Set the uri of the track to play.
    pub fn set_uri(&self, track_url: TrackURL) {
        self.playbin
            .set_property("uri", Some(track_url.url.as_str()));
    }
    /// Is the player paused?
    pub fn is_paused(&self) -> bool {
        self.playbin.current_state() == gst::State::Paused
    }
    /// Is the player playing?
    pub fn is_playing(&self) -> bool {
        self.playbin.current_state() == gst::State::Playing
    }
    /// Retreive the current Gstreamer state of the player.
    pub fn current_state(&self) -> GstState {
        self.playbin.current_state()
    }
    /// Retreive the current position from the player.
    pub fn position(&self) -> ClockTime {
        if let Some(position) = self.playbin.query_position::<ClockTime>() {
            position
        } else {
            ClockTime::default()
        }
    }
    /// Seek to a specified time in the current track.
    pub fn seek(&self, time: ClockValue, flags: Option<SeekFlags>) -> Result<()> {
        let flags = if let Some(flags) = flags {
            flags
        } else {
            SeekFlags::FLUSH | SeekFlags::KEY_UNIT
        };

        match self.playbin.seek_simple(flags, time.inner_clocktime()) {
            Ok(_) => {
                self.state
                    .player
                    .insert::<String, ClockValue>(AppKey::Player(PlayerKey::Position), time);

                Ok(())
            }
            Err(error) => {
                error!("{}", error.message);
                Err(Error::Seek)
            }
        }
    }
    /// Retreive the current track's duration from the player.
    pub fn duration(&self) -> ClockTime {
        if let Some(duration) = self.playbin.query_duration::<ClockTime>() {
            duration
        } else {
            ClockTime::default()
        }
    }
    /// Jump forward in the currently playing track +10 seconds.
    pub fn jump_forward(&self) {
        if let Some(current_position) = self.playbin.query_position::<ClockTime>() {
            if let Some(duration) = self.playbin.query_duration::<ClockTime>() {
                let ten_seconds = ClockTime::from_seconds(10);
                let next_position = current_position + ten_seconds;

                if next_position < duration {
                    match self.seek(next_position.into(), None) {
                        Ok(_) => (),
                        Err(error) => {
                            error!("{:?}", error);
                        }
                    }
                } else {
                    match self.seek(duration.into(), None) {
                        Ok(_) => (),
                        Err(error) => {
                            error!("{:?}", error);
                        }
                    }
                }
            }
        }
    }
    /// Jump forward in the currently playing track +10 seconds.
    pub fn jump_backward(&self) {
        if let Some(current_position) = self.playbin.query_position::<ClockTime>() {
            if current_position.seconds() < 10 {
                match self.seek(ClockTime::default().into(), None) {
                    Ok(_) => self.app_state().player.insert::<String, ClockValue>(
                        AppKey::Player(PlayerKey::Position),
                        ClockTime::default().into(),
                    ),
                    Err(error) => {
                        error!("{:?}", error);
                        self.app_state().player.insert::<String, ClockValue>(
                            AppKey::Player(PlayerKey::Position),
                            current_position.into(),
                        )
                    }
                }
            } else {
                let ten_seconds = ClockTime::from_seconds(10);
                let seek_position = current_position - ten_seconds;
                match self.seek(seek_position.into(), None) {
                    Ok(_) => self.app_state().player.insert::<String, ClockValue>(
                        AppKey::Player(PlayerKey::Position),
                        seek_position.into(),
                    ),
                    Err(error) => {
                        error!("{:?}", error);
                        self.app_state().player.insert::<String, ClockValue>(
                            AppKey::Player(PlayerKey::Position),
                            current_position.into(),
                        )
                    }
                }
            }
        }
    }
    /// Skip forward to the next track in the playlist.
    pub async fn skip_forward(&self, num: Option<usize>) -> Result<()> {
        let tree = self.state.player.clone();

        let mut playlist = self.playlist.write().await;
        let mut prev_playlist = self.playlist_previous.write().await;

        if let Some(previous_track) = get_player!(PlayerKey::NextUp, tree, PlaylistTrack) {
            prev_playlist.push_back(previous_track);
        }

        if let Some(number) = num {
            // Grab all of the tracks, up to the next one to play.
            prev_playlist.append(
                playlist
                    .drain(..number)
                    .collect::<VecDeque<PlaylistTrack>>(),
            );
        }

        if let Some(mut next_track_to_play) = playlist.pop_front() {
            debug!("fetching url for next track");

            next_track_to_play = self.attach_track_url(next_track_to_play).await?;

            if let Some(track_url) = next_track_to_play.clone().track_url {
                debug!("skipping forward to next track");
                self.ready();

                self.state.player.insert::<String, PlaylistTrack>(
                    AppKey::Player(PlayerKey::NextUp),
                    next_track_to_play,
                );

                self.state.player.insert::<String, PlaylistValue>(
                    AppKey::Player(PlayerKey::Playlist),
                    playlist.clone(),
                );

                self.state.player.insert::<String, PlaylistValue>(
                    AppKey::Player(PlayerKey::PreviousPlaylist),
                    prev_playlist.clone(),
                );

                self.playbin.set_property("uri", Some(track_url.url));
                self.play();
            }
        }
        Ok(())
    }
    /// Skip backwards by playing the first track in previous track playlist.
    pub async fn skip_backward(&self, num: Option<usize>) -> Result<()> {
        // If the track is greater than 1 second into playing,
        // then we just want to go back to the beginning.
        // If triggered again within a second after playing,
        // it will skip to the previous track.
        if let Some(current_position) = self.playbin.query_position::<ClockTime>() {
            let one_second = ClockTime::from_seconds(1);

            if current_position > one_second && num.is_none() {
                debug!("current track position >1s, seeking to start of track");
                self.seek(ClockTime::default().into(), None)
                    .expect("failed to seek");

                return Ok(());
            }
        }

        let mut prev_playlist = self.playlist_previous.write().await;
        let mut playlist = self.playlist.write().await;
        let tree = self.state.player.clone();

        if let Some(previously_played_track) = get_player!(PlayerKey::NextUp, tree, PlaylistTrack) {
            playlist.push_front(previously_played_track);
        }

        if let Some(number) = num {
            // Grab all of the tracks, up to the next one to play,
            // inlcuding the currently playing track from above.
            let diff = number + 1 - playlist.len();
            let skipped_tracks = prev_playlist
                .drain(diff + 1..)
                .rev()
                .collect::<VecDeque<PlaylistTrack>>();

            for track in skipped_tracks {
                playlist.push_front(track);
            }
        }

        if let Some(mut next_track_to_play) = prev_playlist.pop_back() {
            next_track_to_play = self.attach_track_url(next_track_to_play).await?;

            if let Some(track_url) = next_track_to_play.clone().track_url {
                debug!("skipping backward to previous track");
                self.ready();

                self.state.player.insert::<String, PlaylistTrack>(
                    AppKey::Player(PlayerKey::NextUp),
                    next_track_to_play,
                );

                self.state.player.insert::<String, PlaylistValue>(
                    AppKey::Player(PlayerKey::Playlist),
                    playlist.clone(),
                );
                self.state.player.insert::<String, PlaylistValue>(
                    AppKey::Player(PlayerKey::PreviousPlaylist),
                    prev_playlist.clone(),
                );

                self.playbin.set_property("uri", Some(track_url.url));
                self.play();
            }
        }

        Ok(())
    }
    /// Skip to a specific track number in the current playlist.
    pub async fn skip_to(&self, track_number: usize) -> Result<()> {
        if track_number < self.playlist().read().await.len() {
            debug!("skipping forward to track number {}", track_number);
            self.skip_forward(Some(track_number)).await
        } else {
            debug!("skipping backward to track number {}", track_number);
            self.skip_backward(Some(track_number)).await
        }
    }
    /// Plays a single track.
    pub async fn play_track(&self, track: Track, quality: AudioQuality) {
        let playlist_track = PlaylistTrack::new(track, Some(quality.clone()), None);
        self.playlist.write().await.push_back(playlist_track);
        self.start(quality).await;
    }
    /// Plays a full album.
    pub async fn play_album(&self, album: Album, quality: AudioQuality) {
        if let Some(tracklist) = album.to_playlist_tracklist(quality.clone()) {
            debug!("creating playlist");
            for playlist_track in tracklist {
                self.playlist.write().await.push_back(playlist_track);
            }

            self.start(quality).await;
        }
    }
    /// Inserts the most recent position, duration and progress values into the state
    /// at a set interval.
    fn clock_loop(&self, mut quit_receiver: BroadcastReceiver<bool>) {
        loop {
            if let Ok(quit) = quit_receiver.try_recv() {
                if quit {
                    debug!("quitting");
                    break;
                }
            }
            if self.playbin.current_state() != GstState::VoidPending
                || self.playbin.current_state() != GstState::Null
            {
                let pos: Option<ClockTime> = self.playbin.query_position();
                let dur: Option<ClockTime> = self.playbin.query_duration();
                let state = self.state.clone();

                if let Some(position) = pos {
                    state.player.insert::<String, ClockValue>(
                        AppKey::Player(PlayerKey::Position),
                        position.into(),
                    );

                    if let Some(duration) = dur {
                        state.player.insert::<String, ClockValue>(
                            AppKey::Player(PlayerKey::Duration),
                            duration.into(),
                        );

                        if position >= ClockTime::from_seconds(0) && position <= duration {
                            let remaining = duration - position;
                            let progress = position.seconds() as f64 / duration.seconds() as f64;
                            state.player.insert::<String, FloatValue>(
                                AppKey::Player(PlayerKey::Progress),
                                progress.into(),
                            );
                            state.player.insert::<String, ClockValue>(
                                AppKey::Player(PlayerKey::DurationRemaining),
                                remaining.into(),
                            );
                        }
                    }
                }

                std::thread::sleep(Duration::from_millis(REFRESH_RESOLUTION));
            }
        }
    }
    /// Stats the player.
    async fn start(&self, quality: AudioQuality) {
        let mut next_track = match self.playlist.write().await.pop_front() {
            Some(it) => it,
            _ => return,
        };
        let playbin = &self.playbin;

        if let Ok(track_url) = self
            .client
            .track_url(next_track.track.id, Some(quality.clone()), None)
            .await
        {
            playbin.set_property("uri", Some(track_url.url.as_str()));
            next_track.set_track_url(track_url);

            self.state.player.insert::<String, PlaylistTrack>(
                AppKey::Player(PlayerKey::NextUp),
                next_track.clone(),
            );

            self.state.player.insert::<String, PlaylistValue>(
                AppKey::Player(PlayerKey::Playlist),
                self.playlist.read().await.clone(),
            );

            self.play();

            self.state.player.insert::<String, StatusValue>(
                AppKey::Player(PlayerKey::Status),
                gst::State::Playing.into(),
            );
        }
    }
    /// Sets up basic functionality for the player.
    pub async fn setup(&self, resume: bool) {
        mpris::init(self.clone()).await;

        let cloned_self = self.clone();
        let quitter = self.app_state().quitter();
        std::thread::spawn(move || {
            cloned_self.clock_loop(quitter);
        });

        let mut cloned_self = self.clone();
        tokio::spawn(async move {
            cloned_self.player_loop(resume).await;
        });

        let playlist = self.playlist.clone();
        let prev_playlist = self.playlist_previous.clone();
        let state = self.state.clone();

        // Connects to the `about-to-finish` signal so the player
        // can setup the next track to play. Enables gapless playback.
        let outer_client = self.client.clone();
        let player_tree = self.state.player.clone();
        self.playbin
            .connect("about-to-finish", false, move |values| {
                debug!("about to finish");
                let client = outer_client.clone();
                let cloned_state = state.clone();
                let playbin = values[0]
                    .get::<glib::Object>()
                    .expect("playbin \"about-to-finish\" signal values[0]");

                if let Some(next_track) = executor::block_on(playlist.write()).pop_front() {
                    debug!("received new track, adding to player");
                    if let Ok(next_playlist_track_url) =
                        executor::block_on(client.track_url(next_track.track.id, None, None))
                    {
                        if let Some(previous_track) =
                            get_player!(PlayerKey::NextUp, player_tree, PlaylistTrack)
                        {
                            executor::block_on(prev_playlist.write()).push_back(previous_track);
                        }
                        cloned_state.player.insert::<String, PlaylistTrack>(
                            AppKey::Player(PlayerKey::NextUp),
                            next_track,
                        );
                        playbin.set_property("uri", Some(next_playlist_track_url.url));
                    }
                }

                None
            });
    }
    /// Attach a `TrackURL` to the given track.
    pub async fn attach_track_url(&self, mut track: PlaylistTrack) -> Result<PlaylistTrack> {
        if let Ok(track_url) = self.client.track_url(track.track.id, None, None).await {
            Ok(track.set_track_url(track_url))
        } else {
            Err(Error::TrackURL)
        }
    }
    /// Handles messages from the player and takes necessary action.
    async fn player_loop(&mut self, mut resume: bool) {
        let mut messages = self.playbin.bus().unwrap().stream();
        let mut quitter = self.state.quitter();

        loop {
            select! {
                Ok(quit) = quitter.recv() => {
                    if quit {
                        debug!("quitting");
                        break;
                    }
                }
                Some(msg) = messages.next() => {
                    match msg.view() {
                        MessageView::Eos(_) => {
                            debug!("END OF STREAM");

                            self.stop();
                            self.state.quit();
                            break;
                        },
                        MessageView::StreamStart(_) => {
                            let state = &mut self.state;
                            let tree = state.player.clone();

                            // When a stream starts, add the new track duration
                            // from the player to the state.
                            if let Some(next_track) = get_player!(PlayerKey::NextUp, tree, PlaylistTrack) {
                               state.player.insert::<String, ClockValue>(AppKey::Player(PlayerKey::Duration),ClockTime::from_seconds(next_track.track.duration.try_into().unwrap()).into());
                            }
                        }
                        MessageView::AsyncDone(_) => {
                            // If the player is resuming from a previous session,
                            // seek to the last position saved to the state.
                            if resume {
                                let state = &mut self.state;
                                let tree = state.player.clone();

                                if let Some(position) = get_player!(PlayerKey::Position, tree, ClockValue) {
                                    resume = false;
                                    self.seek(position, None).expect("seek failure");

                                }
                            }

                        }
                        MessageView::StateChanged(state_changed) => {
                            if state_changed
                                .src()
                                .map(|s| s == self.playbin)
                                .unwrap_or(false)
                            {
                                let current_state = state_changed
                                    .current()
                                    .to_value()
                                    .get::<GstState>()
                                    .unwrap();

                                match current_state {
                                    GstState::Playing => {
                                        debug!("player state changed to Playing");
                                        self.state.player.insert::<String, StatusValue>(AppKey::Player(PlayerKey::Status),GstState::Playing.into());
                                    }
                                    GstState::Paused => {
                                        debug!("player state changed to Paused");
                                        self.state.player.insert::<String, StatusValue>(AppKey::Player(PlayerKey::Status),GstState::Paused.into());
                                    }
                                    GstState::Ready => {
                                        debug!("player state changed to Ready");
                                        self.state.player.insert::<String, StatusValue>(AppKey::Player(PlayerKey::Status),GstState::Ready.into());
                                    }
                                    _ => (),
                                }
                            }
                        }
                        MessageView::Error(err) => {
                            println!(
                                "Error from {:?}: {} ({:?})",
                                err.src().map(|s| s.path_string()),
                                err.error(),
                                err.debug()
                            );
                            break;
                        }
                        _ => (),
                    }

                }
            }
        }
    }
}
