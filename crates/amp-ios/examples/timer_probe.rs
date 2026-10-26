//! Native integration probe for the vendored winit timer. See scripts/test-ios-timer.py.
#[cfg(target_os = "ios")]
mod probe {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use i_slint_backend_winit::{CustomApplicationHandler, EventResult};
    use slint::ComponentHandle;
    use winit::event::StartCause;
    use winit::event_loop::{ActiveEventLoop, ControlFlow};

    slint::slint! {
        export component ProbeWindow inherits Window {
            callback external-wake();
            title: "iOS timer regression probe";
            width: 300px;
            height: 200px;
            Text { text: "Testing native timer scheduling"; }
        }
    }

    struct Handler {
        scenario: String,
        started: Instant,
        previous: Instant,
        fixed_deadline: Instant,
        next_deadline: Instant,
        timer_wakes: usize,
        iterations: usize,
        ready: bool,
        external_wakes: Rc<Cell<usize>>,
    }

    fn passed(scenario: &str) -> ! {
        println!("PROBE_PASS {scenario}");
        std::process::exit(0);
    }

    impl CustomApplicationHandler for Handler {
        fn new_events(&mut self, _: &ActiveEventLoop, cause: StartCause) -> EventResult {
            if let StartCause::ResumeTimeReached {
                requested_resume, ..
            } = cause
            {
                match self.scenario.as_str() {
                    "fixed-deadline" if requested_resume == self.fixed_deadline => {
                        self.timer_wakes += 1;
                    }
                    "wait-until" | "wait-until-freeze"
                        if requested_resume == self.next_deadline =>
                    {
                        self.timer_wakes += 1;
                        self.next_deadline = Instant::now() + Duration::from_millis(50);
                    }
                    _ => {}
                }
            }
            EventResult::Propagate
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) -> EventResult {
            let now = Instant::now();
            let gap = now.duration_since(self.previous);
            self.previous = now;
            self.iterations += 1;
            if self.ready && gap > Duration::from_millis(200) {
                println!("PROBE_GAP_MS {:.3}", gap.as_secs_f64() * 1000.0);
            }
            if !self.ready && self.iterations >= 5 {
                self.ready = true;
                println!("PROBE_READY {}", self.scenario);
            }
            match self.scenario.as_str() {
                "poll" => {
                    event_loop.set_control_flow(ControlFlow::Poll);
                    if now.duration_since(self.started) > Duration::from_secs(3) {
                        passed(&self.scenario);
                    }
                }
                "fixed-deadline" => {
                    // Reuse the exact deadline even after it has fired. A patch
                    // which only increases the repeat interval hangs here if it
                    // retains winit's unchanged-deadline optimization.
                    event_loop.set_control_flow(ControlFlow::WaitUntil(self.fixed_deadline));
                    if self.timer_wakes >= 3 {
                        passed(&self.scenario);
                    }
                }
                "wait-until" | "wait-until-freeze" => {
                    event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_deadline));
                    if (self.scenario == "wait-until" && self.timer_wakes >= 8)
                        || (self.scenario == "wait-until-freeze"
                            && now.duration_since(self.started) > Duration::from_secs(3))
                    {
                        passed(&self.scenario);
                    }
                }
                "wait" => {
                    event_loop.set_control_flow(ControlFlow::Wait);
                    if self.external_wakes.get() >= 3 {
                        // Wait must park the timer, while external events still
                        // wake it. An accidentally polling timer spins here.
                        assert!(self.iterations < 200, "Wait unexpectedly polled");
                        passed(&self.scenario);
                    }
                    return EventResult::PreventDefault;
                }
                _ => panic!("unknown scenario: {}", self.scenario),
            }
            EventResult::Propagate
        }
    }

    pub fn run() {
        let scenario = std::env::args().nth(1).unwrap_or_else(|| "poll".into());
        let external_wakes = Rc::new(Cell::new(0));
        let started = Instant::now();
        let handler = Handler {
            scenario: scenario.clone(),
            started,
            previous: started,
            fixed_deadline: started + Duration::from_millis(500),
            next_deadline: started + Duration::from_millis(50),
            timer_wakes: 0,
            iterations: 0,
            ready: false,
            external_wakes: external_wakes.clone(),
        };
        let backend = i_slint_backend_winit::Backend::builder()
            .with_custom_application_handler(Box::new(handler))
            .build()
            .unwrap();
        slint::platform::set_platform(Box::new(backend)).unwrap();
        let window = ProbeWindow::new().unwrap();
        // A native user event wakes ControlFlow::Wait without a Slint timer.
        if scenario == "wait" {
            let weak = window.as_weak();
            std::thread::spawn(move || {
                for _ in 0..3 {
                    std::thread::sleep(Duration::from_millis(400));
                    let weak = weak.clone();
                    weak.upgrade_in_event_loop(|window| window.invoke_external_wake())
                        .unwrap();
                }
            });
        }
        window.on_external_wake(move || external_wakes.set(external_wakes.get() + 1));
        window.run().unwrap();
    }
}

#[cfg(target_os = "ios")]
fn main() {
    probe::run();
}

#[cfg(not(target_os = "ios"))]
fn main() {
    eprintln!("Build this probe for aarch64-apple-ios-sim or aarch64-apple-ios.");
}
