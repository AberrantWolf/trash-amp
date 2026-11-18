//! Build searchable catalog views without blocking the presentation thread.
use amp_core::{Library, browse::CatalogIndex};
use std::sync::{Arc, mpsc};

/// At most one build runs at a time. Bursts of catalog changes keep only the
/// newest pending input. The caller can accept intermediate revisions of its
/// current source without applying obsolete results from a different source.
#[derive(Default)]
pub struct IndexBuilder {
    pending: Option<(u64, Arc<Library>)>,
    active: Option<(u64, mpsc::Receiver<CatalogIndex>)>,
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

    fn start_pending(&mut self) {
        if self.active.is_none()
            && let Some((revision, library)) = self.pending.take()
        {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(CatalogIndex::new(&library));
            });
            self.active = Some((revision, rx));
        }
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
