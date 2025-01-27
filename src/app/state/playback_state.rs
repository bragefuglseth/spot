use std::borrow::Cow;
use std::time::Instant;

use crate::app::models::*;
use crate::app::state::{AppAction, AppEvent, UpdatableState};
use crate::app::{BatchQuery, LazyRandomIndex, SongsSource};

#[derive(Debug)]
pub struct PlaybackState {
    available_devices: Vec<ConnectDevice>,
    current_device: Device,
    index: LazyRandomIndex,
    songs: SongListModel,
    list_position: Option<usize>,
    seek_position: PositionMillis,
    source: Option<SongsSource>,
    repeat: RepeatMode,
    is_playing: bool,
    is_shuffled: bool,
}

impl PlaybackState {
    pub fn songs(&self) -> &SongListModel {
        &self.songs
    }

    pub fn is_playing(&self) -> bool {
        self.is_playing && self.list_position.is_some()
    }

    pub fn is_shuffled(&self) -> bool {
        self.is_shuffled
    }

    pub fn repeat_mode(&self) -> RepeatMode {
        self.repeat
    }

    pub fn next_query(&self) -> Option<BatchQuery> {
        let next_index = self.next_index()?;
        let next_index = if self.is_shuffled {
            self.index.get(next_index)?
        } else {
            next_index
        };
        let batch = self.songs.needed_batch_for(next_index);
        if let Some(batch) = batch {
            let source = self.source.as_ref().cloned()?;
            Some(BatchQuery { source, batch })
        } else {
            None
        }
    }

    fn index(&self, i: usize) -> Option<SongDescription> {
        let song = if self.is_shuffled {
            self.songs.index(self.index.get(i)?)
        } else {
            self.songs.index(i)
        };
        Some(song?.into_description())
    }

    pub fn current_source(&self) -> Option<&SongsSource> {
        self.source.as_ref()
    }

    pub fn current_song_index(&self) -> Option<usize> {
        self.list_position
    }

    pub fn current_song_id(&self) -> Option<String> {
        Some(self.index(self.list_position?)?.id)
    }

    pub fn current_song(&self) -> Option<SongDescription> {
        self.index(self.list_position?)
    }

    fn next_id(&self) -> Option<String> {
        self.next_index()
            .and_then(|i| Some(self.songs().index(i)?.description().id.clone()))
    }

    fn clear(&mut self, source: Option<SongsSource>) -> SongListModelPending {
        self.source = source;
        self.index = Default::default();
        self.list_position = None;
        self.songs.clear()
    }

    fn set_batch(&mut self, source: Option<SongsSource>, song_batch: SongBatch) -> bool {
        let ok = self.clear(source).and(|s| s.add(song_batch)).commit();
        self.index.resize(self.songs.len());
        ok
    }

    fn add_batch(&mut self, song_batch: SongBatch) -> bool {
        let ok = self.songs.add(song_batch).commit();
        self.index.resize(self.songs.len());
        ok
    }

    pub fn set_queue(&mut self, tracks: Vec<SongDescription>) {
        self.clear(None).and(|s| s.append(tracks)).commit();
        self.index.grow(self.songs.len());
    }

    pub fn queue(&mut self, tracks: Vec<SongDescription>) {
        self.source = None;
        self.songs.append(tracks).commit();
        self.index.grow(self.songs.len());
    }

    pub fn dequeue(&mut self, ids: &[String]) {
        let current_id = self.current_song_id();
        self.songs.remove(ids).commit();
        self.list_position = current_id.and_then(|id| self.songs.find_index(&id));
        self.index.shrink(self.songs.len());
    }

    fn swap_pos(&mut self, index: usize, other_index: usize) {
        let len = self.songs.len();
        self.list_position = self
            .list_position
            .map(|position| match position {
                i if i == index => other_index,
                i if i == other_index => index,
                _ => position,
            })
            .map(|p| usize::min(p, len - 1))
    }

    pub fn move_down(&mut self, id: &str) -> Option<usize> {
        let index = self.songs.find_index(id)?;
        self.songs.move_down(index).commit();
        self.swap_pos(index + 1, index);
        Some(index)
    }

    pub fn move_up(&mut self, id: &str) -> Option<usize> {
        let index = self.songs.find_index(id).filter(|&index| index > 0)?;
        self.songs.move_up(index).commit();
        self.swap_pos(index - 1, index);
        Some(index)
    }

