//! Presentation/navigation adapter. Row positions never serve as catalog identities.
use super::*;
use amp_core::browse::{CatalogIndex, Entry, Results, Target};
use std::time::Instant;

#[derive(Clone, Default)]
struct Location {
    query: String,
    section: Option<usize>,
    detail: Option<Target>,
    browser_y: f32,
    detail_y: f32,
    track_y: f32,
    selected_track: i32,
    show_library: bool,
    selected_result: Option<Target>,
}
#[derive(Clone)]
enum Action {
    Open(Target),
    More(usize),
    None,
}
struct SearchReply {
    query_version: u64,
    catalog_version: u64,
    results: Results,
}
pub(super) struct Browser {
    index: Arc<CatalogIndex>,
    query: String,
    section: Option<usize>,
    detail: Option<Target>,
    selected_result: Option<Target>,
    history: Vec<Location>,
    rows: Vec<Action>,
    detail_rows: Vec<Action>,
    results: Results,
    query_version: u64,
    catalog_version: u64,
    due: Option<Instant>,
    pending_open: Option<(Instant, Target)>,
    tx: mpsc::Sender<SearchReply>,
    rx: mpsc::Receiver<SearchReply>,
}
impl Default for Browser {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            index: Arc::default(),
            query: String::new(),
            section: None,
            detail: None,
            selected_result: None,
            history: vec![],
            rows: vec![],
            detail_rows: vec![],
            results: Results::default(),
            query_version: 0,
            catalog_version: 0,
            due: None,
            pending_open: None,
            tx,
            rx,
        }
    }
}
fn decorate(
    rows: &mut [BrowserRow],
    actions: &[Action],
    artwork: &std::collections::BTreeMap<String, String>,
) {
    for (row, action) in rows.iter_mut().zip(actions) {
        let id = match action {
            Action::Open(Target::Album(id)) | Action::Open(Target::Track { album: id, .. }) => {
                Some(id)
            }
            _ => None,
        };
        row.artwork = id
            .and_then(|id| artwork.get(&id.item))
            .and_then(|p| slint::Image::load_from_path(std::path::Path::new(p)).ok())
            .unwrap_or_default();
    }
}
fn row(entry: &Entry, compact: bool) -> BrowserRow {
    let kind = match &entry.target {
        Target::Album(_) => {
            if compact {
                5
            } else {
                1
            }
        }
        Target::Artist(_) => 2,
        Target::Track { .. } => 3,
    };
    BrowserRow {
        artwork: Default::default(),
        kind,
        title: entry.title.clone().into(),
        artist: entry.artist.clone().into(),
        subtitle: match kind {
            2 => format!(
                "{} album{}",
                entry.album_count,
                if entry.album_count == 1 { "" } else { "s" }
            )
            .into(),
            3 => entry.album.clone().into(),
            _ => entry.artist.clone().into(),
        },
        duration: if kind == 3 {
            time(entry.seconds)
        } else {
            SharedString::default()
        },
    }
}
fn label(title: String, kind: i32) -> BrowserRow {
    BrowserRow {
        title: title.into(),
        kind,
        ..Default::default()
    }
}
fn append_entries(
    rows: &mut Vec<BrowserRow>,
    actions: &mut Vec<Action>,
    entries: &[Entry],
    limit: usize,
) {
    for entry in entries.iter().take(limit) {
        rows.push(row(entry, false));
        actions.push(Action::Open(entry.target.clone()));
    }
}
fn render_browser(ui: &PlayerWindow, s: &mut State) {
    let b = &mut s.browser;
    let mut rows = vec![];
    let mut actions = vec![];
    if b.query.trim().is_empty() {
        let entries = if s.preferences.browse_artists {
            b.index.album_artists()
        } else {
            b.index.albums.clone()
        };
        append_entries(&mut rows, &mut actions, &entries, usize::MAX);
        if rows.is_empty() {
            rows.push(label("Open a library to begin".into(), 6));
            actions.push(Action::None);
        }
    } else {
        for (i, (title, entries)) in [
            ("Artists", &b.results.artists),
            ("Albums", &b.results.albums),
            ("Tracks", &b.results.tracks),
        ]
        .into_iter()
        .enumerate()
        {
            if b.section.is_some_and(|section| section != i) || entries.is_empty() {
                continue;
            }
            rows.push(label(format!("{title} ({})", entries.len()), 0));
            actions.push(Action::None);
            append_entries(
                &mut rows,
                &mut actions,
                entries,
                if b.section.is_some() { usize::MAX } else { 5 },
            );
            if b.section.is_none() && entries.len() > 5 {
                rows.push(label(format!("Show all {} {title}", entries.len()), 4));
                actions.push(Action::More(i));
            }
        }
        if rows.is_empty() {
            rows.push(label(
                if b.due.is_some() {
                    "Searching…"
                } else {
                    "No results"
                }
                .into(),
                6,
            ));
            actions.push(Action::None);
        }
    }
    b.rows = actions;
    decorate(&mut rows, &b.rows, &s.artwork);
    ui.set_browser_rows(ModelRc::new(VecModel::from(rows)));
    ui.set_browser_selected(
        b.selected_result
            .as_ref()
            .and_then(|target| {
                b.rows
                    .iter()
                    .position(|a| matches!(a, Action::Open(t) if t == target))
            })
            .map_or(-1, |i| i as i32),
    );
    ui.set_section_open(b.section.is_some());
    ui.set_can_back(!b.history.is_empty());
}
fn render_artist(ui: &PlayerWindow, s: &mut State, name: &str) {
    let page = s.browser.index.artist(name);
    ui.set_artist_view(true);
    ui.set_album_title(name.into());
    ui.set_album_artist(
        format!(
            "{} albums · {} appearances",
            page.albums.len(),
            page.appearances.iter().map(|(_, t)| t.len()).sum::<usize>()
        )
        .into(),
    );
    let mut rows = vec![label("Albums".into(), 0)];
    let mut actions = vec![Action::None];
    append_entries(&mut rows, &mut actions, &page.albums, usize::MAX);
    if page.albums.is_empty() {
        rows.push(label("No albums as album artist".into(), 6));
        actions.push(Action::None);
    }
    if !page.appearances.is_empty() {
        rows.push(label("Appears on".into(), 0));
        actions.push(Action::None);
        for (album, tracks) in page.appearances {
            rows.push(row(&album, true));
            actions.push(Action::Open(album.target));
            append_entries(&mut rows, &mut actions, &tracks, usize::MAX);
        }
    }
    decorate(&mut rows, &actions, &s.artwork);
    s.browser.detail_rows = actions;
    ui.set_artist_rows(ModelRc::new(VecModel::from(rows)));
}
fn location(ui: &PlayerWindow, s: &State) -> Location {
    Location {
        query: s.browser.query.clone(),
        section: s.browser.section,
        detail: s.browser.detail.clone(),
        browser_y: ui.get_browser_y(),
        detail_y: ui.get_detail_y(),
        track_y: ui.get_track_y(),
        selected_track: ui.get_selected_track(),
        show_library: ui.get_show_library(),
        selected_result: s.browser.selected_result.clone(),
    }
}
fn push(ui: &PlayerWindow, s: &mut State) {
    s.browser.history.push(location(ui, s));
    ui.set_can_back(true);
}
fn show_target(ui: &PlayerWindow, s: &mut State, target: &Target) {
    match target {
        Target::Artist(name) => {
            render_artist(ui, s, name);
            ui.set_show_library(false);
        }
        Target::Album(id) | Target::Track { album: id, .. } => {
            if let Some(index) = s.library.albums.iter().position(|a| &a.id == id) {
                select(ui, s, index);
                if let Target::Track { track, .. } = target {
                    if let Some(i) = s.library.albums[index]
                        .tracks
                        .iter()
                        .position(|t| &t.id == track)
                    {
                        ui.set_selected_track(i as i32);
                        ui.set_track_y(-(i as f32 * 48.0 * ui.get_text_scale()));
                    }
                } else {
                    ui.set_track_y(0.0);
                }
            }
        }
    }
}
fn open(ui: &PlayerWindow, s: &mut State, target: Target) {
    s.browser.pending_open = None;
    if s.browser.detail.as_ref() == Some(&target) && !ui.get_show_library() {
        return;
    }
    ui.set_search_editing(false);
    push(ui, s);
    s.browser.selected_result = Some(target.clone());
    s.browser.detail = Some(target.clone());
    ui.set_detail_y(0.0);
    show_target(ui, s, &target);
    ui.set_browser_selected(
        s.browser
            .rows
            .iter()
            .position(|a| matches!(a, Action::Open(t) if t == &target))
            .map_or(-1, |i| i as i32),
    );
}
fn schedule(s: &mut State) {
    s.browser.query_version += 1;
    s.browser.due = Some(Instant::now() + Duration::from_millis(100));
}
pub(super) fn rebuild(ui: &PlayerWindow, s: &mut State, reset: bool) {
    s.browser.index = Arc::new(CatalogIndex::new(&s.library));
    s.browser.catalog_version += 1;
    if reset {
        s.browser.pending_open = None;
        s.browser.query.clear();
        s.browser.section = None;
        s.browser.detail = None;
        s.browser.history.clear();
        s.browser.selected_result = None;
        ui.set_search_text("".into());
        ui.set_artist_view(false);
        ui.set_show_library(true);
        ui.set_browser_y(0.0);
        ui.set_detail_y(0.0);
        ui.set_track_y(0.0);
    }
    if !s.browser.query.trim().is_empty() {
        schedule(s);
    }
    if let Some(Target::Artist(name)) = s.browser.detail.clone() {
        render_artist(ui, s, &name);
    }
    render_browser(ui, s);
}
pub(super) fn poll(ui: &PlayerWindow, s: &mut State) {
    if s.browser
        .pending_open
        .as_ref()
        .is_some_and(|(due, _)| Instant::now() >= *due)
    {
        let (_, target) = s.browser.pending_open.take().unwrap();
        open(ui, s, target);
    }
    if s.browser.due.is_some_and(|due| Instant::now() >= due) {
        s.browser.due = None;
        let index = s.browser.index.clone();
        let query = s.browser.query.clone();
        let query_version = s.browser.query_version;
        let catalog_version = s.browser.catalog_version;
        let tx = s.browser.tx.clone();
        std::thread::spawn(move || {
            let results = index.search(&query);
            let _ = tx.send(SearchReply {
                query_version,
                catalog_version,
                results,
            });
        });
    }
    while let Ok(reply) = s.browser.rx.try_recv() {
        if reply.query_version == s.browser.query_version
            && reply.catalog_version == s.browser.catalog_version
        {
            s.browser.results = reply.results;
            render_browser(ui, s);
        }
    }
}
pub(super) fn resolve_track(s: &State, target: &Target) -> Option<Track> {
    if let Target::Track { album, track } = target {
        s.library
            .albums
            .iter()
            .find(|a| &a.id == album)?
            .tracks
            .iter()
            .find(|t| &t.id == track)
            .cloned()
    } else {
        None
    }
}
/// The explicit queue can live before playback or in the tail of the active sequence.
fn remaining_queue(s: &State) -> Vec<Track> {
    let played = s.player.as_ref().map_or(0, |p| p.snapshot().track + 1);
    remaining_queue_after(s, played)
}
fn remaining_queue_after(s: &State, played: usize) -> Vec<Track> {
    s.playlist
        .iter()
        .skip(s.queue_start.max(played))
        .chain(&s.queued)
        .cloned()
        .collect()
}
fn single_track_sequence(s: &State, target: &Target) -> Option<Vec<Track>> {
    let mut sequence = vec![resolve_track(s, target)?];
    sequence.extend(remaining_queue(s));
    Some(sequence)
}
fn play_one(ui: &PlayerWindow, s: &mut State, target: &Target) {
    let Some(sequence) = single_track_sequence(s, target) else {
        return;
    };
    if s.player.is_none() {
        match amp_audio::Player::with_resolver(s.resolver.clone()) {
            Ok(p) => s.player = Some(p),
            Err(e) => {
                ui.set_status(format!("Audio output: {e:#}").into());
                return;
            }
        }
    }
    s.playlist = sequence;
    s.queue_start = 1;
    s.queued.clear();
    s.player.as_ref().unwrap().play(s.playlist.clone(), 0);
    ui.set_status("Playing track".into());
}
pub(super) fn enqueue_track(ui: &PlayerWindow, s: &mut State, track: Track) {
    if let Some(player) = &s.player {
        s.playlist.push(track.clone());
        player.append(vec![track]);
    } else {
        s.queued.push(track);
    }
    ui.set_queued(ModelRc::new(VecModel::from(
        remaining_queue(s)
            .iter()
            .map(|t| SharedString::from(crate::collector::queue_label(t)))
            .collect::<Vec<_>>(),
    )));
    ui.set_status("Added to up next".into());
}
fn action(ui: &PlayerWindow, s: &mut State, detail: bool, i: i32, verb: i32) {
    let action = if detail {
        &s.browser.detail_rows
    } else {
        &s.browser.rows
    }
    .get(i as usize)
    .cloned();
    match action {
        Some(Action::Open(target)) => match verb {
            1 => {
                if s.browser.pending_open.is_some() {
                    open(ui, s, target.clone());
                }
                play_one(ui, s, &target);
            }
            3 if matches!(target, Target::Track { .. }) && (detail || !ui.get_wide()) => {
                // Keep a disappearing row alive during the double-click interval. Sidebar
                // rows on desktop do not disappear and can navigate immediately.
                s.browser.selected_result = Some(target.clone());
                s.browser.pending_open =
                    Some((Instant::now() + Duration::from_millis(500), target));
                if !detail {
                    ui.set_browser_selected(i);
                }
            }
            2 => {
                s.browser.pending_open = None;
                if let Some(track) = resolve_track(s, &target) {
                    enqueue_track(ui, s, track);
                }
            }
            _ => open(ui, s, target),
        },
        Some(Action::More(section)) => {
            push(ui, s);
            s.browser.section = Some(section);
            ui.set_browser_y(0.0);
            render_browser(ui, s);
        }
        _ => {}
    }
}
pub(super) fn wire(ui: &PlayerWindow, state: &Rc<RefCell<State>>) {
    let weak = ui.as_weak();
    let state1 = state.clone();
    ui.on_focus_search_requested(move || {
        if let Some(ui) = weak.upgrade() {
            state1.borrow_mut().browser.pending_open = None;
            ui.set_settings(false);
            ui.set_show_library(true);
            // Conditional phone content must be instantiated before requesting focus.
            let weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(50), move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_focus_search(true);
                }
            });
        }
    });
    let weak = ui.as_weak();
    let state1 = state.clone();
    ui.on_search_changed(move || {
        if let Some(ui) = weak.upgrade() {
            let mut s = state1.borrow_mut();
            s.browser.pending_open = None;
            s.browser.query = ui.get_search_text().to_string();
            s.browser.section = None;
            s.browser.results = Results::default();
            s.browser.selected_result = None;
            // A cleared/edited query must not be resurrected by an older navigation entry.
            let query = s.browser.query.clone();
            for entry in &mut s.browser.history {
                entry.query = query.clone();
                entry.section = None;
                entry.browser_y = 0.0;
                entry.selected_result = None;
            }
            ui.set_browser_y(0.0);
            schedule(&mut s);
            render_browser(&ui, &mut s);
        }
    });
    let weak = ui.as_weak();
    let state1 = state.clone();
    ui.on_browse_action(move |detail, i, verb| {
        if let Some(ui) = weak.upgrade() {
            action(&ui, &mut state1.borrow_mut(), detail, i, verb);
        }
    });
    let weak = ui.as_weak();
    let state1 = state.clone();
    ui.on_open_artist(move |name| {
        if let Some(ui) = weak.upgrade() {
            open(
                &ui,
                &mut state1.borrow_mut(),
                Target::Artist(name.to_string()),
            );
        }
    });
    let weak = ui.as_weak();
    let state1 = state.clone();
    ui.on_back(move || {
        if let Some(ui) = weak.upgrade() {
            let mut s = state1.borrow_mut();
            s.browser.pending_open = None;
            if let Some(loc) = s.browser.history.pop() {
                s.browser.query = loc.query;
                s.browser.section = loc.section;
                s.browser.detail = loc.detail;
                s.browser.selected_result = loc.selected_result;
                ui.set_search_text(s.browser.query.clone().into());
                if let Some(target) = s.browser.detail.clone() {
                    show_target(&ui, &mut s, &target);
                } else {
                    s.selected = None;
                    albums(&ui, &s);
                    ui.set_artist_view(false);
                }
                ui.set_show_library(loc.show_library);
                ui.set_selected_track(loc.selected_track);
                render_browser(&ui, &mut s);
                ui.set_browser_y(loc.browser_y);
                ui.set_detail_y(loc.detail_y);
                ui.set_track_y(loc.track_y);
            } else {
                ui.set_show_library(true);
            }
        }
    });
    let weak = ui.as_weak();
    let state1 = state.clone();
    ui.on_activate_selection(move || {
        if let Some(ui) = weak.upgrade() {
            let mut s = state1.borrow_mut();
            if let Some(target @ Target::Track { .. }) = s.browser.selected_result.clone() {
                if s.browser.pending_open.is_some() {
                    open(&ui, &mut s, target.clone());
                }
                play_one(&ui, &mut s, &target);
            } else if ui.get_selected_track() >= 0 && !ui.get_artist_view() {
                start(&ui, &mut s, ui.get_selected_track() as usize);
            }
        }
    });
    let state1 = state.clone();
    ui.on_album_track_selected(move || clear_track_context(&mut state1.borrow_mut()));
}
pub(super) fn clear_track_context(s: &mut State) {
    s.browser.selected_result = None;
    s.browser.pending_open = None;
}
pub(super) fn mode_changed(ui: &PlayerWindow, s: &mut State) {
    s.browser.pending_open = None;
    ui.set_browser_y(0.0);
    render_browser(ui, s);
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;
    fn setup(name: &str) -> (PlayerWindow, Rc<RefCell<State>>, PathBuf) {
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(980.0, 760.0));
        ui.show().unwrap();
        let path =
            std::env::temp_dir().join(format!("trash-browse-{name}-{}.sqlite", std::process::id()));
        let state = Rc::new(RefCell::new(State {
            resolver: None,
            artwork: Default::default(),
            browser: Browser::default(),
            library: amp_library::demo_library(),
            selected: None,
            playlist: vec![],
            queued: vec![],
            queue_start: 0,
            player: None,
            store: Store::open(&path).unwrap(),
            preferences: Preferences::default(),
        }));
        rebuild(&ui, &mut state.borrow_mut(), true);
        wire(&ui, &state);
        (ui, state, path)
    }
    fn results(ui: &PlayerWindow, s: &mut State, query: &str) {
        s.browser.query = query.into();
        ui.set_search_text(query.into());
        s.browser.results = s.browser.index.search(query);
        render_browser(ui, s);
    }
    #[test]
    fn previews_show_all_and_back_restore_search() {
        let (ui, state, path) = setup("preview");
        results(&ui, &mut state.borrow_mut(), "movement");
        let more = state
            .borrow()
            .browser
            .rows
            .iter()
            .position(|a| matches!(a, Action::More(2)))
            .unwrap();
        assert_eq!(
            state
                .borrow()
                .browser
                .rows
                .iter()
                .filter(|a| matches!(a, Action::Open(Target::Track { .. })))
                .count(),
            5
        );
        ui.set_browser_y(-80.0);
        ui.invoke_browse_action(false, more as i32, 0);
        assert!(ui.get_section_open());
        assert!(state.borrow().browser.rows.len() > 5);
        ui.invoke_back();
        assert!(!ui.get_section_open());
        assert_eq!(ui.get_search_text(), "movement");
        assert_eq!(ui.get_browser_y(), -80.0);
        drop(state);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn track_selection_is_id_based_and_does_not_play_or_enqueue() {
        let (ui, state, path) = setup("select");
        ui.set_wide(false);
        results(&ui, &mut state.borrow_mut(), "movement");
        let (i, target) = state
            .borrow()
            .browser
            .rows
            .iter()
            .enumerate()
            .find_map(|(i, a)| {
                if let Action::Open(t @ Target::Track { .. }) = a {
                    Some((i, t.clone()))
                } else {
                    None
                }
            })
            .unwrap();
        state.borrow_mut().library.albums.reverse();
        ui.invoke_browse_action(false, i as i32, 0);
        assert!(!ui.get_show_library());
        assert_eq!(ui.get_selected_track(), 0);
        assert!(state.borrow().player.is_none());
        assert!(state.borrow().queued.is_empty());
        assert_eq!(state.borrow().browser.selected_result, Some(target.clone()));
        ui.invoke_back();
        assert!(ui.get_show_library());
        assert_eq!(ui.get_search_text(), "movement");
        ui.invoke_browse_action(false, i as i32, 2);
        assert!(ui.get_show_library());
        assert_eq!(
            state.borrow().queued[0].id,
            resolve_track(&state.borrow(), &target).unwrap().id
        );
        let last = {
            let s = state.borrow();
            let album = &s.library.albums[0];
            Target::Track {
                album: album.id.clone(),
                track: album.tracks[9].id.clone(),
            }
        };
        open(&ui, &mut state.borrow_mut(), last);
        assert_eq!(ui.get_selected_track(), 9);
        slint::platform::update_timers_and_animations();
        use i_slint_backend_testing::ElementHandle;
        let list = ElementHandle::find_by_accessible_label(&ui, "Album tracks")
            .next()
            .unwrap();
        let track = ElementHandle::find_by_accessible_label(&ui, "Movement 10")
            .next()
            .unwrap();
        assert!(track.absolute_position().y >= list.absolute_position().y);
        assert!(
            track.absolute_position().y + track.size().height
                <= list.absolute_position().y + list.size().height + 1.0
        );
        drop(state);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn artist_navigation_clear_and_metadata_rebuild_keep_details() {
        let (ui, state, path) = setup("artist");
        results(&ui, &mut state.borrow_mut(), "fixture");
        ui.invoke_open_artist("Layout fixture".into());
        assert!(ui.get_artist_view());
        use i_slint_backend_testing::ElementHandle;
        let releases = ElementHandle::find_by_accessible_label(&ui, "Artist releases")
            .next()
            .unwrap();
        assert!(
            releases.size().width > 500.0,
            "Back must not constrain detail width"
        );
        ui.set_wide(false);
        ui.window().set_size(slint::LogicalSize::new(390.0, 740.0));
        slint::platform::update_timers_and_animations();
        let releases = ElementHandle::find_by_accessible_label(&ui, "Artist releases")
            .next()
            .unwrap();
        assert!(releases.size().width > 340.0);
        let title = ui.get_album_title();
        rebuild(&ui, &mut state.borrow_mut(), false);
        assert!(ui.get_artist_view());
        assert_eq!(ui.get_album_title(), title);
        ui.set_search_text("".into());
        ui.invoke_search_changed();
        assert!(ui.get_artist_view());
        ui.invoke_back();
        assert_eq!(ui.get_search_text(), "");
        assert!(ui.get_show_library());
        drop(state);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn stale_query_and_catalog_replies_are_discarded() {
        let (ui, state, path) = setup("stale");
        let mut s = state.borrow_mut();
        results(&ui, &mut s, "missing");
        let stale = s.browser.index.search("movement");
        s.browser.query_version = 2;
        s.browser
            .tx
            .send(SearchReply {
                query_version: 1,
                catalog_version: s.browser.catalog_version,
                results: stale.clone(),
            })
            .unwrap();
        s.browser
            .tx
            .send(SearchReply {
                query_version: 2,
                catalog_version: s.browser.catalog_version - 1,
                results: stale,
            })
            .unwrap();
        poll(&ui, &mut s);
        assert!(s.browser.results.tracks.is_empty());
        rebuild(&ui, &mut s, true);
        assert_eq!(ui.get_search_text(), "");
        assert!(!ui.get_can_back());
        drop(s);
        drop(state);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn search_input_keeps_transport_keys_and_focus_shortcut_works_on_phone() {
        use slint::platform::{Key, WindowEvent};
        use std::cell::Cell;
        let (ui, state, path) = setup("focus");
        let toggles = Rc::new(Cell::new(0));
        let t = toggles.clone();
        ui.on_toggle_play(move || t.set(t.get() + 1));
        let keys = |text: SharedString| {
            ui.window()
                .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            ui.window()
                .dispatch_event(WindowEvent::KeyReleased { text });
        };
        ui.set_focus_search(true);
        slint::platform::update_timers_and_animations();
        assert!(ui.get_search_editing());
        keys("a".into());
        keys(" ".into());
        keys("b".into());
        keys(Key::LeftArrow.into());
        keys(Key::Return.into());
        assert_eq!(ui.get_search_text(), "a b");
        assert_eq!(toggles.get(), 0);
        keys(Key::Escape.into());
        assert!(!ui.get_search_editing());
        keys(" ".into());
        assert_eq!(toggles.get(), 1);
        ui.set_wide(false);
        ui.set_show_library(false);
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Control.into(),
        });
        keys("f".into());
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Control.into(),
        });
        slint::platform::update_timers_and_animations();
        assert!(ui.get_show_library());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(60));
        slint::platform::update_timers_and_animations();
        assert!(ui.get_search_editing());
        drop(state);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn pending_pointer_navigation_preserves_source_and_can_be_cancelled() {
        let (ui, state, path) = setup("pointer");
        ui.set_wide(false);
        results(&ui, &mut state.borrow_mut(), "movement");
        let i = state
            .borrow()
            .browser
            .rows
            .iter()
            .position(|a| matches!(a, Action::Open(Target::Track { .. })))
            .unwrap();
        ui.invoke_browse_action(false, i as i32, 3);
        assert!(ui.get_show_library());
        assert!(state.borrow().browser.pending_open.is_some());
        let target = state
            .borrow()
            .browser
            .pending_open
            .as_ref()
            .unwrap()
            .1
            .clone();
        state.borrow_mut().browser.pending_open = Some((Instant::now(), target.clone()));
        poll(&ui, &mut state.borrow_mut());
        assert!(!ui.get_show_library());
        assert_eq!(state.borrow().browser.selected_result, Some(target));
        assert!(state.borrow().player.is_none());
        ui.invoke_back();
        ui.invoke_browse_action(false, i as i32, 3);
        ui.set_search_text("changed".into());
        ui.invoke_search_changed();
        assert!(state.borrow().browser.pending_open.is_none());
        drop(state);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn hundred_thousand_results_only_instantiate_visible_rows() {
        use i_slint_backend_testing::ElementHandle;
        let (ui, state, path) = setup("virtualized");
        {
            let mut s = state.borrow_mut();
            let original = s.library.albums.clone();
            for batch in 1..5 {
                for mut album in original.clone() {
                    album.id.item = format!("{batch}/{}", album.id.item);
                    for track in &mut album.tracks {
                        track.id.item = format!("{batch}/{}", track.id.item);
                    }
                    s.library.albums.push(album);
                }
            }
            rebuild(&ui, &mut s, true);
            results(&ui, &mut s, "movement");
            s.browser.section = Some(2);
            render_browser(&ui, &mut s);
            assert_eq!(s.browser.rows.len(), 100_001);
        }
        slint::platform::update_timers_and_animations();
        let count = ElementHandle::find_by_element_type_name(&ui, "BrowserItem").count();
        assert!(count > 0 && count < 100, "instantiated {count} rows");
        ui.set_browser_y(-50_000.0);
        slint::platform::update_timers_and_animations();
        let count = ElementHandle::find_by_element_type_name(&ui, "BrowserItem").count();
        assert!(
            count > 0 && count < 100,
            "instantiated {count} scrolled rows"
        );
        drop(state);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn explicit_queue_excludes_automatic_album_tail() {
        let (ui, state, path) = setup("queue");
        let mut s = state.borrow_mut();
        let album = s.library.albums[0].tracks.clone();
        s.playlist = album.clone();
        s.queue_start = album.len();
        let explicit = s.library.albums[1].tracks[0].clone();
        s.playlist.push(explicit.clone());
        s.queued.push(explicit.clone());
        let remaining = remaining_queue(&s);
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().all(|t| t.id == explicit.id));
        let target = Target::Track {
            album: s.library.albums[2].id.clone(),
            track: s.library.albums[2].tracks[3].id.clone(),
        };
        let sequence = single_track_sequence(&s, &target).unwrap();
        assert_eq!(sequence.len(), 3);
        assert_eq!(sequence[0].id, resolve_track(&s, &target).unwrap().id);
        assert_eq!(sequence[1].id, explicit.id);
        assert_eq!(remaining_queue_after(&s, s.playlist.len()).len(), 1);
        assert!(ui.get_queued().row_count() == 0);
        drop(s);
        drop(state);
        let _ = std::fs::remove_file(path);
    }
}
