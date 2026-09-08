use amp_core::{DumpPreference, Library, Preferences, Track};
use amp_library::{FilesystemProvider, Store};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
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
    Progress(u64, usize),
}
fn time(seconds: f64) -> SharedString {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60).into()
}
fn albums(ui: &PlayerWindow, state: &State) {
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
    albums(&ui, &state.borrow());
    let (tx, rx) = mpsc::channel();
    let generation = Arc::new(AtomicU64::new(0));
    let scan: Rc<dyn Fn(PathBuf)> = {
        let state = state.clone();
        let weak = ui.as_weak();
        let generation = generation.clone();
        Rc::new(move |root| {
            let version = generation.fetch_add(1, Ordering::Relaxed) + 1;
            let cancel = generation.clone();
            let tx = tx.clone();
            let preferences = state.borrow().preferences.clone();
            if let Some(ui) = weak.upgrade() {
                ui.set_scanning(true);
                ui.set_status("Scanning music folder…".into());
            }
            std::thread::spawn(move || {
                let provider = FilesystemProvider { root, preferences };
                let result = provider.scan_with_progress(
                    &|| cancel.load(Ordering::Relaxed) != version,
                    &mut |n| {
                        let _ = tx.send(ScanEvent::Progress(version, n));
                    },
                );
                let _ = tx.send(ScanEvent::Done(version, result));
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
        ui.on_preferences_changed(move || {
            if let Some(ui) = weak.upgrade() {
                let mut s = state.borrow_mut();
                s.preferences.text_scale = ui.get_text_scale();
                s.preferences.reduced_motion = ui.get_reduced_motion();
                s.preferences.automatic_metadata = ui.get_automatic_metadata();
                s.preferences.dump_preference = if ui.get_prefer_raw() {
                    DumpPreference::Redumper
                } else {
                    DumpPreference::BinCue
                };
                if let Err(e) = s.store.save_preferences(&s.preferences) {
                    ui.set_status(format!("Could not save settings: {e}").into())
                } else {
                    ui.set_status(
                        "Settings saved. Reopen the folder to change dump selection.".into(),
                    )
                }
            }
        });
    }
    let timer = slint::Timer::default();
    {
        let state = state.clone();
        let weak = ui.as_weak();
        let generation = generation.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(50),
            move || {
                let Some(ui) = weak.upgrade() else { return };
                let size = ui.window().size().to_logical(ui.window().scale_factor());
                ui.set_wide(size.width > 720.0 * ui.get_text_scale());
                ui.set_short(size.height < 420.0 * ui.get_text_scale());
                let mut s = state.borrow_mut();
                while let Ok(event) = rx.try_recv() {
                    match event {
                        ScanEvent::Progress(v, n) if v == generation.load(Ordering::Relaxed) => {
                            ui.set_status(format!("Scanning… {n} files inspected").into())
                        }
                        ScanEvent::Done(v, result) if v == generation.load(Ordering::Relaxed) => {
                            ui.set_scanning(false);
                            match result {
                                Ok(library) => {
                                    let status = format!(
                                        "{} albums • {} scan warnings",
                                        library.albums.len(),
                                        library.warnings.len()
                                    );
                                    for warning in &library.warnings {
                                        eprintln!("{warning}");
                                    }
                                    let saved = s.store.save_library(&library);
                                    s.library = library;
                                    s.selected = None;
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
                    ui.set_playing(snapshot.playing);
                    ui.set_level(snapshot.level);
                    if let Some(track) = s.playlist.get(snapshot.track) {
                        ui.set_now_title(track.title.clone().into());
                        ui.set_now_artist(track.artist.clone().into());
                        ui.set_elapsed(time(snapshot.seconds));
                        ui.set_duration(time(track.duration_seconds));
                        ui.set_progress(if track.duration_seconds > 0.0 {
                            (snapshot.seconds / track.duration_seconds).clamp(0.0, 1.0) as f32
                        } else {
                            0.0
                        });
                    }
                    if let Some(error) = player.error() {
                        ui.set_status(error.into());
                    }
                }
            },
        );
    }
    if let Some(index) = args.iter().position(|a| a == "--library")
        && let Some(path) = args.get(index + 1)
    {
        scan(PathBuf::from(path))
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