    fn play(&mut self, id: &str) -> bool {
        if self.current_song_id().map(|cur| cur == id).unwrap_or(false) {
            return false;
        }

        let found_index = self.songs.find_index(id);

        if let Some(index) = found_index {
            if self.is_shuffled {
                self.index.reset_picking_first(index);
                self.play_index(0);
            } else {
                self.play_index(index);
            }
            true
        } else {
            false
        }
    }

    fn stop(&mut self) {
        self.list_position = None;
        self.is_playing = false;
        self.seek_position.set(0, false);
    }

    fn play_index(&mut self, index: usize) -> Option<String> {
        self.is_playing = true;
        self.list_position.replace(index);
        self.seek_position.set(0, true);
        self.index.next_until(index + 1);
        self.current_song_id()
    }

    fn play_next(&mut self) -> Option<String> {
        self.next_index().and_then(move |i| {
            self.seek_position.set(0, true);
            self.play_index(i)
        })
    }

    pub fn next_index(&self) -> Option<usize> {
        let len = self.songs.len();
        self.list_position.and_then(|p| match self.repeat {
            RepeatMode::Song => Some(p),
            RepeatMode::Playlist if len != 0 => Some((p + 1) % len),
            RepeatMode::None => Some(p + 1).filter(|&i| i < len),
            _ => None,
        })
    }

    fn play_prev(&mut self) -> Option<String> {
        self.prev_index().and_then(move |i| {
            // Only jump to the previous track if we aren't more than 2 seconds (2,000 ms) into the current track.
            // Otherwise, seek to the start of the current track.
            // (This replicates the behavior of official Spotify clients.)
            if self.seek_position.current() <= 2000 {
                self.seek_position.set(0, true);
                self.play_index(i)
            } else {
                self.seek_position.set(0, true);
                None
            }
        })
    }

    pub fn prev_index(&self) -> Option<usize> {
        let len = self.songs.len();
        self.list_position.and_then(|p| match self.repeat {
            RepeatMode::Song => Some(p),
            RepeatMode::Playlist if len != 0 => Some((if p == 0 { len } else { p }) - 1),
            RepeatMode::None => Some(p).filter(|&i| i > 0).map(|i| i - 1),
            _ => None,
        })
    }

    fn toggle_play(&mut self) -> Option<bool> {
        if self.list_position.is_some() {
            self.is_playing = !self.is_playing;

            match self.is_playing {
                false => self.seek_position.pause(),
                true => self.seek_position.resume(),
            };

            Some(self.is_playing)
        } else {
            None
        }
    }

    fn set_shuffled(&mut self, shuffled: bool) {
        self.is_shuffled = shuffled;
        let old = self.list_position.replace(0).unwrap_or(0);
        self.index.reset_picking_first(old);
    }

    pub fn available_devices(&self) -> &Vec<ConnectDevice> {
        &self.available_devices
    }

    pub fn current_device(&self) -> &Device {
        &self.current_device
    }
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            available_devices: vec![],
            current_device: Device::Local,
            index: LazyRandomIndex::default(),
            songs: SongListModel::new(50),
            list_position: None,
            seek_position: PositionMillis::new(1.0),
            source: None,
            repeat: RepeatMode::None,
            is_playing: false,
            is_shuffled: false,
        }
    }
}

#[derive(Clone, Debug)]
pub enum PlaybackAction {
    TogglePlay,
    Play,
    Pause,
    Stop,
    SetRepeatMode(RepeatMode),
    SetShuffled(bool),
    ToggleRepeat,
    ToggleShuffle,
    Seek(u32),
    SyncSeek(u32),
    Load(String),
    LoadSongs(Vec<SongDescription>),
    LoadPagedSongs(SongsSource, SongBatch),
    SetVolume(f64),
    Next,
    Previous,
    Preload,
    Queue(Vec<SongDescription>),
    Dequeue(String),
    SwitchDevice(Device),
    SetAvailableDevices(Vec<ConnectDevice>),
}

impl From<PlaybackAction> for AppAction {
    fn from(playback_action: PlaybackAction) -> Self {
        Self::PlaybackAction(playback_action)
    }
}

#[derive(Clone, Debug)]
pub enum Device {
    Local,
    Connect(ConnectDevice),
}

