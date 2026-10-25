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
    time::Duration,
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
    native_update: NativeUpdate,
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
                    native_update,
                },
            )
        });
        let publisher = Publisher::new(
            dispatch,
            Arc::new(move |state| {
                SESSIONS.with(|sessions| {
                    if let Some(controls) = sessions.borrow_mut().get_mut(&id) {
                        update(&mut controls.controls, state.clone());
                        (controls.native_update)(&state);
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
            if let Some(mut controls) = sessions.borrow_mut().remove(&self.id) {
                let _ = controls.controls.set_playback(MediaPlayback::Stopped);
                (controls.native_update)(&NowPlaying {
                    track: None,
                    playing: false,
                    seconds: 0.0,
                });
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
fn update(controls: &mut MediaControls, state: NowPlaying) {
    let Some(track) = state.track else {
        let _ = controls.set_metadata(MediaMetadata::default());
        let _ = controls.set_playback(MediaPlayback::Stopped);
        return;
    };
    let _ = controls.set_metadata(MediaMetadata {
        title: Some(&track.title),
        artist: Some(&track.artist),
        duration: Some(duration(track.duration_seconds)),
        ..Default::default()
    });
    let progress = Some(MediaPosition(duration(state.seconds)));
    let _ = controls.set_playback(if state.playing {
        MediaPlayback::Playing { progress }
    } else {
        MediaPlayback::Paused { progress }
    });
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
