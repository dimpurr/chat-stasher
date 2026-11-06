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
    /// When the last line was printed, or `None` before the first falls due.
    last_emitted: Option<Instant>,
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
            last_emitted: None,
            prefix: format!("[{prefix}]"),
            now: Box::new(now),
            sink: Box::new(sink),
        }
    }

    /// One snapshot finished. Prints when the interval since the last line
    /// has elapsed (or immediately when this heartbeat was built with a
    /// zero interval — the fixture shape, one line per event).
    ///
    /// The counters are the walk's own, passed by the caller rather than
    /// accumulated here, so the line can never disagree with the report the
    /// same walk is building.
    pub fn beat(&mut self, host: &str, scanned: usize, from_cache: usize, in_repo: usize) {
        let now = (self.now)();
        let due = match self.last_emitted {
            None => true,
            Some(last) => now.duration_since(last) >= self.interval,
        };
        if !due {
            return;
        }
        self.last_emitted = Some(now);
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
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A clock a test advances by hand, plus a sink that collects lines.
    #[derive(Clone)]
    struct Harness {
        now: Rc<RefCell<Instant>>,
        lines: Rc<RefCell<Vec<String>>>,
    }

    impl Harness {
        fn new() -> Self {
            let base = Instant::now();
            Harness {
                now: Rc::new(RefCell::new(base)),
                lines: Rc::new(RefCell::new(Vec::new())),
            }
        }

        fn advance(&self, secs: u64) {
            *self.now.borrow_mut() += Duration::from_secs(secs);
        }

        fn lines(&self) -> Vec<String> {
            self.lines.borrow().clone()
        }

        fn heartbeat(&self, interval_secs: u64) -> ScanHeartbeat {
            let now = Rc::clone(&self.now);
            let lines = Rc::clone(&self.lines);
            ScanHeartbeat::with_parts(
                "export",
                Duration::from_secs(interval_secs),
                move || *now.borrow(),
                move |line| lines.borrow_mut().push(line.to_string()),
            )
        }
    }

    /// S3's own sentence: a long scan prints at least one line per interval,
    /// and not one per event. With a 5 s interval, events at 1 s spacing
    /// line out at 0 s and every 5 s after.
    #[test]
    fn a_long_scan_lines_out_once_per_interval_not_per_event() {
        let harness = Harness::new();
        let mut beat = harness.heartbeat(5);
        for _ in 0..12 {
            beat.beat("host-a", 1, 0, 12);
            harness.advance(1);
        }
        let lines = harness.lines();
        assert_eq!(
            lines.len(),
            3,
            "12 one-second events under a 5-second interval: lines at 0s, 5s, 10s — {lines:?}"
        );
        assert!(lines[0].starts_with("[export] scan         : scanned "));
        assert!(lines[0].contains("scanned 1 of 12"), "counts include in_repo: {}", lines[0]);
        assert!(lines[1].contains("(0 from cache)"), "{}", lines[1]);
    }

    /// The zero interval is the fixture shape (one line per event), and the
    /// counts on each line are the caller's own at that moment — a walk that
    /// has finished 3 of 4 snapshots says 3, cache hits included.
    #[test]
    fn a_zero_interval_lines_every_event_with_the_caller_counts() {
        let harness = Harness::new();
        let mut beat = harness.heartbeat(0);
        beat.beat("host-a", 1, 0, 4);
        beat.beat("host-a", 2, 1, 4);
        beat.beat("host-b", 3, 1, 4);
        beat.beat("host-b", 4, 2, 4);
        let lines = harness.lines();
        assert_eq!(lines.len(), 4);
        assert!(lines[3].contains("scanned 4 of 4 snapshots (2 from cache)"), "{}", lines[3]);
    }

    /// A scan that finishes inside one interval prints no intermediate line
    /// at all — the default cadence never adds noise to a fast, warm run.
    #[test]
    fn a_short_scan_prints_no_heartbeat() {
        let harness = Harness::new();
        let mut beat = harness.heartbeat(5);
        for _ in 0..3 {
            beat.beat("host-a", 0, 0, 3);
            harness.advance(1);
        }
        assert!(
            harness.lines().is_empty(),
            "three seconds of walking under a five-second interval stays quiet"
        );
    }
}
