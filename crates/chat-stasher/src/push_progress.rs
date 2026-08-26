//! Throttled, newline-oriented progress output for `push`.
//!
//! rustic_core 0.12.0 ships a `ProgressBars` / `RusticProgress` callback pair
//! (`rustic_core::progress`). The backup path creates one `bytes` bar
//! (`"backing up..."`) via `Repository::progress_bytes`, and the parallel
//! archiver feeds the resulting [`Progress`] one `inc(bytes)` per data chunk,
//! a `set_length(total_bytes)` from a pre-scan, and a `finish()` once the
//! snapshot is sealed. We plug a [`PushProgressBars`] into the repository via
//! `Repository::new_with_progress` and translate those byte events into
//! newline-terminated `progress:` lines.
//!
//! The callbacks are byte-granular: rustic never tells us which file
//! (session/shard) a chunk belongs to, so `shards=` is a byte-proportional
//! estimate against the known sealed-shard total, reaching `total/total`
//! exactly on `finish()`. No session content or session id is ever printed.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustic_core::{Progress, ProgressBars, ProgressType, RusticProgress};

/// Wall-clock gap between two progress lines during a long push.
///
/// 5 s over a ~3.9 GiB / 3147-shard run ≈ a dozen lines — enough to prove
/// liveness without flooding the launchd log.
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

/// A progress callback that receives rustic's byte events for one backup.
///
/// Thread-safe: the archiver calls `inc` from many rayon workers at once, so
/// the byte counters are atomics and the throttle state is mutex-guarded.
pub struct PushProgress {
    /// Sealed shard total (the `N` in `shards=N/total`).
    total_shards: u64,
    /// Minimum wall-clock gap between two emitted lines.
    interval: Duration,
    /// When the reporter was created (elapsed is measured from here).
    start: Instant,
    /// `set_length(total)` from rustic's pre-scan.
    total_bytes: AtomicU64,
    /// Bytes fed via `inc(...)` so far.
    done_bytes: AtomicU64,
    /// Throttle state, serialised so concurrent archiver threads don't race it.
    state: Mutex<PushProgressState>,
    /// Injectable clock (tests freeze/advance it); production uses `Instant::now`.
    now: Box<dyn Fn() -> Instant + Send + Sync>,
    /// Where each line goes; production writes to stderr, tests collect.
    sink: Box<dyn Fn(&str) + Send + Sync>,
}

#[derive(Default)]
struct PushProgressState {
    last_emitted_at: Option<Instant>,
}

