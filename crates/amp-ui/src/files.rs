//! Progressive discovery stays independent of browsing and never blocks transport.
use super::*;
use amp_app::files::{FileLoad, LoadEvent};

struct ActiveLoad {
    load: FileLoad,
    session: String,
    queued: usize,
}
type Job = Rc<RefCell<Option<ActiveLoad>>>;
pub(super) struct Controller {
    job: Job,
    weak: slint::Weak<PlayerWindow>,
    state: Rc<RefCell<State>>,
}
impl Controller {
    pub fn new(
        ui: &PlayerWindow,
        state: Rc<RefCell<State>>,
        install: Option<FileDropInstaller>,
    ) -> Self {
        let job: Job = Default::default();
        let weak = ui.as_weak();
        if let Some(install) = install {
            let weak = weak.clone();
            let job = job.clone();
            let playback = state.borrow().playback.clone();
            install(
                ui.window(),
                Rc::new(move |event| {
                    let Some(ui) = weak.upgrade() else {
                        return false;
                    };
                    let ready = job.borrow().is_none() && ui.get_error_message().is_empty();
                    match event {
                        FileDropEvent::Hover(hovered) => {
                            ui.set_drop_hovered(hovered && ready);
                            ready
                        }
                        FileDropEvent::Files(paths) => {
                            if !ready || paths.is_empty() {
                                return false;
                            }
                            let session = format!("drop:{}", uuid::Uuid::new_v4());
                            match FileLoad::start(paths, session.clone()) {
                                Ok(load) => {
                                    playback
                                        .begin_file_discovery(session.clone(), load.cancel_token());
                                    *job.borrow_mut() = Some(ActiveLoad {
                                        load,
                                        session,
                                        queued: 0,
                                    });
                                    ui.set_loading_message("Finding music…".into());
                                    ui.set_loading_path("".into());
                                    ui.set_drop_hovered(false);
                                    ui.set_loading_files(true);
                                    true
                                }
                                Err(e) => {
                                    ui.set_status(format!("Cannot load dropped music: {e}").into());
                                    false
                                }
                            }
                        }
                    }
                }),
            );
        }
        {
            let weak = weak.clone();
            let job = job.clone();
            ui.on_cancel_file_load(move || {
                if let Some(active) = job.borrow_mut().take() {
                    // Drop cancels producer backpressure too; no filesystem join on UI.
                    if let Some(ui) = weak.upgrade() {
                        ui.set_loading_files(false);
                        ui.set_status(
                            format!(
                                "Stopped finding music. Kept {} queued tracks.",
                                active.queued
                            )
                            .into(),
                        );
                    }
                }
            });
        }
        Self { job, weak, state }
    }
    pub fn poll(&self) {
        let Some(ui) = self.weak.upgrade() else {
            return;
        };
        let mut job = self.job.borrow_mut();
        let Some(active) = job.as_mut() else {
            return;
        };
        if active.load.is_cancelled() {
            job.take();
            ui.set_loading_files(false);
            return;
        }
        // Limit work per UI tick, then append as one batch without disturbing pause/seek.
        let mut append = Vec::new();
        let mut finished = None;
        for _ in 0..32 {
            match active.load.next_event() {
                Ok(LoadEvent::Track(track)) => {
                    if active.queued == 0 {
                        let mut state = self.state.borrow_mut();
                        state
                            .playback
                            .start_file_queue(track, active.load.cancel_token());
                        browse::show_playback_queue(&ui, &mut state);
                    } else {
                        append.push(track);
                    }
                    active.queued += 1;
                }
                Ok(LoadEvent::Finished(summary)) => {
                    finished = Some(summary);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    ui.set_status(
                        "Music discovery stopped unexpectedly. Queued tracks remain available."
                            .into(),
                    );
                    // Also handles a worker panic without leaving a permanent progress banner.
                    job.take();
                    ui.set_loading_files(false);
                    return;
                }
            }
        }
        if !append.is_empty() {
            self.state
                .borrow()
                .playback
                .append_file_queue(active.session.clone(), append);
        }
        if let Some(summary) = finished {
            // Completion isn't cancellation: pending playback commands must remain valid.
            // Ordered commands retain the token after a normally drained loader is dropped.
            self.state
                .borrow()
                .playback
                .finish_file_queue(active.session.clone());
            ui.set_status(
                if active.queued == 0 {
                    format!(
                        "No supported music found. Skipped {} items.",
                        summary.skipped
                    )
                } else {
                    format!(
                        "Added {} tracks. Skipped {} items.",
                        active.queued, summary.skipped
                    )
                }
                .into(),
            );
            active.load.finish();
            job.take();
            ui.set_loading_files(false);
        } else {
            let p = active.load.progress();
            ui.set_loading_message(
                format!(
                    "Finding music · {} queued · {} checked · {} skipped",
                    active.queued, p.visited, p.skipped
                )
                .into(),
            );
            ui.set_loading_path(p.path.to_string_lossy().into_owned().into());
        }
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.job.borrow_mut().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use i_slint_backend_testing::ElementHandle;
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    use std::cell::Cell;
    #[test]
    fn discovery_status_keeps_keyboard_pointer_and_accessibility_transport_available() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = PlayerWindow::new().unwrap();
        ui.set_home(true);
        ui.window().set_size(slint::LogicalSize::new(980., 740.));
        ui.show().unwrap();
        let toggles = Rc::new(Cell::new(0));
        let count = toggles.clone();
        ui.on_toggle_play(move || count.set(count.get() + 1));
        let next = Rc::new(Cell::new(0));
        let count = next.clone();
        ui.on_next(move || count.set(count.get() + 1));
        let cancels = Rc::new(Cell::new(0));
        let count = cancels.clone();
        ui.on_cancel_file_load(move || count.set(count.get() + 1));
        ui.set_loading_files(true);
        ui.set_loading_message("Finding music · 2 queued".into());
        let play = ElementHandle::find_by_accessible_label(&ui, "Play")
            .next()
            .unwrap();
        let position = slint::LogicalPosition::new(
            play.absolute_position().x + 12.,
            play.absolute_position().y + 12.,
        );
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
        assert_eq!(toggles.get(), 1);
        play.invoke_accessible_default_action();
        assert_eq!(toggles.get(), 2);
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::RightArrow.into(),
        });
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::RightArrow.into(),
        });
        assert_eq!(next.get(), 1);
        assert!(ui.get_loading_files());
        ElementHandle::find_by_accessible_label(&ui, "Stop finding music")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(cancels.get(), 1);
    }
}
