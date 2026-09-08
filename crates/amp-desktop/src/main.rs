use amp_core::{Album, DumpPreference, Library, Preferences, Track};
use amp_library::{FilesystemProvider, Store};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};
slint::include_modules!();
struct State {
    library: Library,
    selected: Option<usize>,
    playlist: Vec<Track>,
    queued: Vec<Track>,
    queue_start: usize,
    player: Option<amp_audio::Player>,
    store: Store,
    preferences: Preferences,
}
enum ScanEvent {
    Done(u64, anyhow::Result<Library>),
    Progress(u64, amp_library::ScanProgress),
    Metadata(u64, Album),
    MetadataDone(u64, anyhow::Result<()>),
}
fn spawn_metadata(
    mut library: Library,
    cache: PathBuf,
    generation: Arc<AtomicU64>,
    version: u64,
    enabled: Arc<AtomicBool>,
    tx: mpsc::Sender<ScanEvent>,
) {
    std::thread::spawn(move || {
        let result = Store::open(&cache).and_then(|store| {
            amp_library::metadata::enrich(
                &mut library,
                &store,
                &|| enabled.load(Ordering::Relaxed),
                &|| generation.load(Ordering::Relaxed) != version,
                &mut |album| {
                    let _ = tx.send(ScanEvent::Metadata(version, album.clone()));
                },
            )
        });
        let _ = tx.send(ScanEvent::MetadataDone(version, result));
    });
}
fn time(seconds: f64) -> SharedString {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60).into()
}
fn refresh_album_rows(ui: &PlayerWindow, state: &State) {
    ui.set_albums(ModelRc::new(VecModel::from(
        state
            .library
            .albums
            .iter()
            .map(|a| AlbumRow {
                title: a.title.clone().into(),
                artist: a.artist.clone().into(),
                detail: a.source_label.clone().into(),
            })
            .collect::<Vec<_>>(),
    )));
}
fn sort_albums(ui: &PlayerWindow, state: &mut State) {
    let selected = state
        .selected
        .and_then(|i| state.library.albums.get(i))
        .map(|a| a.id.clone());
    state.library.albums.sort_by_cached_key(|a| {
        let title = a.title.to_lowercase();
        let artist = a.artist.to_lowercase();
        if state.preferences.sort_by_artist {
            (artist, title, a.id.item.clone())
        } else {
            (title, artist, a.id.item.clone())
        }
    });
    state.selected = selected.and_then(|id| state.library.albums.iter().position(|a| a.id == id));
    refresh_album_rows(ui, state);
    ui.set_selected_album(state.selected.map(|i| i as i32).unwrap_or(-1));
}
fn albums(ui: &PlayerWindow, state: &State) {
    refresh_album_rows(ui, state);
    ui.set_selected_album(-1);
    ui.set_tracks(ModelRc::default());
    ui.set_album_title("Select an album".into());
    ui.set_album_artist(state.library.root.clone().into());
}
fn select(ui: &PlayerWindow, state: &mut State, index: usize) {
    let Some(album) = state.library.albums.get(index) else {
        return;
    };
    state.selected = Some(index);
    ui.set_active_track(-1);
    ui.set_selected_track(-1);
    ui.set_show_library(false);
    ui.set_selected_album(index as i32);
    ui.set_album_title(album.title.clone().into());
    ui.set_album_artist(album.artist.clone().into());
    ui.set_tracks(ModelRc::new(VecModel::from(
        album
            .tracks
            .iter()
            .map(|t| TrackRow {
                title: t.title.clone().into(),
                duration: time(t.duration_seconds),
                number: format!("{:02}", t.number).into(),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_status(album.source_label.clone().into());
}
fn start(ui: &PlayerWindow, state: &mut State, index: usize) {
    let Some(album) = state.selected.and_then(|i| state.library.albums.get(i)) else {
        return;
    };
    if index >= album.tracks.len() {
        return;
    }
    state.playlist = album.tracks.clone();
    state.queue_start = state.playlist.len();
    state.playlist.append(&mut state.queued);
    ui.set_queued(ModelRc::default());
    if state.player.is_none() {
        match amp_audio::Player::new() {
            Ok(p) => state.player = Some(p),
            Err(e) => {
                ui.set_status(format!("Audio output: {e:#}").into());
                return;
            }
        }
    }
    state
        .player
        .as_ref()
        .unwrap()
        .play(state.playlist.clone(), index);
    ui.set_status("Playing album".into());
}
fn media_controls(
    ui: &PlayerWindow,
    tx: mpsc::Sender<souvlaki::MediaControlEvent>,
) -> Result<souvlaki::MediaControls, String> {
    #[cfg(target_os = "windows")]
    let hwnd = {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let handle = ui.window().window_handle();
        match handle.window_handle().map_err(|e| e.to_string())?.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get() as *mut std::ffi::c_void),
            _ => return Err("No Windows media-control window".into()),
        }
    };
    #[cfg(not(target_os = "windows"))]
    let hwnd = {
        let _ = ui;
        None
    };
    let mut controls = souvlaki::MediaControls::new(souvlaki::PlatformConfig {
        dbus_name: "trash_amp",
        display_name: "Trash Amp",
        hwnd,
    })
    .map_err(|e| format!("{e:?}"))?;
    controls
        .attach(move |event| {
            let _ = tx.send(event);
        })
        .map_err(|e| format!("{e:?}"))?;
    Ok(controls)
}

fn media_event(ui: &PlayerWindow, state: &mut State, event: souvlaki::MediaControlEvent) {
    use souvlaki::MediaControlEvent as E;
    match event {
        E::Play | E::Toggle if state.player.is_none() || state.playlist.is_empty() => {
            start(ui, state, 0)
        }
        E::Play | E::Pause | E::Toggle | E::Stop => {
            if let Some(p) = &state.player {
                let playing = p.snapshot().playing;
                if matches!(event, E::Toggle) || (matches!(event, E::Play) != playing) {
                    p.toggle();
                }
                if matches!(event, E::Stop) {
                    p.seek(0.0);
                }
            }
        }
        E::Next => {
            if let Some(p) = &state.player {
                p.next();
            }
        }
        E::Previous => {
            if let Some(p) = &state.player {
                p.previous();
            }
        }
        E::SetPosition(position) => {
            if let Some(p) = &state.player {
                p.seek(position.0.as_secs_f64());
            }
        }
        _ => {}
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.iter().any(|a| a == "--help") {
        println!(
            "trash-amp [--demo] [--library PATH]\n--demo loads a generated 2,000-album layout fixture (no audio).\nTRASH_AMP_DATA_DIR overrides the local cache directory."
        );
        return Ok(());
    }
    let data = std::env::var_os("TRASH_AMP_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            directories::ProjectDirs::from("org", "AberrantWolf", "TrashAmp")
                .unwrap()
                .data_local_dir()
                .to_owned()
        });
    std::fs::create_dir_all(&data)?;
    let store = Store::open(&data.join("library.sqlite3"))?;
    let preferences = store.preferences()?;
    let library = if args.iter().any(|a| a == "--demo") {
        amp_library::demo_library()
    } else {
        store.library()?.unwrap_or_default()
    };
    let ui = PlayerWindow::new()?;
    ui.set_sort_order(i32::from(preferences.sort_by_artist));
    ui.set_reduced_motion(preferences.reduced_motion);
    ui.set_prefer_raw(preferences.dump_preference == DumpPreference::Redumper);
    ui.set_automatic_metadata(preferences.automatic_metadata);
    ui.set_text_scale(preferences.text_scale);
    let state = Rc::new(RefCell::new(State {
        library,
        selected: None,
        playlist: vec![],
        queued: vec![],
        queue_start: 0,
        player: None,
        store,
        preferences,
    }));
    sort_albums(&ui, &mut state.borrow_mut());
    albums(&ui, &state.borrow());
    let (tx, rx) = mpsc::channel();
    let generation = Arc::new(AtomicU64::new(0));
    let metadata_enabled = Arc::new(AtomicBool::new(
        state.borrow().preferences.automatic_metadata,
    ));
    let cache_path = data.join("library.sqlite3");
    let scan: Rc<dyn Fn(PathBuf)> = {
        let state = state.clone();
        let weak = ui.as_weak();
        let generation = generation.clone();
        let metadata_enabled = metadata_enabled.clone();
        let cache_path = cache_path.clone();
        let tx = tx.clone();
        Rc::new(move |root| {
            let version = generation.fetch_add(1, Ordering::Relaxed) + 1;
            let cancel = generation.clone();
            let tx = tx.clone();
            let preferences = state.borrow().preferences.clone();
            let metadata_enabled = metadata_enabled.clone();
            let cache_path = cache_path.clone();
            if let Some(ui) = weak.upgrade() {
                ui.set_scanning(true);
                ui.set_scan_progress(0.0);
                ui.set_status("Scanning music folder…".into());
            }
            std::thread::spawn(move || {
                let provider = FilesystemProvider { root, preferences };
                let mut last_report = std::time::Instant::now() - Duration::from_secs(1);
                let mut last_total = None;
                let result = provider.scan_with_progress(
                    &|| cancel.load(Ordering::Relaxed) != version,
                    &mut |n| {
                        if n.total != last_total
                            || n.total == Some(n.completed)
                            || last_report.elapsed() >= Duration::from_millis(100)
                        {
                            let _ = tx.send(ScanEvent::Progress(version, n));
                            last_total = n.total;
                            last_report = std::time::Instant::now();
                        }
                    },
                );
                match result {
                    Ok(library) => {
                        let _ = tx.send(ScanEvent::Done(version, Ok(library.clone())));
                        spawn_metadata(library, cache_path, cancel, version, metadata_enabled, tx);
                    }
                    Err(error) => {
                        let _ = tx.send(ScanEvent::Done(version, Err(error)));
                    }
                }
            });
        })
    };
    {
        let scan = scan.clone();
        ui.on_open_folder(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Open music library")
                .pick_folder()
            {
                scan(path)
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let generation = generation.clone();
        ui.on_demo(move || {
            generation.fetch_add(1, Ordering::Relaxed);
            let mut s = state.borrow_mut();
            s.library = amp_library::demo_library();
            s.selected = None;
            if let Some(ui) = weak.upgrade() {
                ui.set_scanning(false);
                sort_albums(&ui, &mut s);
                albums(&ui, &s);
                ui.set_status(
                    "2,000 generated albums. This fixture contains no playable audio.".into(),
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_select_album(move |i| {
            if let Some(ui) = weak.upgrade() {
                select(&ui, &mut state.borrow_mut(), i as usize)
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_play_track(move |i| {
            if let Some(ui) = weak.upgrade() {
                start(&ui, &mut state.borrow_mut(), i as usize)
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_toggle_play(move || {
            let mut s = state.borrow_mut();
            if s.playlist.is_empty() || s.player.is_none() {
                if let Some(ui) = weak.upgrade() {
                    start(&ui, &mut s, 0)
                }
            } else if let Some(p) = &s.player {
                p.toggle()
            }
        });
    }
    {
        let state = state.clone();
        ui.on_next(move || {
            if let Some(p) = &state.borrow().player {
                p.next()
            }
        });
    }
    {
        let state = state.clone();
        ui.on_previous(move || {
            if let Some(p) = &state.borrow().player {
                p.previous()
            }
        });
    }
    {
        let state = state.clone();
        ui.on_seek(move |fraction| {
            let s = state.borrow();
            if let Some(p) = &s.player
                && let Some(t) = s.playlist.get(p.snapshot().track)
            {
                p.seek(fraction as f64 * t.duration_seconds)
            }
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.on_enqueue(move |i| {
            let mut s = state.borrow_mut();
            let t = s
                .selected
                .and_then(|a| s.library.albums.get(a))
                .and_then(|a| a.tracks.get(i as usize))
                .cloned();
            if let Some(t) = t {
                if s.playlist.is_empty() {
                    s.queued.push(t);
                } else {
                    s.playlist.push(t.clone());
                    if let Some(p) = &s.player {
                        p.append(vec![t]);
                    }
                }
                if let Some(ui) = weak.upgrade() {
                    ui.set_queued(ModelRc::new(VecModel::from(
                        s.queued
                            .iter()
                            .map(|t| SharedString::from(&t.title))
                            .collect::<Vec<_>>(),
                    )));
                    ui.set_status("Added to up next".into());
                }
            }
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.on_clear_queue(move || {
            let mut s = state.borrow_mut();
            s.queued.clear();
            let keep = s
                .player
                .as_ref()
                .map(|p| s.queue_start.max(p.snapshot().track + 1))
                .unwrap_or(0);
            s.playlist.truncate(keep);
            if let Some(p) = &s.player {
                p.truncate(keep);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_queued(ModelRc::default())
            }
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        let metadata_enabled = metadata_enabled.clone();
        ui.on_preferences_changed(move || {
            if let Some(ui) = weak.upgrade() {
                let mut s = state.borrow_mut();
                s.preferences.text_scale = ui.get_text_scale();
                s.preferences.reduced_motion = ui.get_reduced_motion();
                s.preferences.automatic_metadata = ui.get_automatic_metadata();
                metadata_enabled.store(s.preferences.automatic_metadata, Ordering::Relaxed);
                s.preferences.dump_preference = if ui.get_prefer_raw() {
                    DumpPreference::Redumper
                } else {
                    DumpPreference::BinCue
                };
                if let Err(e) = s.store.save_preferences(&s.preferences) {
                    ui.set_status(format!("Could not save settings: {e}").into())
                } else {
                    ui.set_status(
                        "Settings saved. Reopen for dump selection or new metadata lookups.".into(),
                    )
                }
            }
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.on_sort_changed(move || {
            if let Some(ui) = weak.upgrade() {
                let mut s = state.borrow_mut();
                s.preferences.sort_by_artist = ui.get_sort_order() == 1;
                sort_albums(&ui, &mut s);
                if let Err(e) = s.store.save_preferences(&s.preferences) {
                    ui.set_status(e.to_string().into());
                }
            }
        });
    }
    let (media_tx, media_rx) = mpsc::channel();
    let mut controls = None;
    let mut media_initialized = false;
    let mut last_media = None;
    let mut scan_files = None;
    let timer = slint::Timer::default();
    {
        let state = state.clone();
        let weak = ui.as_weak();
        let generation = generation.clone();
        let metadata_enabled = metadata_enabled.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(50),
            move || {
                let Some(ui) = weak.upgrade() else { return };
                let size = ui.window().size().to_logical(ui.window().scale_factor());
                ui.set_wide(size.width > 720.0 * ui.get_text_scale());
                ui.set_short(size.height < 420.0 * ui.get_text_scale());
                if !media_initialized {
                    media_initialized = true;
                    match media_controls(&ui, media_tx.clone()) {
                        Ok(c) => controls = Some(c),
                        Err(e) => { eprintln!("Media keys unavailable: {e}"); ui.set_status(format!("Media keys unavailable: {e}").into()); }
                    }
                }
                let mut s = state.borrow_mut();
                while let Ok(event) = media_rx.try_recv() { media_event(&ui, &mut s, event); }
                while let Ok(event) = rx.try_recv() {
                    match event {
                        ScanEvent::Progress(v, n) if v == generation.load(Ordering::Relaxed) => {
                            scan_files = n.total;
                            ui.set_scan_progress(n.total.filter(|t| *t > 0).map(|t| n.completed as f32 / t as f32).unwrap_or(0.0));
                            ui.set_status(match n.total {
                                Some(total) => format!("Scanning… {} / {total} files", n.completed),
                                None => format!("Finding files… {} found", n.discovered),
                            }.into());
                        }
                        ScanEvent::Done(v, result) if v == generation.load(Ordering::Relaxed) => {
                            ui.set_scanning(false);
                            match result {
                                Ok(library) => {
                                    let status = format!(
                                        "{} files • {} albums • {} scan warnings • checking metadata…",
                                        scan_files.unwrap_or(0), library.albums.len(),
                                        library.warnings.len()
                                    );
                                    for warning in &library.warnings {
                                        eprintln!("{warning}");
                                    }
                                    let saved = s.store.save_library(&library);
                                    s.library = library;
                                    s.selected = None;
                                    sort_albums(&ui, &mut s);
                                    albums(&ui, &s);
                                    ui.set_status(
                                        match saved {
                                            Ok(()) => status,
                                            Err(e) => format!("{status}; cache save failed: {e}"),
                                        }
                                        .into(),
                                    );
                                }
                                Err(e) => ui.set_status(format!("Library unchanged: {e:#}").into()),
                            }
                        }
                        ScanEvent::Metadata(v, album)
                            if v == generation.load(Ordering::Relaxed) =>
                        {
                            if let Some(index) =
                                s.library.albums.iter().position(|a| a.id == album.id)
                            {
                                // Playback access/order remain unchanged; refresh display names only.
                                let state = &mut *s;
                                for t in state.playlist.iter_mut().chain(state.queued.iter_mut()) {
                                    if let Some(updated) =
                                        album.tracks.iter().find(|n| n.id == t.id)
                                    {
                                        t.title = updated.title.clone();
                                        t.artist = updated.artist.clone();
                                    }
                                }
                                let selected_updated = s.selected == Some(index);
                                let order_changed = s.library.albums[index].title != album.title || s.library.albums[index].artist != album.artist;
                                s.library.albums[index] = album;
                                if let Some(model) = ui
                                    .get_albums()
                                    .as_any()
                                    .downcast_ref::<VecModel<AlbumRow>>()
                                {
                                    let a = &s.library.albums[index];
                                    model.set_row_data(
                                        index,
                                        AlbumRow {
                                            title: a.title.clone().into(),
                                            artist: a.artist.clone().into(),
                                            detail: a.source_label.clone().into(),
                                        },
                                    );
                                }
                                if order_changed { sort_albums(&ui, &mut s); }
                                if selected_updated && let Some(index) = s.selected {
                                    let show_library = ui.get_show_library();
                                    select(&ui, &mut s, index);
                                    ui.set_show_library(show_library);
                                }
                            }
                        }
                        ScanEvent::MetadataDone(v, result)
                            if v == generation.load(Ordering::Relaxed) =>
                        {
                            if let Err(e) = s.store.save_library(&s.library) {
                                ui.set_status(format!("Metadata cache save failed: {e}").into());
                            } else if let Err(e) = result
                                && metadata_enabled.load(Ordering::Relaxed)
                            {
                                ui.set_status(
                                    format!("Online metadata paused: {e}; local library available")
                                        .into(),
                                );
                            } else {
                                let unmatched = s.library.albums.iter().filter(|a| a.source_label.contains("MusicBrainz: no-match")).count();
                                let ambiguous = s.library.albums.iter().filter(|a| a.source_label.contains("MusicBrainz: ambiguous")).count();
                                let files = scan_files.map(|n| format!("{n} files • ")).unwrap_or_default();
                                let mode = if metadata_enabled.load(Ordering::Relaxed) { "metadata checked" } else { "cached metadata checked; online disabled" };
                                ui.set_status(format!("{files}{} albums • {mode} • {unmatched} unmatched • {ambiguous} ambiguous", s.library.albums.len()).into());
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(player) = &s.player {
                    let snapshot = player.snapshot();
                    let upcoming = s
                        .playlist
                        .iter()
                        .skip(s.queue_start.max(snapshot.track + 1))
                        .map(|t| SharedString::from(&t.title))
                        .collect::<Vec<_>>();
                    use slint::Model;
                    if ui.get_queued().iter().collect::<Vec<_>>() != upcoming {
                        ui.set_queued(ModelRc::new(VecModel::from(upcoming)));
                    }
                    ui.set_active_track(s.selected.and_then(|i| s.library.albums.get(i))
                        .and_then(|a| s.playlist.get(snapshot.track).and_then(|t| a.tracks.iter().position(|r| r.id == t.id)))
                        .map(|i| i as i32).unwrap_or(-1));
                    ui.set_playing(snapshot.playing);
                    ui.set_level(snapshot.level);
                    if let (Some(c), Some(t)) = (&mut controls, s.playlist.get(snapshot.track)) {
                        let identity = (t.id.clone(), t.title.clone(), t.artist.clone(), snapshot.playing, snapshot.seconds as u64);
                        if last_media.as_ref() != Some(&identity) {
                            let _ = c.set_metadata(souvlaki::MediaMetadata {
                                title: Some(&t.title), artist: Some(&t.artist),
                                duration: Some(Duration::from_secs_f64(t.duration_seconds.max(0.0))), ..Default::default()
                            });
                            let progress = Some(souvlaki::MediaPosition(Duration::from_secs_f64(snapshot.seconds.max(0.0))));
                            let _ = c.set_playback(if snapshot.playing { souvlaki::MediaPlayback::Playing { progress } } else { souvlaki::MediaPlayback::Paused { progress } });
                            last_media = Some(identity);
                        }
                    }
                    if let Some(track) = s.playlist.get(snapshot.track) {
                        ui.set_now_title(track.title.clone().into());
                        ui.set_now_artist(track.artist.clone().into());
                        ui.set_elapsed(time(snapshot.seconds));
                        ui.set_duration(time(track.duration_seconds));
                        if !ui.get_scrubbing() {
                            ui.set_progress(if track.duration_seconds > 0.0 {
                                (snapshot.seconds / track.duration_seconds).clamp(0.0, 1.0) as f32
                            } else {
                                0.0
                            });
                        }
                    }
                    if let Some(error) = player.error() {
                        ui.set_status(error.into());
                    }
                }
            },
        );
    }
    if !args.iter().any(|a| a == "--library" || a == "--demo") {
        spawn_metadata(
            state.borrow().library.clone(),
            cache_path,
            generation.clone(),
            generation.load(Ordering::Relaxed),
            metadata_enabled,
            tx,
        );
    }
    if let Some(index) = args.iter().position(|a| a == "--library")
        && let Some(path) = args.get(index + 1)
    {
        scan(PathBuf::from(path))
    }
    if args.iter().any(|a| a == "--settings") {
        ui.set_settings(true);
    }
    // Reproducible visual QA uses the actual Slint renderer, not a separate mockup.
    if let Some(i) = args.iter().position(|a| a == "--size")
        && let Some((w, h)) = args.get(i + 1).and_then(|s| s.split_once('x'))
    {
        ui.window()
            .set_size(slint::LogicalSize::new(w.parse()?, h.parse()?));
    }
    if let Some(i) = args.iter().position(|a| a == "--text-scale")
        && let Some(value) = args.get(i + 1)
    {
        ui.set_text_scale(value.parse::<f32>()?.clamp(1.0, 2.0));
    }
    if args.iter().any(|a| a == "--select-first") {
        select(&ui, &mut state.borrow_mut(), 0);
    }
    if let Some(path) = args
        .iter()
        .position(|a| a == "--snapshot")
        .and_then(|i| args.get(i + 1))
        .cloned()
    {
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_secs(1), move || {
            if let Some(ui) = weak.upgrade() {
                match ui.window().take_snapshot() {
                    Ok(pixels) => {
                        if let Err(e) = image::save_buffer(
                            &path,
                            pixels.as_bytes(),
                            pixels.width(),
                            pixels.height(),
                            image::ColorType::Rgba8,
                        ) {
                            eprintln!("Snapshot failed: {e}");
                        }
                    }
                    Err(e) => eprintln!("Snapshot failed: {e}"),
                }
            }
            let _ = slint::quit_event_loop();
        });
    }
    ui.run()?;
    generation.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    slint::slint! {
        import { TransportSeek } from "ui/transport-seek.slint";
        export component SeekHarness inherits Window {
            in-out property <float> position <=> seek.position;
            out property <float> displayed-position: seek.displayed-position;
            out property <bool> scrubbing: seek.scrubbing;
            seek := TransportSeek { width: 100%; height: 100%; }
        }
    }
    slint::slint! {
        import { TrackItem } from "ui/track-item.slint";
        export component TrackHarness inherits Window {
            callback play(); callback select(); callback enqueue();
            TrackItem { width: 100%; height: 48px; track: {title: "A track", number: "01", duration: "3:00"};
                play => { root.play(); } select => { root.select(); } enqueue => { root.enqueue(); }
            }
        }
    }
    #[test]
    fn album_pane_restores_desktop_width_and_remembers_drag_across_phone_mode() {
        use i_slint_backend_testing::ElementHandle;
        use slint::platform::PointerEventButton;
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.set_album_title(
            "A long album title that should wrap instead of squeezing the album browser".into(),
        );
        ui.window().set_size(slint::LogicalSize::new(980.0, 680.0));
        ui.show().unwrap();
        let pane_width = || {
            ElementHandle::find_by_accessible_label(&ui, "Album browser")
                .next()
                .unwrap()
                .size()
                .width
        };
        assert!((pane_width() - 280.0).abs() < 1.0);
        let roundtrip = |expected: f32| {
            for show_library in [true, false] {
                ui.set_show_library(show_library);
                ui.set_wide(false);
                ui.window().set_size(slint::LogicalSize::new(390.0, 760.0));
                slint::platform::update_timers_and_animations();
                if show_library {
                    assert!((pane_width() - 358.0).abs() < 1.0);
                }
                ui.window().set_size(slint::LogicalSize::new(980.0, 680.0));
                ui.set_wide(true);
                slint::platform::update_timers_and_animations();
                assert!(
                    (pane_width() - expected).abs() < 1.0,
                    "actual {} expected {expected}",
                    pane_width()
                );
            }
        };
        roundtrip(280.0);
        let divider = ElementHandle::find_by_accessible_label(&ui, "Resize album browser")
            .next()
            .unwrap();
        let position = divider.absolute_position();
        divider.mock_drag(
            slint::LogicalPosition::new(
                position.x + 88.0,
                position.y + divider.size().height / 2.0,
            ),
            PointerEventButton::Left,
        );
        assert!(
            (pane_width() - 360.0).abs() < 2.0,
            "drag width {}",
            pane_width()
        );
        roundtrip(360.0);
        // A smaller desktop temporarily clamps the pane without erasing its saved width.
        ui.set_library_width(420.0);
        ui.window().set_size(slint::LogicalSize::new(740.0, 680.0));
        slint::platform::update_timers_and_animations();
        assert!(
            (pane_width() - 333.0).abs() < 1.0,
            "clamped width {}",
            pane_width()
        );
        ui.window().set_size(slint::LogicalSize::new(980.0, 680.0));
        slint::platform::update_timers_and_animations();
        assert!((pane_width() - 420.0).abs() < 1.0);
    }
    #[test]
    fn desktop_row_selects_double_click_plays_and_hover_button_plays() {
        use slint::platform::{PointerEventButton, WindowEvent};
        use std::cell::Cell;
        i_slint_backend_testing::init_no_event_loop();
        let ui = TrackHarness::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(500.0, 48.0));
        let played = Rc::new(Cell::new(0));
        let selected = Rc::new(Cell::new(0));
        let p = played.clone();
        ui.on_play(move || p.set(p.get() + 1));
        let s = selected.clone();
        ui.on_select(move || s.set(s.get() + 1));
        ui.show().unwrap();
        let click = |x| {
            let position = slint::LogicalPosition::new(x, 24.0);
            ui.window()
                .dispatch_event(WindowEvent::PointerMoved { position });
            ui.window().dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(30));
            ui.window().dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
        };
        click(150.0);
        assert_eq!(selected.get(), 1);
        assert_eq!(played.get(), 0);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
        click(150.0);
        assert_eq!(played.get(), 1);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(1));
        click(18.0);
        assert_eq!(played.get(), 2);
    }
    #[test]
    fn keyboard_transport_and_sort_preserve_album_selection() {
        use slint::platform::WindowEvent;
        use std::cell::Cell;
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.show().unwrap();
        let toggled = Rc::new(Cell::new(0));
        let next = Rc::new(Cell::new(0));
        let previous = Rc::new(Cell::new(0));
        let t = toggled.clone();
        ui.on_toggle_play(move || t.set(t.get() + 1));
        let n = next.clone();
        ui.on_next(move || n.set(n.get() + 1));
        let p = previous.clone();
        ui.on_previous(move || p.set(p.get() + 1));
        for text in [
            SharedString::from(" "),
            slint::platform::Key::RightArrow.into(),
            slint::platform::Key::LeftArrow.into(),
        ] {
            ui.window()
                .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            ui.window()
                .dispatch_event(WindowEvent::KeyReleased { text });
        }
        assert_eq!((toggled.get(), next.get(), previous.get()), (1, 1, 1));
        let dir =
            std::env::temp_dir().join(format!("trash-amp-sort-test-{}.db", std::process::id()));
        let mut state = State {
            library: amp_library::demo_library(),
            selected: Some(0),
            playlist: vec![],
            queued: vec![],
            queue_start: 0,
            player: None,
            store: Store::open(&dir).unwrap(),
            preferences: Preferences::default(),
        };
        let id = state.library.albums[0].id.clone();
        sort_albums(&ui, &mut state);
        assert_eq!(state.library.albums[state.selected.unwrap()].id, id);
        assert!(
            state
                .library
                .albums
                .windows(2)
                .all(|w| w[0].title.to_lowercase() <= w[1].title.to_lowercase())
        );
        state.preferences.sort_by_artist = true;
        sort_albums(&ui, &mut state);
        assert_eq!(state.library.albums[state.selected.unwrap()].id, id);
        assert!(
            state
                .library
                .albums
                .windows(2)
                .all(|w| w[0].artist.to_lowercase() <= w[1].artist.to_lowercase())
        );
        drop(state);
        let _ = std::fs::remove_file(dir);
    }
    #[test]
    fn playhead_follows_clock_after_user_scrubs_and_restarts() {
        use slint::platform::{PointerEventButton, WindowEvent};
        i_slint_backend_testing::init_no_event_loop();
        let seek = SeekHarness::new().unwrap();
        seek.window().set_size(slint::LogicalSize::new(400.0, 40.0));
        seek.show().unwrap();
        seek.set_position(0.1);
        for target in [0.5, 0.75] {
            let position = slint::LogicalPosition::new(400.0 * target, 20.0);
            seek.window()
                .dispatch_event(WindowEvent::PointerMoved { position });
            seek.window().dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
            seek.window().dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
            assert!(
                (seek.get_position() - target).abs() < 0.1,
                "pointer must change the real slider: value={}, target={target}, size={:?}",
                seek.get_position(),
                seek.window().size()
            );
            assert!(!seek.get_scrubbing());
            seek.set_position(0.0);
            assert_eq!(seek.get_displayed_position(), 0.0);
            seek.set_position(0.2);
            assert_eq!(seek.get_displayed_position(), 0.2);
            seek.set_position(0.3);
            assert_eq!(seek.get_displayed_position(), 0.3);
        }
    }
}
