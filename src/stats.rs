use std::fmt::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ROLLING_WINDOW: Duration = Duration::from_secs(3600);

/// Fixed capacity for the recent-generations ring. Sized for roughly one
/// generation every 3.5 seconds sustained for an hour — far above any
/// realistic rate. If exceeded, oldest entries are silently overwritten,
/// which only affects the last-hour count (totals remain accurate).
const RING_CAP: usize = 1024;

#[derive(Copy, Clone, Debug)]
pub enum Kind {
    Pane,
    Window,
}

struct Ring {
    buf: [Option<Instant>; RING_CAP],
    next: usize,
}

impl Ring {
    fn new() -> Self {
        Self {
            buf: [None; RING_CAP],
            next: 0,
        }
    }

    fn push(&mut self, t: Instant) {
        self.buf[self.next] = Some(t);
        self.next = (self.next + 1) % RING_CAP;
    }

    fn count_since(&self, cutoff: Instant) -> usize {
        self.buf
            .iter()
            .filter_map(|x| x.as_ref())
            .filter(|t| **t >= cutoff)
            .count()
    }
}

pub struct Stats {
    start: Instant,
    pane_generations: u64,
    window_generations: u64,
    in_flight: usize,
    recent: Ring,
    total_generation_time: Duration,
}

impl Stats {
    pub fn new(now: Instant) -> Self {
        Self {
            start: now,
            pane_generations: 0,
            window_generations: 0,
            in_flight: 0,
            recent: Ring::new(),
            total_generation_time: Duration::ZERO,
        }
    }

    fn in_flight_inc(&mut self) {
        self.in_flight += 1;
    }

    fn in_flight_dec(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    /// Record a successful generation. Timestamps are recorded at completion
    /// time, so "last hour" counts when a title landed, not when it started.
    fn record_success(&mut self, kind: Kind, now: Instant, duration: Duration) {
        match kind {
            Kind::Pane => self.pane_generations += 1,
            Kind::Window => self.window_generations += 1,
        }
        self.recent.push(now);
        self.total_generation_time += duration;
    }

    pub fn render(&self, now: Instant) -> String {
        // Fall back to `self.start` (not `now`) so early-lifetime renders count
        // entries since process start rather than filtering everything out.
        let cutoff = now.checked_sub(ROLLING_WINDOW).unwrap_or(self.start);
        let last_hour = self.recent.count_since(cutoff);
        let total = self.pane_generations + self.window_generations;
        let uptime = now.duration_since(self.start);
        // `total` saturates to u32::MAX for the division — unreachable in
        // practice (u32::MAX is ~136 years at 1 gen/sec) but cheap insurance.
        let divisor = u32::try_from(total).unwrap_or(u32::MAX);
        let avg_interval = uptime.checked_div(divisor);
        let avg_gen_time = self.total_generation_time.checked_div(divisor);

        let mut out = String::new();
        let _ = writeln!(out, "uptime: {}", format_duration(uptime));
        let _ = writeln!(
            out,
            "total generations: {total} (panes: {}, windows: {})",
            self.pane_generations, self.window_generations
        );
        let _ = writeln!(out, "last hour: {last_hour}");
        let _ = writeln!(
            out,
            "avg interval: {}",
            avg_interval
                .map(format_duration)
                .as_deref()
                .unwrap_or("n/a")
        );
        let _ = writeln!(
            out,
            "avg generation time: {}",
            avg_gen_time
                .map(format_duration_ms)
                .as_deref()
                .unwrap_or("n/a")
        );
        let _ = write!(out, "in flight: {}", self.in_flight);
        out
    }
}

/// RAII guard for a single in-flight generation. Construction increments the
/// in-flight counter synchronously on the caller's thread. On drop, the
/// counter is decremented — so a panicking worker thread won't leak the
/// increment. Call `success` (consumes the guard) on the happy path to
/// record a completed generation; failure paths (claude returned nothing,
/// panic) just let the guard drop.
pub struct InFlight {
    // Option so `success` can `take()` the state, leaving Drop a no-op.
    // Consuming `self` in `success` makes double-recording a compile error.
    state: Option<InFlightState>,
}

struct InFlightState {
    stats: Arc<Mutex<Stats>>,
    started_at: Instant,
}

impl InFlight {
    pub fn new(stats: Arc<Mutex<Stats>>) -> Self {
        stats.lock().unwrap().in_flight_inc();
        Self {
            state: Some(InFlightState {
                stats,
                started_at: Instant::now(),
            }),
        }
    }

    pub fn success(mut self, kind: Kind) {
        let state = self
            .state
            .take()
            .expect("InFlight state already taken; this is a bug");
        let now = Instant::now();
        let duration = now.duration_since(state.started_at);
        let mut stats = state.stats.lock().unwrap();
        stats.record_success(kind, now, duration);
        stats.in_flight_dec();
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            state.stats.lock().unwrap().in_flight_dec();
        }
    }
}

fn format_duration(d: Duration) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// Like format_duration but with sub-second precision, for short durations
/// like per-generation wall time.
fn format_duration_ms(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.2}s", d.as_secs_f64())
    }
}