impl fmt::Debug for PushProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PushProgress")
            .field("total_shards", &self.total_shards)
            .field("interval", &self.interval)
            .field("total_bytes", &self.total_bytes.load(Ordering::Relaxed))
            .field("done_bytes", &self.done_bytes.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl PushProgress {
    /// Create a reporter for a stage holding `total_shards` sealed shards.
    pub fn new(total_shards: u64) -> Self {
        Self::new_with_parts(total_shards, PROGRESS_INTERVAL, Instant::now, default_sink)
    }

    fn new_with_parts(
        total_shards: u64,
        interval: Duration,
        now: impl Fn() -> Instant + Send + Sync + 'static,
        sink: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self {
            total_shards,
            interval,
            start: now(),
            total_bytes: AtomicU64::new(0),
            done_bytes: AtomicU64::new(0),
            state: Mutex::new(PushProgressState::default()),
            now: Box::new(now),
            sink: Box::new(sink),
        }
    }

    /// Record rustic's pre-scan byte total.
    fn note_total(&self, total: u64) {
        self.total_bytes.store(total, Ordering::Relaxed);
    }

    /// Record `inc` more bytes processed and maybe emit a progress line.
    fn note_inc(&self, inc: u64) {
        let done = self.done_bytes.fetch_add(inc, Ordering::Relaxed) + inc;
        self.maybe_emit(done);
    }

    /// Seal the run: always emit the final line (`shards` reaches `total/total`).
    fn note_finish(&self) {
        let done = self.done_bytes.load(Ordering::Relaxed);
        let total = self.total_bytes.load(Ordering::Relaxed);
        self.emit_line(done, total);
    }

    fn maybe_emit(&self, done: u64) {
        let now = (self.now)();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let time_due = match state.last_emitted_at {
            None => true,
            Some(last) => now.duration_since(last) >= self.interval,
        };
        if time_due {
            state.last_emitted_at = Some(now);
            self.emit_line(done, self.total_bytes.load(Ordering::Relaxed));
        }
    }

    fn emit_line(&self, done: u64, total: u64) {
        let shards = estimated_shards(done, total, self.total_shards);
        let elapsed = (self.now)().duration_since(self.start).as_secs();
        let line = format!(
            "progress: shards={shards}/{} bytes={done} elapsed={elapsed}s",
            self.total_shards
        );
        (self.sink)(&line);
    }
}

/// Byte-proportional estimate of how many shards the given byte progress
/// represents. `done` is clamped to `total_bytes` so the estimate never
/// overshoots the sealed-shard total.
fn estimated_shards(done_bytes: u64, total_bytes: u64, total_shards: u64) -> u64 {
    if total_bytes == 0 || total_shards == 0 {
        return 0;
    }
    let done = done_bytes.min(total_bytes);
    ((done as u128 * total_shards as u128) / total_bytes as u128) as u64
}

fn default_sink(line: &str) {
    eprintln!("{line}");
}

/// `&self`-owned handle handed to rustic's `Progress::new`.
///
/// `RusticProgress` is a foreign trait, and `Arc<PushProgress>` is not a
/// covered local type for the orphan rule, so the sharing handle is this
/// local newtype instead of `Arc<PushProgress>` directly.
#[derive(Debug, Clone)]
struct PushProgressHandle(Arc<PushProgress>);

impl RusticProgress for PushProgressHandle {
    fn is_hidden(&self) -> bool {
        false
    }

    fn set_length(&self, len: u64) {
        self.0.note_total(len);
    }

    fn set_title(&self, _title: &str) {}

    fn inc(&self, inc: u64) {
        self.0.note_inc(inc);
    }

    fn finish(&self) {
        self.0.note_finish();
    }
}

/// `ProgressBars` handed to `Repository::new_with_progress`.
///
/// Only the `Bytes` bar (the `"backing up..."` one) drives the reporter;
/// counter/spinner bars used by other repository internals stay hidden.
#[derive(Debug, Clone)]
pub struct PushProgressBars {
    progress: Arc<PushProgress>,
}

impl PushProgressBars {
    pub fn new(progress: Arc<PushProgress>) -> Self {
        Self { progress }
    }
}

impl ProgressBars for PushProgressBars {
    fn progress(&self, progress_type: ProgressType, _prefix: &str) -> Progress {
        if matches!(progress_type, ProgressType::Bytes) {
            Progress::new(PushProgressHandle(self.progress.clone()))
        } else {
            Progress::hidden()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// A controllable clock: the test advances `Arc<AtomicU64>` nanoseconds and
    /// the returned closure reports `base + nanos` as "now".
    fn fake_clock() -> (Arc<AtomicU64>, impl Fn() -> Instant + Send + Sync + 'static) {
        let base = Instant::now();
        let nanos = Arc::new(AtomicU64::new(0));
        let nanos_for_clock = nanos.clone();
        (nanos, move || {
            base + Duration::from_nanos(nanos_for_clock.load(Ordering::Relaxed))
        })
    }

    /// A sink that appends every emitted line to a shared vector.
    fn collect_sink() -> (
        Arc<Mutex<Vec<String>>>,
        impl Fn(&str) + Send + Sync + 'static,
    ) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let lines_for_sink = lines.clone();
        (lines, move |line: &str| {
            lines_for_sink
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(line.to_string());
        })
    }

    #[test]
    fn estimated_shards_scales_with_bytes_done() {
        assert_eq!(estimated_shards(0, 1000, 3147), 0);
        assert_eq!(estimated_shards(500, 1000, 3147), 1573);
        assert_eq!(estimated_shards(1000, 1000, 3147), 3147);
        assert_eq!(estimated_shards(2000, 1000, 3147), 3147); // clamped
        assert_eq!(estimated_shards(100, 0, 3147), 0); // total unknown
        assert_eq!(estimated_shards(100, 1000, 0), 0); // no shards at all
    }

    #[test]
    fn progress_lines_are_throttled_to_interval() {
        let (clock, now) = fake_clock();
        let (lines, sink) = collect_sink();
        let reporter = Arc::new(PushProgress::new_with_parts(
            3147,
            Duration::from_secs(5),
            now,
            sink,
        ));
        reporter.note_total(1000);

        reporter.note_inc(100); // t=0: first byte ever → emit
        clock.fetch_add(2 * 1_000_000_000, Ordering::Relaxed);
        reporter.note_inc(100); // t=2s: inside the 5s window → silent
        clock.fetch_add(4 * 1_000_000_000, Ordering::Relaxed);
        reporter.note_inc(100); // t=6s: 6s since the first emit → emit
        clock.fetch_add(2 * 1_000_000_000, Ordering::Relaxed);
        reporter.note_inc(100); // t=8s: inside the window again → silent
        reporter.note_finish(); // final line always

        let got = lines.lock().unwrap_or_else(|poison| poison.into_inner());
        assert_eq!(
            got.len(),
            3,
            "expected first-inc + 5s-tick + finish, got {got:?}"
        );
        assert!(
            got[1].contains("bytes=300"),
            "second line should be the 6s tick at 300 bytes, got {:?}",
            got[1]
        );
        assert!(
            got[2].contains("bytes=400"),
            "finish line must report the accumulated bytes (done<total here), got {:?}",
            got[2]
        );
    }

    #[test]
    fn progress_finish_emits_final_line_reaching_total() {
        let (_clock, now) = fake_clock();
        let (lines, sink) = collect_sink();
        let reporter = Arc::new(PushProgress::new_with_parts(
            3147,
            Duration::from_secs(3600),
            now,
            sink,
        ));
        reporter.note_total(1000);
        reporter.note_inc(1000); // done == total, so the finish line must reach total/total
        reporter.note_finish();

        let got = lines.lock().unwrap_or_else(|poison| poison.into_inner());
        assert_eq!(
            got.len(),
            2,
            "expected first-inc line + finish line, got {got:?}"
        );
        assert!(
            got.last().unwrap().contains("shards=3147/3147"),
            "finish must report shards=total/total, got {:?}",
            got.last()
        );
        assert!(
            got.last().unwrap().contains("bytes=1000"),
            "finish must report the last byte count, got {:?}",
            got.last()
        );
    }

    #[test]
    fn progress_bar_type_maps_bytes_to_reporter_and_hides_others() {
        let (_clock, now) = fake_clock();
        let (lines, sink) = collect_sink();
        let reporter = Arc::new(PushProgress::new_with_parts(
            1,
            Duration::from_secs(5),
            now,
            sink,
        ));
        let bars = PushProgressBars::new(reporter);

        let bytes_bar = bars.progress(ProgressType::Bytes, "backing up...");
        assert!(
            !bytes_bar.is_hidden(),
            "the backing-up bar must drive the reporter"
        );
        bytes_bar.set_length(10);
        bytes_bar.inc(10);
        bytes_bar.finish();

        assert!(
            bars.progress(ProgressType::Counter, "indexing...")
                .is_hidden(),
            "internal counters must not drive the reporter"
        );
        assert!(
            bars.progress(ProgressType::Spinner, "spinning...")
                .is_hidden(),
            "internal spinners must not drive the reporter"
        );

        let got = lines.lock().unwrap_or_else(|poison| poison.into_inner());
        assert!(!got.is_empty(), "bytes bar must emit progress lines");
        assert!(
            got.last().unwrap().contains("shards=1/1"),
            "last line must reach the shard total, got {:?}",
            got.last()
        );
    }

    #[test]
    fn progress_concurrent_inc_is_thread_safe_and_throttled() {
        let (_clock, now) = fake_clock();
        let (lines, sink) = collect_sink();
        let reporter = Arc::new(PushProgress::new_with_parts(
            10,
            Duration::from_secs(60),
            now,
            sink,
        ));
        reporter.note_total(10_000);

        let handles: Vec<_> = (0..8u64)
            .map(|i| {
                let reporter = reporter.clone();
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        reporter.note_inc(i + 1);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(
            reporter.done_bytes.load(Ordering::Relaxed),
            3600,
            "8 threads × 100 × (1..=8) must accumulate exactly"
        );

        reporter.note_finish();
        let got = lines.lock().unwrap_or_else(|poison| poison.into_inner());
        assert_eq!(
            got.len(),
            2,
            "800 incs must still be throttled to first-inc + finish, got {got:?}"
        );
        assert!(
            got.last().unwrap().contains("bytes=3600"),
            "finish must report the accumulated total, got {:?}",
            got.last()
        );
    }
}
