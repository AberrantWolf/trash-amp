//! Progressive local-file playlists. No catalog, store, network, or file writes.
use amp_core::Track;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default)]
pub struct LoadProgress {
    pub visited: usize,
    pub loaded: usize,
    pub skipped: usize,
    pub path: PathBuf,
}
#[derive(Default)]
pub struct LoadSummary {
    pub loaded: usize,
    pub skipped: usize,
    /// At most five diagnostic examples, even for very large directory trees.
    pub warnings: Vec<String>,
    pub cancelled: bool,
}
pub enum LoadEvent {
    Track(Track),
    Finished(LoadSummary),
}

/// Dropping the job cancels it; the UI never joins a blocked filesystem read.
pub struct FileLoad {
    cancelled: Arc<AtomicBool>,
    cancel_on_drop: bool,
    progress: Arc<Mutex<LoadProgress>>,
    events: mpsc::Receiver<LoadEvent>,
}
impl FileLoad {
    pub fn start(paths: Vec<PathBuf>, session: String) -> std::io::Result<Self> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(LoadProgress::default()));
        // Backpressure bounds memory without delaying the first discovered track.
        let (tx, events) = mpsc::sync_channel(32);
        let stop = cancelled.clone();
        let latest = progress.clone();
        std::thread::Builder::new()
            .name("dropped-music".into())
            .spawn(move || {
                let summary = scan(
                    paths,
                    &session,
                    &stop,
                    |p| *latest.lock().unwrap() = p.clone(),
                    |track| send(&tx, LoadEvent::Track(track), &stop),
                    amp_library::local_file_track,
                );
                if !summary.cancelled {
                    send(&tx, LoadEvent::Finished(summary), &stop);
                }
            })?;
        Ok(Self {
            cancelled,
            cancel_on_drop: true,
            progress,
            events,
        })
    }
    /// The event stream was drained normally; queued playback commands still own the token.
    pub fn finish(&mut self) {
        self.cancel_on_drop = false;
    }
    pub fn progress(&self) -> LoadProgress {
        self.progress.lock().unwrap().clone()
    }
    pub fn next_event(&self) -> Result<LoadEvent, mpsc::TryRecvError> {
        self.events.try_recv()
    }
    /// Playback also cancels this token if the user replaces or clears the queue.
    pub fn cancel_token(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}
impl Drop for FileLoad {
    fn drop(&mut self) {
        if self.cancel_on_drop {
            self.cancelled.store(true, Ordering::Relaxed);
        }
    }
}
fn send(tx: &mpsc::SyncSender<LoadEvent>, mut event: LoadEvent, stop: &AtomicBool) -> bool {
    while !stop.load(Ordering::Relaxed) {
        match tx.try_send(event) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Full(value)) => {
                event = value;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
        }
    }
    false
}
struct Collection<'a, P, E, F> {
    cancelled: &'a AtomicBool,
    progress: P,
    emit: E,
    probe: F,
    last_progress: Instant,
    state: LoadProgress,
    result: LoadSummary,
    files: HashSet<PathBuf>,
    directories: HashSet<PathBuf>,
}
impl<
    P: FnMut(&LoadProgress),
    E: FnMut(Track) -> bool,
    F: FnMut(&Path, &str) -> anyhow::Result<Track>,
