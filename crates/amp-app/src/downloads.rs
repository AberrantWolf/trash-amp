//! Session download batches, independent of views and their polling lifetime.
use amp_core::Track;
use anyhow::{Result, bail};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// Persistence and transfer policy belong to the provider; batch ownership does not.
pub trait Backend: Send + Sync + 'static {
    fn pending(&self) -> Result<Vec<Track>>;
    fn reset_cancel(&self);
    fn cancelled(&self) -> bool;
    /// Set cancellation intent without waiting for persistence or a transfer.
    fn interrupt(&self);
    fn cancel(&self) -> Result<()>;
    fn schedule(&self, tracks: &[Track]) -> Result<()>;
    fn download(&self, track: Track) -> Result<()>;
    fn remove(&self, track: &Track) -> Result<()>;
}
#[derive(Clone, Copy)]
pub enum Action {
    Download,
    Remove,
}
pub enum Event {
    Message(String),
    Changed,
}
#[derive(Clone)]
pub struct Downloads {
    backend: Arc<dyn Backend>,
    busy: Arc<AtomicBool>,
    transition: Arc<Mutex<()>>,
    cancelling: Arc<AtomicBool>,
    observer: Arc<dyn Fn(Event) + Send + Sync>,
}
impl Downloads {
    /// Resume only persisted pending work; cancelled work needs an explicit start.
    pub fn new(
        backend: Arc<dyn Backend>,
        observer: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        let pending = backend.pending()?;
        let service = Self {
            backend,
            busy: Arc::default(),
            transition: Arc::default(),
            cancelling: Arc::default(),
            observer,
        };
        if !pending.is_empty() {
            service.run(pending, Action::Download, false)?;
        }
        Ok(service)
    }
    pub fn start(&self, tracks: Vec<Track>, action: Action) -> Result<()> {
        self.run(tracks, action, true)
    }
    /// Interrupt immediately, then persist cancellation off the calling thread.
    /// Admission stays closed until that write finishes, including when the
    /// active transfer finishes before its cancellation worker gets scheduled.
    pub fn request_cancel(&self) -> Result<()> {
        if self.cancelling.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.backend.interrupt();
        let service = self.clone();
        let spawned = std::thread::Builder::new()
            .name("amp-download-cancel".into())
            .spawn(move || {
                let transition = service.transition.lock().unwrap();
                let result = service.backend.cancel();
                service.cancelling.store(false, Ordering::Release);
                drop(transition);
                let message = match result {
                    Ok(()) => "Download cancellation saved".into(),
                    Err(error) => format!("Could not save download cancellation: {error}"),
                };
                (service.observer)(Event::Message(message));
                (service.observer)(Event::Changed);
            });
        if let Err(error) = spawned {
            self.cancelling.store(false, Ordering::Release);
            return Err(error.into());
        }
        Ok(())
    }
    fn run(&self, tracks: Vec<Track>, action: Action, schedule: bool) -> Result<()> {
        // UI commands never wait behind cancellation persistence. A retry is
        // accepted only after the earlier cancellation has fully committed.
        let _transition = self.transition.try_lock().map_err(|_| {
            anyhow::anyhow!("Download cancellation is still being saved; retry shortly")
        })?;
        if self.cancelling.load(Ordering::Acquire) {
            bail!("Download cancellation is still being saved; retry shortly")
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            bail!("Downloads are active; wait or cancel before changing downloads")
        }
        self.backend.reset_cancel();
        // Cancel may have arrived after admission but before reset. Reapply its
        // immediate intent; its worker will serialize persistence after this lock.
        if self.cancelling.load(Ordering::Acquire) {
            self.backend.interrupt();
        }
        let service = self.clone();
        let spawned = std::thread::Builder::new()
            .name("amp-downloads".into())
            .spawn(move || {
                let result = (|| -> Result<usize> {
                    if schedule && matches!(action, Action::Download) {
                        service.backend.schedule(&tracks)?;
                    }
                    let mut failed = 0;
                    for track in tracks {
                        if service.backend.cancelled() {
                            break;
                        }
                        let result = match action {
                            Action::Download => service.backend.download(track),
                            Action::Remove => service.backend.remove(&track),
                        };
                        if let Err(error) = result {
                            if service.backend.cancelled() {
                                break;
                            }
                            failed += 1;
                            (service.observer)(Event::Message(error.to_string()));
                        }
                    }
                    Ok(failed)
                })();
                let message = if service.backend.cancelled() {
                    "Downloads cancelled".into()
                } else {
                    match result {
                        Ok(failed) => format!("Downloads settled · {failed} failed"),
                        Err(error) => error.to_string(),
                    }
                };
                service.busy.store(false, Ordering::Release);
                (service.observer)(Event::Message(message));
                (service.observer)(Event::Changed);
            });
        if let Err(error) = spawned {
            self.busy.store(false, Ordering::Release);
            return Err(error.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, mpsc};
    struct Fake {
        cancelled: AtomicBool,
        entered: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
        attempts: Mutex<Vec<String>>,
        cancel_gate: Option<(mpsc::Sender<()>, Mutex<mpsc::Receiver<()>>)>,
    }
    impl Backend for Fake {
        fn pending(&self) -> Result<Vec<Track>> {
            Ok(vec![])
        }
        fn reset_cancel(&self) {
            self.cancelled.store(false, Ordering::Release);
        }
        fn cancelled(&self) -> bool {
            self.cancelled.load(Ordering::Acquire)
        }
        fn interrupt(&self) {
            self.cancelled.store(true, Ordering::Release);
        }
        fn cancel(&self) -> Result<()> {
            self.interrupt();
            if let Some((entered, resume)) = &self.cancel_gate {
                entered.send(())?;
                resume.lock().unwrap().recv()?;
            }
            Ok(())
        }
        fn schedule(&self, _: &[Track]) -> Result<()> {
            Ok(())
        }
        fn download(&self, track: Track) -> Result<()> {
            self.attempts.lock().unwrap().push(track.title);
            self.entered.send(())?;
            self.resume.lock().unwrap().recv()?;
            Ok(())
        }
        fn remove(&self, _: &Track) -> Result<()> {
            Ok(())
        }
    }
    fn track(title: &str) -> Track {
        Track {
            scan_status: None,
            blocked_reason: None,
            availability: Default::default(),
            source: None,
            id: amp_core::ItemId {
                library: "test".into(),
                provider: "test".into(),
                item: title.into(),
            },
            title: title.into(),
            artist: String::new(),
            number: 1,
            disc: 1,
            duration_seconds: 1.,
            access: amp_core::MediaAccess::File {
                locator: title.into(),
            },
        }
    }
    #[test]
    fn batch_cancel_stops_the_tail_and_rejects_overlapping_batches() {
        let (entered, started) = mpsc::channel();
        let (resume, paused) = mpsc::channel();
        let backend = Arc::new(Fake {
            cancelled: AtomicBool::new(false),
            entered,
            resume: Mutex::new(paused),
            attempts: Mutex::default(),
            cancel_gate: None,
        });
        let (events, updates) = mpsc::channel();
        let service = Downloads::new(
            backend.clone(),
            Arc::new(move |event| {
                let _ = events.send(event);
            }),
        )
        .unwrap();
        service
            .start(vec![track("first"), track("tail")], Action::Download)
            .unwrap();
        started
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(
            service
                .start(vec![track("overlap")], Action::Download)
                .is_err()
        );
        service.request_cancel().unwrap();
        resume.send(()).unwrap();
        while service.busy.load(Ordering::Acquire) || service.cancelling.load(Ordering::Acquire) {
            if matches!(
                updates
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap(),
                Event::Changed
            ) {
                continue;
            }
        }
        assert_eq!(*backend.attempts.lock().unwrap(), ["first"]);
        assert!(!service.busy.load(Ordering::Acquire));
    }
    #[test]
    fn blocked_cancellation_persistence_never_blocks_commands_or_cancels_a_newer_batch() {
        let (cancel_entered, cancelling) = mpsc::channel();
        let (allow_cancel, cancel_gate) = mpsc::channel();
        let (entered, _) = mpsc::channel();
        let (_, paused) = mpsc::channel();
        let backend = Arc::new(Fake {
            cancelled: AtomicBool::new(false),
            entered,
            resume: Mutex::new(paused),
            attempts: Mutex::default(),
            cancel_gate: Some((cancel_entered, Mutex::new(cancel_gate))),
        });
        let (events, updates) = mpsc::channel();
        let service = Downloads::new(
            backend.clone(),
            Arc::new(move |event| {
                let _ = events.send(event);
            }),
        )
        .unwrap();
        // The call must return while durable cancellation is still gated.
        service.request_cancel().unwrap();
        cancelling
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(backend.cancelled());
        let retry = service.clone();
        let (completed, completion) = mpsc::channel();
        let caller = std::thread::spawn(move || {
            completed
                .send(retry.start(vec![], Action::Download).is_err())
                .unwrap();
        });
        assert!(
            completion
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
        );
        caller.join().unwrap();
        assert!(
            backend.cancelled(),
            "rejected retries must not clear cancellation"
        );
        allow_cancel.send(()).unwrap();
        loop {
            if matches!(
                updates
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap(),
                Event::Changed
            ) {
                break;
            }
        }
        service.start(vec![], Action::Remove).unwrap();
        assert!(
            !backend.cancelled(),
            "a later accepted batch clears only settled cancellation"
        );
    }
}
