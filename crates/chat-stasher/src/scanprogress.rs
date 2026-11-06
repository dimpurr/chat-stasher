//! scanprogress — the ISSUE-007 §4 S3 heartbeat for one archive scan.
//!
//! A cold scan of a real archive is the command shape that measured ten
//! minutes with one stderr line and zero stdout (ISSUE-007 §2 F3), because a
//! scan's work sits between two prints: the trace names it before it runs,
//! and nothing speaks again until the whole cumulative walk has finished.
//! This module is the "every 5 seconds, at least one line" half of the fix:
//! fed once per finished snapshot by
//! [`crate::search::search_sessions_with_progress`], it lines out how far the
//! walk has reached — counts, hosts and cache hits only, in the exact
//! vocabulary the search tier already reports, never a session id beyond the
//! host and never any conversation content (see [`crate::push_progress`] for
//! the same discipline on the push path).
//!
//! The cadence is isolated so it is assertable without sleeping: the clock
//! and the sink are injectable, and one environment variable
//! ([`SCAN_HEARTBEAT_INTERVAL_ENV`]) turns every event into a line for the
//! fixtures — the same override pattern
//! [`crate::readstep::READ_STEP_BUDGET_ENV`] established for the step
//! budgets.

use std::time::{Duration, Instant};

/// The default gap between two heartbeat lines: ISSUE-007 S3's own number.
pub const DEFAULT_HEARTBEAT_SECS: u64 = 5;

/// The test/ops override for the heartbeat interval (seconds). A value that
/// does not parse keeps the default; `0` means a line per finished snapshot —
/// the debug and fixture spelling, so a fixture can pin the single-sweep
/// shape without sleeping to make a scan look slow.
pub const SCAN_HEARTBEAT_INTERVAL_ENV: &str = "CHAT_STASHER_SCAN_PROGRESS_SECS";

/// The heartbeat for one scan. Owns everything a cadence needs: the interval,
/// when the last line went out, where a line goes, and what line to write.
///
/// Not `Sync` by design: one heartbeat serves one scan in one thread (the
/// deadline worker [`crate::readstep::run_deadline_body`] moved it into).
pub struct ScanHeartbeat {
    /// Minimum gap between two printed lines.
    interval: Duration,
    /// When the scan started (for the elapsed figure).
    start: Instant,
    /// When the last line was printed, or the scan's start — so a scan that
    /// finishes inside one interval prints **nothing**: ISSUE-007 S3 asks
    /// for a line only from a run that outlives the interval, and a warm
    /// cache answering in milliseconds must not grow extra output.
    last_emitted: Instant,
    /// The `[export]` / `[search]` prefix family the lines join.
    prefix: String,
    now: Box<dyn Fn() -> Instant + Send + Sync>,
    sink: Box<dyn Fn(&str) + Send + Sync>,
}

impl ScanHeartbeat {
    /// The production heartbeat for one command prefix, printing to stderr.
    pub fn for_command(prefix: &str) -> Self {
        Self::for_command_with_sink(prefix, |line| eprintln!("{line}"))
    }

