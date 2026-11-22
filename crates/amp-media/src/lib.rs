//! Native media-session adapter. Native objects stay on their installation
//! thread; the application worker sends coalesced updates through the host's
//! dispatcher. This crate has no Slint dependency or platform selection logic.
use amp_app::{NowPlaying, Playback, Transport};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub type MainTask = Box<dyn FnOnce() + Send>;
/// Must schedule tasks onto the thread which installs the media session.
pub type MainDispatch = Arc<dyn Fn(MainTask) + Send + Sync>;

thread_local! {
    static SESSIONS: RefCell<BTreeMap<u64, SessionControls>> = const { RefCell::new(BTreeMap::new()) };
}
pub type NativeUpdate = Box<dyn Fn(&NowPlaying)>;
struct SessionControls {
    controls: MediaControls,
    publisher: MetadataPublisher,
    sent: Sent,
}

enum MetadataPublisher {
    Supplement(NativeUpdate),
    Native(NativeUpdate),
}
impl MetadataPublisher {
    fn publish(&self, state: &NowPlaying, backend: impl FnOnce()) {
        match self {
            Self::Supplement(callback) => {
                backend();
                callback(state);
            }
            Self::Native(callback) => callback(state),
        }
    }
}
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// Keep this guard on the installation thread. Removing it detaches native
/// callbacks and makes already-dispatched updates harmless.
pub struct MediaSession {
    id: u64,
    playback: Playback,
    _main_thread: PhantomData<Rc<()>>,
}
impl MediaSession {
    pub fn install(
        config: PlatformConfig,
        playback: Playback,
        dispatch: MainDispatch,
    ) -> Result<Self, String> {
        Self::install_with_update(config, playback, dispatch, Box::new(|_| {}))
    }
    /// Hosts may supplement the native backend's metadata on the installation
    /// thread. The hook runs after every coalesced update, including clearing.
    pub fn install_with_update(
        config: PlatformConfig,
        playback: Playback,
        dispatch: MainDispatch,
        native_update: NativeUpdate,
    ) -> Result<Self, String> {
        Self::install_with_publisher(
            config,
            playback,
            dispatch,
            MetadataPublisher::Supplement(native_update),
        )
    }
    /// Let the host publish a complete native state atomically. Souvlaki still
    /// handles remote commands, but does not write intermediate metadata states.
    pub fn install_with_native_metadata(
        config: PlatformConfig,
        playback: Playback,
        dispatch: MainDispatch,
        publish: NativeUpdate,
    ) -> Result<Self, String> {
        Self::install_with_publisher(
            config,
            playback,
            dispatch,
            MetadataPublisher::Native(publish),
        )
    }
    fn install_with_publisher(
        config: PlatformConfig,
        playback: Playback,
        dispatch: MainDispatch,
        publisher: MetadataPublisher,
    ) -> Result<Self, String> {
        let mut controls = MediaControls::new(config).map_err(|e| format!("{e:?}"))?;
        let commands = playback.clone();
        controls
            .attach(move |event| {
                if let Some(transport) = transport(event) {
                    commands.transport(transport);
                }
            })
            .map_err(|e| format!("{e:?}"))?;
        let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        SESSIONS.with(|sessions| {
            sessions.borrow_mut().insert(
                id,
                SessionControls {
                    controls,
                    publisher,
                    sent: Sent::default(),
                },
            )
        });
        let publisher = Publisher::new(
            dispatch,
            Arc::new(move |state| {
                SESSIONS.with(|sessions| {
                    if let Some(controls) = sessions.borrow_mut().get_mut(&id) {
                        let SessionControls {
                            controls,
                            publisher,
                            sent,
                        } = controls;
                        publisher.publish(&state, || update(controls, sent, &state));
                    }
                });
            }),
        );
        playback.observe_now_playing(Some(Arc::new(move |state| publisher.publish(state))));
        Ok(Self {
            id,
            playback,
            _main_thread: PhantomData,
        })
    }
}
impl Drop for MediaSession {
    fn drop(&mut self) {
        self.playback.observe_now_playing(None);
        SESSIONS.with(|sessions| {
            if let Some(SessionControls {
                mut controls,
                publisher,
                mut sent,
            }) = sessions.borrow_mut().remove(&self.id)
            {
                let state = NowPlaying {
                    album: None,
                    track: None,
                    playing: false,
                    seconds: 0.0,
                };
                publisher.publish(&state, || update(&mut controls, &mut sent, &state));
                // Souvlaki's MPRIS backend joins its D-Bus thread, which first
                // works through every queued update; don't hold up quitting.
                #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "windows")))]
                std::thread::spawn(move || drop(controls));
            }
        });
    }
}

