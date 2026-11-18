//! Build searchable catalog views without blocking the presentation thread.
use amp_core::{Library, browse::CatalogIndex};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

#[derive(Clone)]
pub struct ReadTicket {
    pub serial: u64,
    cancelled: Arc<AtomicBool>,
}

impl ReadTicket {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Owns a read for the currently viewed identity. Reopening the same item after
/// leaving it receives a new serial, so late replies cannot settle the new read.
pub struct SelectionRead<K> {
    current: Option<(K, ReadTicket)>,
    serial: u64,
}

impl<K> Default for SelectionRead<K> {
    fn default() -> Self {
        Self {
            current: None,
            serial: 0,
        }
    }
}

impl<K> SelectionRead<K> {
    pub fn cancel(&mut self) {
        if let Some((_, ticket)) = self.current.take() {
            ticket.cancelled.store(true, Ordering::Release);
        }
    }
}

impl<K: PartialEq> SelectionRead<K> {
    pub fn keep_target(&mut self, target: Option<&K>) -> bool {
        if self
            .current
            .as_ref()
            .is_some_and(|(key, _)| Some(key) == target)
        {
            return true;
        }
        self.cancel();
        false
    }

    pub fn begin(&mut self, target: K) -> Option<ReadTicket> {
        if self.keep_target(Some(&target)) {
            return None;
        }
        self.serial = self.serial.wrapping_add(1);
        let ticket = ReadTicket {
            serial: self.serial,
            cancelled: Arc::default(),
        };
        self.current = Some((target, ticket.clone()));
        Some(ticket)
    }

    pub fn accepts(&self, target: &K, serial: u64) -> bool {
        self.current
            .as_ref()
            .is_some_and(|(key, ticket)| key == target && ticket.serial == serial)
    }
}

impl<K> Drop for SelectionRead<K> {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// At most one build runs at a time. Bursts of catalog changes keep only the
/// newest pending input. The caller can accept intermediate revisions of its
/// current source without applying obsolete results from a different source.
#[derive(Default)]
pub struct IndexBuilder {
    pending: Option<(u64, Arc<Library>)>,
    active: Option<(u64, mpsc::Receiver<CatalogIndex>)>,
    worker: Option<mpsc::Sender<IndexWork>>,
}

enum IndexWork {
    Build(Arc<Library>, mpsc::Sender<CatalogIndex>),
    Release(Arc<CatalogIndex>),
}

impl IndexBuilder {
    pub fn request(&mut self, revision: u64, library: Arc<Library>) {
        self.pending = Some((revision, library));
        self.start_pending();
    }

    pub fn poll(&mut self) -> Option<(u64, CatalogIndex)> {
        let (revision, result) = self.active.as_ref()?;
        let completed = match result.try_recv() {
            Ok(index) => Some((*revision, index)),
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => None,
        };
        self.active = None;
        self.start_pending();
        completed
    }

    /// A large retired index can take longer to release than a presentation frame.
    pub fn release(&mut self, index: Arc<CatalogIndex>) {
        self.send(IndexWork::Release(index));
    }

    fn start_pending(&mut self) {
        if self.active.is_none()
            && let Some((revision, library)) = self.pending.take()
        {
            let (tx, rx) = mpsc::channel();
            self.send(IndexWork::Build(library, tx));
            self.active = Some((revision, rx));
        }
    }

    fn send(&mut self, work: IndexWork) {
        let work = if let Some(worker) = &self.worker {
            match worker.send(work) {
                Ok(()) => return,
                Err(mpsc::SendError(work)) => work,
            }
        } else {
            work
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(work) = rx.recv() {
                match work {
                    IndexWork::Build(library, result) => {
                        let _ = result.send(CatalogIndex::new(&library));
                    }
                    IndexWork::Release(index) => drop(index),
                }
            }
        });
        let _ = tx.send(work);
        self.worker = Some(tx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amp_core::{Album, ItemId};
    use std::time::{Duration, Instant};

    #[test]
    fn a_burst_builds_the_active_and_latest_pending_catalogs() {
        let library = |title: &str| {
            Arc::new(Library {
                albums: vec![Album {
                    id: ItemId {
                        library: "fixture".into(),
                        provider: "fixture".into(),
                        item: title.into(),
                    },
                    title: title.into(),
                    artist: "Artist".into(),
                    source_label: String::new(),
                    metadata: Default::default(),
                    tracks: Vec::new(),
                }],
                ..Default::default()
            })
        };
        let mut builder = IndexBuilder::default();
        builder.request(1, library("First"));
        builder.request(2, library("Obsolete"));
        builder.request(3, library("Current"));
        let deadline = Instant::now() + Duration::from_secs(2);
        for (expected_revision, expected_title) in [(1, "First"), (3, "Current")] {
            let (revision, index) = loop {
                if let Some(result) = builder.poll() {
                    break result;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            };
            assert_eq!(revision, expected_revision);
            assert_eq!(index.albums[0].title, expected_title);
        }
        assert!(builder.poll().is_none());
    }
}
