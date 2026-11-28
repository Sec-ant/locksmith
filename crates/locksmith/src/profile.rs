//! Phase timings for benchmark builds.
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

static SAMPLES: Mutex<Vec<(&'static str, Duration)>> = Mutex::new(Vec::new());

/// A timer that records a sample when it goes out of scope.
#[derive(Debug)]
pub struct Span {
    component: &'static str,
    start: Instant,
}

/// Start timing a benchmark component.
#[must_use]
pub fn span(component: &'static str) -> Span {
    Span {
        component,
        start: Instant::now(),
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        SAMPLES.lock().unwrap().push((self.component, elapsed));
    }
}

/// Drain timings from completed spans.
pub fn take_samples() -> Vec<(&'static str, Duration)> {
    std::mem::take(&mut *SAMPLES.lock().unwrap())
}