fn transport(event: MediaControlEvent) -> Option<Transport> {
    Some(match event {
        MediaControlEvent::Play => Transport::Play,
        MediaControlEvent::Pause => Transport::Pause,
        MediaControlEvent::Toggle => Transport::Toggle,
        MediaControlEvent::Stop => Transport::Stop,
        MediaControlEvent::Next => Transport::Next,
        MediaControlEvent::Previous => Transport::Previous,
        MediaControlEvent::SetPosition(position) => Transport::Seek(position.0.as_secs_f64()),
        _ => return None,
    })
}

fn duration(seconds: f64) -> Duration {
    Duration::try_from_secs_f64(seconds.max(0.0)).unwrap_or_default()
}
/// What was last sent to the platform media controls.
#[derive(Default)]
struct Sent {
    metadata: Option<Option<MetadataKey>>,
    playback: Option<(bool, f64, Instant)>,
}
type MetadataKey = (Option<String>, String, String, u64);

impl Sent {
    /// Whether to send new metadata, then whether to send the playback state.
    /// Clients advance the position themselves while playing, so the playing
    /// position is only resent after a seek or a pause/resume. Resending it
    /// every second backs up souvlaki's MPRIS thread (issue #12).
    fn changes(&self, metadata: &Option<MetadataKey>, playing: bool, seconds: f64) -> (bool, bool) {
        let new_metadata = self.metadata.as_ref() != Some(metadata);
        let new_playback = new_metadata
            || self.playback.is_none_or(|(was_playing, at, when)| {
                let expected = if was_playing {
                    at + when.elapsed().as_secs_f64()
                } else {
                    at
                };
                was_playing != playing || (seconds - expected).abs() > 1.5
            });
        (new_metadata, new_playback)
    }
}

fn update(controls: &mut MediaControls, sent: &mut Sent, state: &NowPlaying) {
    let metadata = state.track.as_ref().map(|track| {
        (
            state.album.as_ref().map(|album| album.title.clone()),
            track.title.clone(),
            track.artist.clone(),
            track.duration_seconds.to_bits(),
        )
    });
    let (new_metadata, new_playback) = sent.changes(&metadata, state.playing, state.seconds);
    if new_metadata {
        let _ = controls.set_metadata(match &state.track {
            Some(track) => MediaMetadata {
                album: state.album.as_ref().map(|album| album.title.as_str()),
                title: Some(&track.title),
                artist: Some(&track.artist),
                duration: Some(duration(track.duration_seconds)),
                ..Default::default()
            },
            None => MediaMetadata::default(),
        });
        sent.metadata = Some(metadata.clone());
    }
    if new_playback {
        let progress = Some(MediaPosition(duration(state.seconds)));
        let _ = controls.set_playback(match (&state.track, state.playing) {
            (None, _) => MediaPlayback::Stopped,
            (Some(_), true) => MediaPlayback::Playing { progress },
            (Some(_), false) => MediaPlayback::Paused { progress },
        });
        sent.playback = Some((state.playing, state.seconds, Instant::now()));
    }
}