#[derive(Clone, Debug)]
pub enum PlaybackEvent {
    PlaybackPaused,
    PlaybackResumed,
    RepeatModeChanged(RepeatMode),
    TrackSeeked(u32),
    SeekSynced(u32),
    VolumeSet(f64),
    TrackChanged(String),
    SourceChanged,
    Preload(String),
    ShuffleChanged(bool),
    PlaylistChanged,
    PlaybackStopped,
    SwitchedDevice(Device),
    AvailableDevicesChanged,
}

impl From<PlaybackEvent> for AppEvent {
    fn from(playback_event: PlaybackEvent) -> Self {
        Self::PlaybackEvent(playback_event)
    }
}

impl UpdatableState for PlaybackState {
    type Action = PlaybackAction;
    type Event = PlaybackEvent;

    fn update_with(&mut self, action: Cow<Self::Action>) -> Vec<Self::Event> {
        match action.into_owned() {
            PlaybackAction::TogglePlay => {
                if let Some(playing) = self.toggle_play() {
                    if playing {
                        vec![PlaybackEvent::PlaybackResumed]
                    } else {
                        vec![PlaybackEvent::PlaybackPaused]
                    }
                } else {
                    vec![]
                }
            }
            PlaybackAction::Play => {
                if !self.is_playing() && self.toggle_play() == Some(true) {
                    vec![PlaybackEvent::PlaybackResumed]
                } else {
                    vec![]
                }
            }
            PlaybackAction::Pause => {
                if self.is_playing() && self.toggle_play() == Some(false) {
                    vec![PlaybackEvent::PlaybackPaused]
                } else {
                    vec![]
                }
            }
            PlaybackAction::ToggleRepeat => {
                self.repeat = match self.repeat {
                    RepeatMode::Song => RepeatMode::None,
                    RepeatMode::Playlist => RepeatMode::Song,
                    RepeatMode::None => RepeatMode::Playlist,
                };
                vec![PlaybackEvent::RepeatModeChanged(self.repeat)]
            }
            PlaybackAction::SetRepeatMode(mode) if self.repeat != mode => {
                self.repeat = mode;
                vec![PlaybackEvent::RepeatModeChanged(self.repeat)]
            }
            PlaybackAction::SetShuffled(shuffled) if self.is_shuffled != shuffled => {
                self.set_shuffled(shuffled);
                vec![PlaybackEvent::ShuffleChanged(shuffled)]
            }
            PlaybackAction::ToggleShuffle => {
                self.set_shuffled(!self.is_shuffled);
                vec![PlaybackEvent::ShuffleChanged(self.is_shuffled)]
            }
            PlaybackAction::Next => {
                if let Some(id) = self.play_next() {
                    vec![
                        PlaybackEvent::TrackChanged(id),
                        PlaybackEvent::PlaybackResumed,
                    ]
                } else {
                    self.stop();
                    vec![PlaybackEvent::PlaybackStopped]
                }
            }
            PlaybackAction::Stop => {
                self.stop();
                vec![PlaybackEvent::PlaybackStopped]
            }
            PlaybackAction::Previous => {
                if let Some(id) = self.play_prev() {
                    vec![
                        PlaybackEvent::TrackChanged(id),
                        PlaybackEvent::PlaybackResumed,
                    ]
                } else {
                    vec![PlaybackEvent::TrackSeeked(0)]
                }
            }
            PlaybackAction::Load(id) => {
                if self.play(&id) {
                    vec![
                        PlaybackEvent::TrackChanged(id),
                        PlaybackEvent::PlaybackResumed,
                    ]
                } else {
                    vec![]
                }
            }
            PlaybackAction::Preload => {
                if let Some(id) = self.next_id() {
                    vec![PlaybackEvent::Preload(id)]
                } else {
                    vec![]
                }
            }
            PlaybackAction::LoadPagedSongs(source, batch)
                if Some(&source) == self.source.as_ref() =>
            {
                if self.add_batch(batch) {
                    vec![PlaybackEvent::PlaylistChanged]
                } else {
                    vec![]
                }
            }
            PlaybackAction::LoadPagedSongs(source, batch)
                if Some(&source) != self.source.as_ref() =>
            {
                debug!("new source: {:?}", &source);
                self.set_batch(Some(source), batch);
                vec![PlaybackEvent::PlaylistChanged, PlaybackEvent::SourceChanged]
            }
            PlaybackAction::LoadSongs(tracks) => {
                self.set_queue(tracks);
                vec![PlaybackEvent::PlaylistChanged, PlaybackEvent::SourceChanged]
            }
            PlaybackAction::Queue(tracks) => {
                self.queue(tracks);
                vec![PlaybackEvent::PlaylistChanged]
            }
            PlaybackAction::Dequeue(id) => {
                self.dequeue(&[id]);
                vec![PlaybackEvent::PlaylistChanged]
            }
            PlaybackAction::Seek(pos) => {
                self.seek_position.set(pos as u64 * 1000, true);
                vec![PlaybackEvent::TrackSeeked(pos)]
            }
            PlaybackAction::SyncSeek(pos) => {
                self.seek_position.set(pos as u64 * 1000, true);
                vec![PlaybackEvent::SeekSynced(pos)]
            }
            PlaybackAction::SetVolume(volume) => vec![PlaybackEvent::VolumeSet(volume)],
            PlaybackAction::SetAvailableDevices(list) => {
                self.available_devices = list;
                vec![PlaybackEvent::AvailableDevicesChanged]
            }
            PlaybackAction::SwitchDevice(new_device) => {
                self.current_device = new_device.clone();
                vec![PlaybackEvent::SwitchedDevice(new_device)]
            }
            _ => vec![],
        }
    }
}