> Collection<'_, P, E, F>
{
    fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
    fn report(&mut self, force: bool) {
        if force || self.last_progress.elapsed() >= Duration::from_millis(100) {
            self.state.loaded = self.result.loaded;
            self.state.skipped = self.result.skipped;
            (self.progress)(&self.state);
            self.last_progress = Instant::now();
        }
    }
    fn visit(&mut self, path: &Path) {
        self.state.visited += 1;
        self.state.path = path.into();
        self.report(false);
    }
    fn skip(&mut self, path: &Path, reason: &str, diagnostic: bool) {
        self.result.skipped += 1;
        if diagnostic && self.result.warnings.len() < 5 {
            self.result
                .warnings
                .push(format!("{}: {reason}", path.display()));
        }
    }
    // Filter DURING enumeration, before retaining paths, deduplicating, or probing.
    fn eligible(&mut self, path: &Path, kind: std::fs::FileType) -> bool {
        if kind.is_dir() || (kind.is_file() && amp_library::is_supported_audio_file(path)) {
            true
        } else {
            self.skip(path, "Unsupported file or symbolic link", false);
            false
        }
    }
    fn run(&mut self, roots: Vec<PathBuf>, session: &str) {
        let mut stack: Vec<Vec<PathBuf>> = Vec::new();
        for root in roots {
            if self.stopped() {
                return;
            }
            self.visit(&root);
            let root = match root.canonicalize() {
                Ok(path) => path,
                Err(e) => {
                    self.skip(&root, &e.to_string(), true);
                    continue;
                }
            };
            stack.push(vec![root]);
            while let Some(frame) = stack.last_mut() {
                if self.stopped() {
                    return;
                }
                let Some(path) = frame.pop() else {
                    stack.pop();
                    continue;
                };
                self.state.path = path.clone();
                self.report(false);
                let kind = match std::fs::symlink_metadata(&path) {
                    Ok(m) => m.file_type(),
                    Err(e) => {
                        self.skip(&path, &e.to_string(), true);
                        continue;
                    }
                };
                if !self.eligible(&path, kind) {
                    continue;
                }
                if kind.is_dir() {
                    if !self.directories.insert(path.clone()) {
                        continue;
                    }
                    let entries = match std::fs::read_dir(&path) {
                        Ok(entries) => entries,
                        Err(e) => {
                            self.skip(&path, &e.to_string(), true);
                            continue;
                        }
                    };
                    // Only this directory's eligible entries are buffered for natural order.
                    // Tracks are emitted as soon as probed, before traversing later folders.
                    let mut children = Vec::new();
                    for entry in entries {
                        if self.stopped() {
                            return;
                        }
                        match entry {
                            Ok(entry) => {
                                let child = entry.path();
                                self.visit(&child);
                                match entry.file_type() {
                                    Ok(kind) if self.eligible(&child, kind) => children.push(child),
                                    Ok(_) => {}
                                    Err(e) => self.skip(&child, &e.to_string(), true),
                                }
                            }
                            Err(e) => self.skip(&path, &e.to_string(), true),
                        }
                    }
                    children.sort_by(|a, b| {
                        natord::compare(&b.to_string_lossy(), &a.to_string_lossy())
                            .then_with(|| b.cmp(a))
                    });
                    stack.push(children);
                } else {
                    let path = match path.canonicalize() {
                        Ok(path) => path,
                        Err(e) => {
                            self.skip(&path, &e.to_string(), true);
                            continue;
                        }
                    };
                    if !self.files.insert(path.clone()) {
                        continue;
                    }
                    if self.stopped() {
                        return;
                    }
                    match (self.probe)(&path, session) {
                        Ok(track) => {
                            if self.stopped() {
                                return;
                            }
                            if !(self.emit)(track) {
                                self.cancelled.store(true, Ordering::Relaxed);
                                return;
                            }
                            self.result.loaded += 1;
                        }
                        Err(e) => self.skip(&path, &e.to_string(), true),
                    }
                    // Metadata can be slow even after a flat directory was enumerated.
                    self.report(false);
                }
            }
        }
    }
}
fn scan(
    paths: Vec<PathBuf>,
    session: &str,
    cancelled: &AtomicBool,
    progress: impl FnMut(&LoadProgress),
    emit: impl FnMut(Track) -> bool,
    probe: impl FnMut(&Path, &str) -> anyhow::Result<Track>,
) -> LoadSummary {
    let mut scan = Collection {
        cancelled,
        progress,
        emit,
        probe,
        last_progress: Instant::now(),
        state: LoadProgress::default(),
        result: LoadSummary::default(),
        files: HashSet::new(),
        directories: HashSet::new(),
    };
    scan.run(paths, session);
    scan.result.cancelled = scan.stopped();
    scan.report(true);
    scan.result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fake(path: &Path, session: &str) -> anyhow::Result<Track> {
        Ok(Track {
            id: amp_core::ItemId {
                library: session.into(),
                provider: "filesystem".into(),
                item: path.to_string_lossy().into(),
            },
            title: path.file_stem().unwrap().to_string_lossy().into(),
            artist: "Unknown artist".into(),
            number: 0,
            disc: 1,
            duration_seconds: 1.,
            source: None,
            availability: Default::default(),
            access: amp_core::MediaAccess::File {
                locator: path.to_string_lossy().into(),
            },
        })
    }
    #[test]
    fn streams_before_probing_remaining_tracks_and_preserves_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("disc2")).unwrap();
        for name in ["2.mp3", "10.MP3", "disc2/1.wav", "junk.txt", "list.m3u"] {
            std::fs::write(dir.path().join(name), []).unwrap();
        }
        let emitted = std::cell::RefCell::new(Vec::new());
        let summary = scan(
            vec![dir.path().into(), dir.path().join("2.mp3")],
            "drop:test",
            &AtomicBool::new(false),
            |_| {},
            |track| {
                emitted.borrow_mut().push(track.title);
                true
            },
            |path, session| {
                assert!(amp_library::is_supported_audio_file(path));
                if path.file_stem().unwrap() == "10" {
                    assert_eq!(*emitted.borrow(), ["2"]);
                }
                fake(path, session)
            },
        );
        assert_eq!(*emitted.borrow(), ["2", "10", "1"]);
        assert_eq!(summary.loaded, 3);
        assert_eq!(summary.skipped, 2);
    }
    #[test]
    fn filters_while_enumerating_and_publishes_metadata_progress() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..2000 {
            std::fs::write(dir.path().join(format!("{n}.txt")), []).unwrap();
        }
        for name in ["1.mp3", "2.mp3"] {
            std::fs::write(dir.path().join(name), []).unwrap();
        }
        let mut progress = Vec::new();
        let summary = scan(
            vec![dir.path().into()],
            "drop:test",
            &AtomicBool::new(false),
            |p| progress.push(p.clone()),
            |_| true,
            |path, session| {
                assert!(amp_library::is_supported_audio_file(path));
                std::thread::sleep(Duration::from_millis(110));
                fake(path, session)
            },
        );
        assert_eq!(summary.loaded, 2);
        assert_eq!(summary.skipped, 2000);
        assert!(summary.warnings.is_empty());
        assert!(
            progress
                .iter()
                .any(|p| p.loaded == 1 && p.path.file_name().is_some_and(|name| name == "1.mp3")),
            "Flat-folder probing must publish both intermediate counts and the current file"
        );
    }
    #[test]
    fn cancellation_keeps_emitted_tracks_and_stops_future_probes() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["1.mp3", "2.mp3"] {
            std::fs::write(dir.path().join(name), []).unwrap();
        }
        let stop = AtomicBool::new(false);
        let mut emitted = Vec::new();
        let mut probes = 0;
        let summary = scan(
            vec![dir.path().into()],
            "drop:test",
            &stop,
            |_| {},
            |t| {
                emitted.push(t);
                stop.store(true, Ordering::Relaxed);
                true
            },
            |p, s| {
                probes += 1;
                fake(p, s)
            },
        );
        assert!(summary.cancelled);
        assert_eq!(emitted.len(), 1);
        assert_eq!(probes, 1);
    }
    #[test]
    fn cancellation_during_probe_does_not_emit_late_track() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.mp3");
        std::fs::write(&path, []).unwrap();
        let stop = AtomicBool::new(false);
        let summary = scan(
            vec![path],
            "drop:test",
            &stop,
            |_| {},
            |_| panic!("Late track"),
            |p, s| {
                stop.store(true, Ordering::Relaxed);
                fake(p, s)
            },
        );
        assert!(summary.cancelled);
        assert_eq!(summary.loaded, 0);
    }
    #[test]
    fn cancellation_wakes_a_backpressured_producer() {
        let (tx, _rx) = mpsc::sync_channel(1);
        tx.send(LoadEvent::Finished(LoadSummary::default())).ok();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let task = std::thread::spawn(move || {
            send(&tx, LoadEvent::Finished(LoadSummary::default()), &flag)
        });
        stop.store(true, Ordering::Relaxed);
        assert!(!task.join().unwrap());
    }
    #[test]
    fn normally_drained_loader_preserves_the_playback_cancellation_token() {
        let mut load = FileLoad::start(vec![], "drop:done".into()).unwrap();
        let token = load.cancel_token();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match load.next_event() {
                Ok(LoadEvent::Finished(summary)) => {
                    assert_eq!(summary.loaded, 0);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(2));
                }
                _ => panic!("Expected normal completion"),
            }
        }
        load.finish();
        drop(load);
        assert!(!token.load(Ordering::Relaxed));
    }
    #[test]
    fn diagnostics_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let roots = (0..20)
            .map(|n| dir.path().join(format!("{n}.mp3")))
            .collect();
        let summary = scan(
            roots,
            "drop:test",
            &AtomicBool::new(false),
            |_| {},
            |_| true,
            fake,
        );
        assert_eq!(summary.skipped, 20);
        assert_eq!(summary.warnings.len(), 5);
    }
    #[cfg(unix)]
    #[test]
    fn skips_recursive_symlinks_but_resolves_explicit_links() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("1.mp3"), []).unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("1.mp3"), dir.path().join("link.mp3")).unwrap();
        let summary = scan(
            vec![dir.path().join("link.mp3"), dir.path().into()],
            "drop:test",
            &AtomicBool::new(false),
            |_| {},
            |_| true,
            fake,
        );
        assert_eq!(summary.loaded, 1);
        assert_eq!(summary.skipped, 2);
    }
}
