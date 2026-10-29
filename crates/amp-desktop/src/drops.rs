//! Native events are one-per-file; debounce them into one playlist replacement.
use amp_ui::{FileDropCallback, FileDropEvent};
use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
use std::{cell::RefCell, path::PathBuf, rc::Rc, time::Duration};

struct DropHandler {
    paths: Rc<RefCell<Vec<PathBuf>>>,
    timer: slint::Timer,
    callback: FileDropCallback,
}
impl DropHandler {
    fn new(callback: FileDropCallback) -> Self {
        Self {
            paths: Default::default(),
            timer: Default::default(),
            callback,
        }
    }
    fn event(&self, event: &WindowEvent) -> EventResult {
        match event {
            WindowEvent::HoveredFile(_) => {
                (self.callback)(FileDropEvent::Hover(true));
            }
            WindowEvent::HoveredFileCancelled => {
                (self.callback)(FileDropEvent::Hover(false));
            }
            WindowEvent::DroppedFile(path) => {
                if !(self.callback)(FileDropEvent::Hover(false)) {
                    return EventResult::PreventDefault;
                }
                self.paths.borrow_mut().push(path.clone());
                let paths = self.paths.clone();
                let callback = self.callback.clone();
                self.timer.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(100),
                    move || {
                        callback(FileDropEvent::Files(std::mem::take(
                            &mut *paths.borrow_mut(),
                        )));
                    },
                );
            }
            _ => return EventResult::Propagate,
        }
        EventResult::PreventDefault
    }
}
pub fn install(window: &slint::Window, callback: FileDropCallback) {
    let handler = DropHandler::new(callback);
    window.on_winit_window_event(move |_, event| handler.event(event));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[test]
    fn native_events_batch_once_restart_timer_and_ignore_busy_drops() {
        i_slint_backend_testing::init_no_event_loop();
        let batches = Rc::new(RefCell::new(Vec::new()));
        let hover = Rc::new(Cell::new(false));
        let ready = Rc::new(Cell::new(true));
        let handler = DropHandler::new({
            let (batches, hover, ready) = (batches.clone(), hover.clone(), ready.clone());
            Rc::new(move |event| {
                match event {
                    FileDropEvent::Hover(value) => hover.set(value && ready.get()),
                    FileDropEvent::Files(paths) => batches.borrow_mut().push(paths),
                }
                ready.get()
            })
        });
        handler.event(&WindowEvent::HoveredFile("2.mp3".into()));
        assert!(hover.get());
        handler.event(&WindowEvent::HoveredFileCancelled);
        assert!(!hover.get());
        handler.event(&WindowEvent::DroppedFile("2.mp3".into()));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(75));
        handler.event(&WindowEvent::DroppedFile("1.mp3".into()));
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(75));
        assert!(batches.borrow().is_empty());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(30));
        assert_eq!(
            *batches.borrow(),
            vec![vec![PathBuf::from("2.mp3"), PathBuf::from("1.mp3")]]
        );
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
        assert_eq!(batches.borrow().len(), 1);
        ready.set(false);
        handler.event(&WindowEvent::DroppedFile("ignored.mp3".into()));
        ready.set(true);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
        assert_eq!(batches.borrow().len(), 1);
        assert!(matches!(
            handler.event(&WindowEvent::Focused(true)),
            EventResult::Propagate
        ));
    }
}