#[derive(Debug)]
struct PositionMillis {
    last_known_position: u64,
    last_resume_instant: Option<Instant>,
    rate: f32,
}

impl PositionMillis {
    fn new(rate: f32) -> Self {
        Self {
            last_known_position: 0,
            last_resume_instant: None,
            rate,
        }
    }

    fn current(&self) -> u64 {
        let current_progress = self.last_resume_instant.map(|ri| {
            let elapsed = ri.elapsed().as_millis() as f32;
            let real_elapsed = self.rate * elapsed;
            real_elapsed.ceil() as u64
        });
        self.last_known_position + current_progress.unwrap_or(0)
    }

    fn set(&mut self, position: u64, playing: bool) {
        self.last_known_position = position;
        self.last_resume_instant = if playing { Some(Instant::now()) } else { None }
    }

    fn pause(&mut self) {
        self.last_known_position = self.current();
        self.last_resume_instant = None;
    }

    fn resume(&mut self) {
        self.last_resume_instant = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::app::models::AlbumRef;

    fn song(id: &str) -> SongDescription {
        SongDescription {
            id: id.to_string(),
            uri: "".to_string(),
            title: "Title".to_string(),
            artists: vec![],
            album: AlbumRef {
                id: "".to_string(),
                name: "".to_string(),
            },
            duration: 1000,
            art: None,
            track_number: None,
        }
    }

    impl PlaybackState {
        fn current_position(&self) -> Option<usize> {
            self.list_position
        }

        fn prev_id(&self) -> Option<String> {
            self.prev_index()
                .and_then(|i| Some(self.songs().index(i)?.description().id.clone()))
        }

        fn song_ids(&self) -> Vec<String> {
            self.songs()
                .collect()
                .iter()
                .map(|s| s.id.clone())
                .collect()
        }
    }

    #[test]
    fn test_initial_state() {
        let state = PlaybackState::default();
        assert!(!state.is_playing());
        assert!(!state.is_shuffled());
        assert!(state.current_song().is_none());
        assert!(state.prev_index().is_none());
        assert!(state.next_index().is_none());
    }

    #[test]
    fn test_play_one() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("foo")]);

        state.play("foo");
        assert!(state.is_playing());

        assert_eq!(state.current_song_id(), Some("foo".to_string()));
        assert!(state.prev_index().is_none());
        assert!(state.next_index().is_none());

        state.toggle_play();
        assert!(!state.is_playing());
    }

    #[test]
    fn test_queue() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("1"), song("2"), song("3")]);

        assert_eq!(state.songs().len(), 3);

        state.play("2");

        state.queue(vec![song("4")]);
        assert_eq!(state.songs().len(), 4);
    }

    #[test]
    fn test_play_multiple() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("1"), song("2"), song("3")]);
        assert_eq!(state.songs().len(), 3);

        state.play("2");
        assert!(state.is_playing());

        assert_eq!(state.current_position(), Some(1));
        assert_eq!(state.prev_id(), Some("1".to_string()));
        assert_eq!(state.current_song_id(), Some("2".to_string()));
        assert_eq!(state.next_id(), Some("3".to_string()));

        state.toggle_play();
        assert!(!state.is_playing());

        state.play_next();
        assert!(state.is_playing());
        assert_eq!(state.current_position(), Some(2));
        assert_eq!(state.prev_id(), Some("2".to_string()));
        assert_eq!(state.current_song_id(), Some("3".to_string()));
        assert!(state.next_index().is_none());

        state.play_next();
        assert!(state.is_playing());
        assert_eq!(state.current_position(), Some(2));
        assert_eq!(state.current_song_id(), Some("3".to_string()));

        state.play_prev();
        state.play_prev();
        assert!(state.is_playing());
        assert_eq!(state.current_position(), Some(0));
        assert!(state.prev_index().is_none());
        assert_eq!(state.current_song_id(), Some("1".to_string()));
        assert_eq!(state.next_id(), Some("2".to_string()));

        state.play_prev();
        assert!(state.is_playing());
        assert_eq!(state.current_position(), Some(0));
        assert_eq!(state.current_song_id(), Some("1".to_string()));
    }

    #[test]
    fn test_shuffle() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("1"), song("2"), song("3"), song("4")]);

        assert_eq!(state.songs().len(), 4);

        state.play("2");
        assert_eq!(state.current_position(), Some(1));

        state.set_shuffled(true);
        assert!(state.is_shuffled());
        assert_eq!(state.current_position(), Some(0));

        state.play_next();
        assert_eq!(state.current_position(), Some(1));

        state.set_shuffled(false);
        assert!(!state.is_shuffled());

        let ids = state.song_ids();
        assert_eq!(
            ids,
            vec![
                "1".to_string(),
                "2".to_string(),
                "3".to_string(),
                "4".to_string()
            ]
        );
    }

    #[test]
    fn test_shuffle_queue() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("1"), song("2"), song("3")]);

        state.set_shuffled(true);
        assert!(state.is_shuffled());

        state.queue(vec![song("4")]);

        state.set_shuffled(false);
        assert!(!state.is_shuffled());

        let ids = state.song_ids();
        assert_eq!(
            ids,
            vec![
                "1".to_string(),
                "2".to_string(),
                "3".to_string(),
                "4".to_string()
            ]
        );
    }

    #[test]
    fn test_move() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("1"), song("2"), song("3")]);

        state.play("2");
        assert!(state.is_playing());

        state.move_down("1");
        assert_eq!(state.current_song_id(), Some("2".to_string()));
        let ids = state.song_ids();
        assert_eq!(ids, vec!["2".to_string(), "1".to_string(), "3".to_string()]);

        state.move_down("2");
        state.move_down("2");
        assert_eq!(state.current_song_id(), Some("2".to_string()));
        let ids = state.song_ids();
        assert_eq!(ids, vec!["1".to_string(), "3".to_string(), "2".to_string()]);

        state.move_down("2");
        assert_eq!(state.current_song_id(), Some("2".to_string()));
        let ids = state.song_ids();
        assert_eq!(ids, vec!["1".to_string(), "3".to_string(), "2".to_string()]);

        state.move_up("2");

        assert_eq!(state.current_song_id(), Some("2".to_string()));
        let ids = state.song_ids();
        assert_eq!(ids, vec!["1".to_string(), "2".to_string(), "3".to_string()]);
    }

    #[test]
    fn test_dequeue_last() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("1"), song("2"), song("3")]);

        state.play("3");
        assert!(state.is_playing());

        state.dequeue(&["3".to_string()]);
        assert_eq!(state.current_song_id(), None);
    }

    #[test]
    fn test_dequeue_a_few_songs() {
        let mut state = PlaybackState::default();
        state.queue(vec![
            song("1"),
            song("2"),
            song("3"),
            song("4"),
            song("5"),
            song("6"),
        ]);

        state.play("5");
        assert!(state.is_playing());

        state.dequeue(&["1".to_string(), "2".to_string(), "3".to_string()]);
        assert_eq!(state.current_song_id(), Some("5".to_string()));
    }

    #[test]
    fn test_dequeue_all() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("3")]);

        state.play("3");
        assert!(state.is_playing());

        state.dequeue(&["3".to_string()]);
        assert_eq!(state.current_song_id(), None);
    }
}