#[derive(Default)]
struct Mailbox {
    latest: Option<NowPlaying>,
    scheduled: bool,
}
struct Publisher {
    mailbox: Arc<Mutex<Mailbox>>,
    dispatch: MainDispatch,
    consume: Arc<dyn Fn(NowPlaying) + Send + Sync>,
}
impl Publisher {
    fn new(dispatch: MainDispatch, consume: Arc<dyn Fn(NowPlaying) + Send + Sync>) -> Self {
        Self {
            mailbox: Arc::default(),
            dispatch,
            consume,
        }
    }
    fn publish(&self, state: NowPlaying) {
        {
            let mut mailbox = self.mailbox.lock().unwrap();
            mailbox.latest = Some(state);
            if mailbox.scheduled {
                return;
            }
            mailbox.scheduled = true;
        }
        let mailbox = self.mailbox.clone();
        let consume = self.consume.clone();
        (self.dispatch)(Box::new(move || {
            let state = {
                let mut mailbox = mailbox.lock().unwrap();
                mailbox.scheduled = false;
                mailbox.latest.take()
            };
            if let Some(state) = state {
                consume(state);
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn playing_position_is_only_resent_after_a_change_or_seek() {
        let track = Some((
            None,
            "Song".to_owned(),
            "Artist".to_owned(),
            200f64.to_bits(),
        ));
        let mut sent = Sent::default();
        assert_eq!(sent.changes(&track, true, 10.0), (true, true));
        sent.metadata = Some(track.clone());
        let started = Instant::now() - Duration::from_secs(3);
        sent.playback = Some((true, 10.0, started));
        // Normal playback three seconds later: nothing to send.
        assert_eq!(sent.changes(&track, true, 13.0), (false, false));
        // A seek, a pause, or a new track is sent.
        assert_eq!(sent.changes(&track, true, 60.0), (false, true));
        assert_eq!(sent.changes(&track, false, 13.0), (false, true));
        let next = Some((
            None,
            "Next".to_owned(),
            "Artist".to_owned(),
            180f64.to_bits(),
        ));
        assert_eq!(sent.changes(&next, true, 0.0), (true, true));
        // While paused the position should stay put.
        sent.playback = Some((false, 13.0, started));
        assert_eq!(sent.changes(&track, false, 13.0), (false, false));
        assert_eq!(sent.changes(&None, false, 0.0), (true, true));
    }

    #[test]
    fn native_publisher_bypasses_intermediate_backend_writes() {
        let received = Rc::new(RefCell::new(Vec::new()));
        let output = received.clone();
        let publisher = MetadataPublisher::Native(Box::new(move |state| {
            output.borrow_mut().push(state.seconds);
        }));
        for seconds in [0.0, 52.0, 8.0] {
            publisher.publish(
                &NowPlaying {
                    album: None,
                    track: None,
                    playing: true,
                    seconds,
                },
                || panic!("native publication must not write a partial backend state"),
            );
        }
        assert_eq!(*received.borrow(), vec![0.0, 52.0, 8.0]);
    }

    #[test]
    fn supplemental_publisher_runs_after_backend_metadata() {
        let order = Rc::new(RefCell::new(Vec::new()));
        let output = order.clone();
        let publisher = MetadataPublisher::Supplement(Box::new(move |_| {
            output.borrow_mut().push("supplement");
        }));
        publisher.publish(
            &NowPlaying {
                album: None,
                track: None,
                playing: false,
                seconds: 0.0,
            },
            || order.borrow_mut().push("backend"),
        );
        assert_eq!(*order.borrow(), vec!["backend", "supplement"]);
    }

    #[test]
    fn suspended_dispatch_keeps_one_task_and_the_latest_state() {
        let tasks = Arc::new(Mutex::new(Vec::<MainTask>::new()));
        let queued = tasks.clone();
        let received = Arc::new(Mutex::new(Vec::new()));
        let consumed = received.clone();
        let publisher = Publisher::new(
            Arc::new(move |task| queued.lock().unwrap().push(task)),
            Arc::new(move |state| consumed.lock().unwrap().push(state.seconds)),
        );
        for seconds in 0..1000 {
            publisher.publish(NowPlaying {
                album: None,
                track: None,
                playing: true,
                seconds: seconds as f64,
            });
        }
        assert_eq!(tasks.lock().unwrap().len(), 1);
        let task = tasks.lock().unwrap().pop().unwrap();
        task();
        assert_eq!(*received.lock().unwrap(), vec![999.0]);
        publisher.publish(NowPlaying {
            album: None,
            track: None,
            playing: false,
            seconds: 1000.0,
        });
        assert_eq!(tasks.lock().unwrap().len(), 1);
        let task = tasks.lock().unwrap().pop().unwrap();
        task();
        assert_eq!(*received.lock().unwrap(), vec![999.0, 1000.0]);
    }
}