    /// The production interval and prefix with a caller-supplied sink —
    /// `cmd_export`'s scan heartbeat reports through the same lines it wants
    /// to re-read if the scan's deadline expires ("last progress: …"), so
    /// one closure feeds both stderr and the timeout sentence.
    pub fn for_command_with_sink(
        prefix: &str,
        sink: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        let interval = std::env::var(SCAN_HEARTBEAT_INTERVAL_ENV)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs)
            // reason: no override at all, or one that does not parse as an
            // unsigned second count — the documented default is the honest
            // cadence, not a guess a user did not write. Zero is accepted,
            // which the directive above says in so many words: it is the
            // debug spelling for a line per snapshot.
            .unwrap_or(Duration::from_secs(DEFAULT_HEARTBEAT_SECS));
        Self::with_parts(prefix, interval, Instant::now, sink)
    }

    /// The fully-injectable spelling: the fixture clock, the fixture sink.
    pub fn with_parts(
        prefix: &str,
        interval: Duration,
        now: impl Fn() -> Instant + Send + Sync + 'static,
        sink: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        let start = now();
        ScanHeartbeat {
            interval,
            start,
            last_emitted: start,
            prefix: format!("[{prefix}]"),
            now: Box::new(now),
            sink: Box::new(sink),
        }
    }

    /// One snapshot finished. Prints when the interval since the last line
    /// has elapsed — never for the first event at the scan's own start, so
    /// a warm run that answers inside one interval prints no heartbeat at
    /// all (and a `Duration::ZERO` interval, the fixture shape, prints
    /// every event — zero never gates a zero).
    ///
    /// The counters are the walk's own, passed by the caller rather than
    /// accumulated here, so the line can never disagree with the report the
    /// same walk is building. The host is deliberately absent from the line:
    /// the walk is cumulative over every host's snapshots, and the line's
    /// `scanned N of M` is the whole destination's progress, not one
    /// machine's.
    pub fn beat(&mut self, scanned: usize, from_cache: usize, in_repo: usize) {
        let now = (self.now)();
        if now.duration_since(self.last_emitted) < self.interval {
            return;
        }
        self.last_emitted = now;
        let elapsed = now.duration_since(self.start).as_secs();
        (self.sink)(&format!(
            "{} scan         : scanned {} of {} snapshots ({} from cache), {elapsed}s",
            self.prefix, scanned, in_repo, from_cache,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A clock a test advances by hand, plus a sink that collects lines —
    /// both behind `Arc<Mutex>` because the heartbeat's clock and sink are
    /// `Send + Sync` by design (the production heartbeat lives inside the
    /// deadline worker thread), so the fixtures need thread-safe handles too.
    struct Harness {
        now: Arc<Mutex<Instant>>,
        lines: Arc<Mutex<Vec<String>>>,
        advanced: Arc<Mutex<u64>>,
    }

    impl Harness {
        fn new() -> Self {
            Harness {
                now: Arc::new(Mutex::new(Instant::now())),
                lines: Arc::new(Mutex::new(Vec::new())),
                advanced: Arc::new(Mutex::new(0)),
            }
        }

        /// Queue `secs` of fake time; the injectable clock consumes the queue
        /// on its next read and moves its base forward, so every read of the
        /// clock sees one monotonic hand.
        fn advance(&self, secs: u64) {
            let mut advanced = self.advanced.lock().unwrap_or_else(|p| p.into_inner());
            *advanced += secs;
        }

        fn lines(&self) -> Vec<String> {
            self.lines.lock().unwrap_or_else(|p| p.into_inner()).clone()
        }

        fn heartbeat(&self, interval_secs: u64) -> ScanHeartbeat {
            let now = Arc::clone(&self.now);
            let lines = Arc::clone(&self.lines);
            let advanced = Arc::clone(&self.advanced);
            ScanHeartbeat::with_parts(
                "export",
                Duration::from_secs(interval_secs),
                move || {
                    let mut moved = advanced.lock().unwrap_or_else(|p| p.into_inner());
                    if *moved > 0 {
                        let step = Duration::from_secs(*moved);
                        *moved = 0;
                        let mut slot = now.lock().unwrap_or_else(|p| p.into_inner());
                        *slot += step;
                    }
                    *now.lock().unwrap_or_else(|p| p.into_inner())
                },
                move |line| {
                    lines
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(line.to_string())
                },
            )
        }
    }

    /// S3's own sentence: a long scan prints at least one line per interval,
    /// and not one per event. With a 5 s interval, events at 1 s spacing first
    /// print when the interval has elapsed (no line at the scan's own
    /// start), then every 5 s after — and a scan that ends inside the first
    /// interval prints nothing at all.
    #[test]
    fn a_long_scan_lines_out_once_per_interval_not_per_event() {
        let harness = Harness::new();
        let mut beat = harness.heartbeat(5);
        for _ in 0..12 {
            beat.beat(1, 0, 12);
            harness.advance(1);
        }
        let lines = harness.lines();
        assert_eq!(
            lines.len(),
            2,
            "12 one-second events under a five-second interval print at ~5s and ~10s: {lines:?}"
        );
        assert!(
            lines[0].starts_with("[export] scan         : scanned "),
            "{}",
            lines[0]
        );
        assert!(
            lines[0].contains("scanned 1 of 12"),
            "counts include the repo listing: {}",
            lines[0]
        );
        assert!(lines[1].contains("(0 from cache)"), "{}", lines[1]);
        // And nothing in the first five seconds: the pre-interval events
        // printed no line at all, which is what the next test pins on its own.
        let harness = Harness::new();
        let mut beat = harness.heartbeat(5);
        for _ in 0..4 {
            beat.beat(0, 0, 3);
            harness.advance(1);
        }
        assert!(
            harness.lines().is_empty(),
            "four seconds of walking under a five-second interval stays quiet: {:?}",
            harness.lines()
        );
    }

    /// The zero interval is the fixture shape (a line per event), and the
    /// counts on each line are the caller's own at that moment — a walk that
    /// has finished 3 of 4 snapshots says 3, cache hits included.
    #[test]
    fn a_zero_interval_lines_every_event_with_the_caller_counts() {
        let harness = Harness::new();
        let mut beat = harness.heartbeat(0);
        beat.beat(1, 0, 4);
        beat.beat(2, 1, 4);
        beat.beat(3, 1, 4);
        beat.beat(4, 2, 4);
        let lines = harness.lines();
        assert_eq!(lines.len(), 4, "one line per finished snapshot: {lines:?}");
        assert!(
            lines[3].contains("scanned 4 of 4 snapshots (2 from cache)"),
            "{}",
            lines[3]
        );
    }

    /// A scan that finishes inside one interval prints no intermediate line
    /// at all — the default cadence never adds noise to a fast, warm run.
    #[test]
    fn a_short_scan_prints_no_heartbeat() {
        let harness = Harness::new();
        let mut beat = harness.heartbeat(5);
        for _ in 0..3 {
            beat.beat(0, 0, 3);
            harness.advance(1);
        }
        assert!(
            harness.lines().is_empty(),
            "three seconds of walking under a five-second interval stays quiet"
        );
    }
}
