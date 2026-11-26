//! Phase timings for benchmark builds.
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

static SAMPLES: Mutex<Vec<(&'static str, Duration)>> = Mutex::new(Vec::new());

pub(crate) struct Span {
    component: &'static str,
    start: Instant,
}

pub(crate) fn span(component: &'static str) -> Span {
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
