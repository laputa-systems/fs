//! Minimal terminal progress reporting.
//!
//! Filesystem workers update [`Progress`] with relaxed atomic operations.  A
//! renderer may sample the counters from a separate thread without putting a
//! lock or an event allocation on the copy hot path.  The renderer is
//! deliberately opt-in and only writes to stderr when stderr is a TTY; the
//! caller owns the start/finish lifecycle and can disable it for
//! `--no-progress`.

use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rustix::termios::{isatty, tcgetwinsize};

const DEFAULT_WIDTH: usize = 80;
const START_DELAY: Duration = Duration::from_millis(150);
const REFRESH_INTERVAL: Duration = Duration::from_millis(80);

/// Hot-path counters sampled by a progress renderer.
#[derive(Debug, Default)]
pub struct Progress {
    scanned_entries: AtomicU64,
    compared_files: AtomicU64,
    planned_files: AtomicU64,
    planned_bytes: AtomicU64,
    completed_files: AtomicU64,
    completed_bytes: AtomicU64,
    streamed_bytes: AtomicU64,
    cloned_bytes: AtomicU64,
    hashed_bytes: AtomicU64,
    skipped_files: AtomicU64,
    deleted_entries: AtomicU64,
    discovery_done: AtomicBool,
    prune_active: AtomicBool,
    finished: AtomicBool,
}

/// A consistent-enough point-in-time copy of [`Progress`] for rendering or
/// reporting.  Counter fields are relaxed snapshots and may reflect workers
/// completing adjacent operations in different orders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub scanned_entries: u64,
    pub compared_files: u64,
    pub planned_files: u64,
    pub planned_bytes: u64,
    pub completed_files: u64,
    pub completed_bytes: u64,
    pub streamed_bytes: u64,
    pub cloned_bytes: u64,
    pub hashed_bytes: u64,
    pub skipped_files: u64,
    pub deleted_entries: u64,
    pub discovery_done: bool,
    pub prune_active: bool,
    pub finished: bool,
}

impl Progress {
    /// Create counters with all values at zero and all phases inactive.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a relaxed snapshot of all counters.
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            scanned_entries: self.scanned_entries.load(Ordering::Relaxed),
            compared_files: self.compared_files.load(Ordering::Relaxed),
            planned_files: self.planned_files.load(Ordering::Relaxed),
            planned_bytes: self.planned_bytes.load(Ordering::Relaxed),
            completed_files: self.completed_files.load(Ordering::Relaxed),
            completed_bytes: self.completed_bytes.load(Ordering::Relaxed),
            streamed_bytes: self.streamed_bytes.load(Ordering::Relaxed),
            cloned_bytes: self.cloned_bytes.load(Ordering::Relaxed),
            hashed_bytes: self.hashed_bytes.load(Ordering::Relaxed),
            skipped_files: self.skipped_files.load(Ordering::Relaxed),
            deleted_entries: self.deleted_entries.load(Ordering::Relaxed),
            discovery_done: self.discovery_done.load(Ordering::Acquire),
            prune_active: self.prune_active.load(Ordering::Acquire),
            finished: self.finished.load(Ordering::Acquire),
        }
    }

    #[inline]
    pub fn add_scanned_entries(&self, amount: u64) {
        self.scanned_entries.fetch_add(amount, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_scanned_entry(&self) {
        self.add_scanned_entries(1);
    }

    #[inline]
    pub fn add_compared_files(&self, amount: u64) {
        self.compared_files.fetch_add(amount, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_compared_file(&self) {
        self.add_compared_files(1);
    }

    #[inline]
    pub fn add_planned_files(&self, amount: u64) {
        self.planned_files.fetch_add(amount, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_planned_file(&self, logical_bytes: u64) {
        self.add_planned_files(1);
        self.add_planned_bytes(logical_bytes);
    }

    #[inline]
    pub fn add_planned_bytes(&self, amount: u64) {
        self.planned_bytes.fetch_add(amount, Ordering::Relaxed);
    }

    #[inline]
    pub fn add_completed_files(&self, amount: u64) {
        self.completed_files.fetch_add(amount, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_completed_file(&self, logical_bytes: u64) {
        self.add_completed_files(1);
        self.add_completed_bytes(logical_bytes);
    }

    #[inline]
    pub fn add_completed_bytes(&self, amount: u64) {
        self.completed_bytes.fetch_add(amount, Ordering::Relaxed);
    }

    #[inline]
    pub fn add_streamed_bytes(&self, amount: u64) {
        self.streamed_bytes.fetch_add(amount, Ordering::Relaxed);
    }

    #[inline]
    pub fn add_cloned_bytes(&self, amount: u64) {
        self.cloned_bytes.fetch_add(amount, Ordering::Relaxed);
    }

    #[cfg(test)]
    #[inline]
    pub fn add_hashed_bytes(&self, amount: u64) {
        self.hashed_bytes.fetch_add(amount, Ordering::Relaxed);
    }

    #[inline]
    pub fn add_skipped_files(&self, amount: u64) {
        self.skipped_files.fetch_add(amount, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_skipped_file(&self) {
        self.add_skipped_files(1);
    }

    #[inline]
    pub fn add_deleted_entries(&self, amount: u64) {
        self.deleted_entries.fetch_add(amount, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_deleted_entry(&self) {
        self.add_deleted_entries(1);
    }

    /// Mark source discovery complete.  This freezes the planning denominator
    /// for the determinate meter.
    pub fn set_discovery_done(&self, done: bool) {
        self.discovery_done.store(done, Ordering::Release);
    }

    /// Mark sync's destructive destination-prune phase active/inactive.
    pub fn set_prune_active(&self, active: bool) {
        self.prune_active.store(active, Ordering::Release);
    }

    /// Mark the operation complete.  Renderers use this as their stop signal.
    pub fn set_finished(&self, finished: bool) {
        self.finished.store(finished, Ordering::Release);
    }

    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

/// Whether the process should render interactive progress to stderr.
///
/// `--no-progress` is represented by `disabled`; no force option exists in V1.
pub fn progress_enabled(disabled: bool) -> bool {
    if disabled {
        return false;
    }
    let stderr = io::stderr();
    // `IsTerminal` is useful on targets where rustix has no termios backend;
    // rustix is used below for the actual width query.
    isatty(&stderr) || stderr.is_terminal()
}

/// A dependency-free, single-line stderr renderer.
///
/// `start` and `finish` are intentionally explicit so callers can tie the
/// renderer to the operation lifecycle.  Short operations finish before the
/// start delay and produce no output.  The renderer owns one sampling thread
/// only while progress is enabled.
pub struct ProgressRenderer {
    progress: Arc<Progress>,
    enabled: bool,
    start_delay: Duration,
    refresh_interval: Duration,
    started: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    started_at: Option<Instant>,
}

impl fmt::Debug for ProgressRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProgressRenderer")
            .field("enabled", &self.enabled)
            .field("running", &self.handle.is_some())
            .finish()
    }
}

impl ProgressRenderer {
    /// Create a renderer using the terminal detected on stderr.
    pub fn new(progress: Arc<Progress>, no_progress: bool) -> Self {
        Self::with_enabled(progress, !no_progress && progress_enabled(false))
    }

    /// Create a renderer with explicit enablement, useful for tests and for a
    /// caller that has already performed terminal policy selection.
    pub fn with_enabled(progress: Arc<Progress>, enabled: bool) -> Self {
        Self {
            progress,
            enabled,
            start_delay: START_DELAY,
            refresh_interval: REFRESH_INTERVAL,
            started: Arc::new(AtomicBool::new(false)),
            handle: None,
            started_at: None,
        }
    }

    /// Override timing thresholds; primarily useful to make renderer tests
    /// deterministic without changing production defaults.
    #[cfg(test)]
    pub fn with_timing(mut self, start_delay: Duration, refresh_interval: Duration) -> Self {
        self.start_delay = start_delay;
        self.refresh_interval = refresh_interval.max(Duration::from_millis(1));
        self
    }

    #[cfg(test)]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Start sampling counters.  This is a no-op when disabled.
    pub fn start(&mut self) {
        if !self.enabled || self.handle.is_some() {
            return;
        }
        let progress = Arc::clone(&self.progress);
        let started = Arc::clone(&self.started);
        let delay = self.start_delay;
        let interval = self.refresh_interval;
        self.started_at = Some(Instant::now());
        self.handle = Some(thread::spawn(move || {
            let start = Instant::now();
            while !progress.finished() {
                let elapsed = start.elapsed();
                if elapsed >= delay {
                    break;
                }
                thread::sleep((delay - elapsed).min(Duration::from_millis(20)));
            }
            if progress.finished() {
                return;
            }
            started.store(true, Ordering::Release);
            let mut stderr = io::stderr();
            let started_at = start;
            loop {
                if progress.finished() {
                    break;
                }
                let snapshot = progress.snapshot();
                let width = terminal_width(&stderr);
                let line = render_line(snapshot, width, started_at.elapsed());
                let _ = write!(stderr, "\r\x1b[K{line}");
                let _ = stderr.flush();
                thread::sleep(interval);
            }
        }));
    }

    /// Finish the renderer.  A successful operation draws one final state and
    /// terminates the line; an error clears the line so the caller's
    /// diagnostic starts at column zero.
    pub fn finish(&mut self, success: bool) {
        self.progress.set_finished(true);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        if !self.started.load(Ordering::Acquire) {
            return;
        }
        let mut stderr = io::stderr();
        if success {
            let elapsed = self
                .started_at
                .map(|start| start.elapsed())
                .unwrap_or_default();
            let line = render_line(self.progress.snapshot(), terminal_width(&stderr), elapsed);
            let _ = write!(stderr, "\r\x1b[K{line}\n");
        } else {
            let _ = write!(stderr, "\r\x1b[K");
        }
        let _ = stderr.flush();
    }
}

impl Drop for ProgressRenderer {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.finish(false);
        }
    }
}

fn terminal_width(stderr: &io::Stderr) -> usize {
    tcgetwinsize(stderr)
        .ok()
        .map(|size| usize::from(size.ws_col))
        .filter(|width| *width > 0)
        .unwrap_or(DEFAULT_WIDTH)
}

fn render_line(snapshot: ProgressSnapshot, width: usize, elapsed: Duration) -> String {
    let width = width.max(1);
    if snapshot.prune_active {
        let mut line = format!(
            "pruning  {} scanned  {} deleted",
            compact_count(snapshot.scanned_entries),
            compact_count(snapshot.deleted_entries)
        );
        truncate_to_width(&mut line, width);
        return line;
    }

    let rate = if elapsed.is_zero() {
        0.0
    } else {
        snapshot.completed_bytes as f64 / elapsed.as_secs_f64()
    };
    if !snapshot.discovery_done {
        let mut line = format!(
            "|  {} scanned  {} copied  {}  {} changed",
            compact_count(snapshot.scanned_entries),
            format_bytes(snapshot.completed_bytes),
            format_rate(rate),
            compact_count(snapshot.completed_files)
        );
        truncate_to_width(&mut line, width);
        return line;
    }

    let total = snapshot.planned_bytes;
    let done = snapshot.completed_bytes.min(total);
    let percent = if total == 0 {
        if snapshot.planned_files == 0 { 100 } else { 0 }
    } else {
        ((done.saturating_mul(100)) / total).min(100)
    };
    let mut line = if width >= 72 {
        let bar_width = (width.saturating_sub(53)).clamp(8, 24);
        format!(
            "{}/{} {}% {}  {}  {} files",
            format_bytes(done),
            format_bytes(total),
            percent,
            progress_bar(percent, bar_width),
            format_rate(rate),
            compact_count(snapshot.completed_files)
        )
    } else if width >= 42 {
        format!(
            "{}/{} {}%  {}",
            format_bytes(done),
            format_bytes(total),
            percent,
            format_rate(rate)
        )
    } else if width >= 20 {
        format!(
            "{}/{} {}% {}",
            format_bytes(done),
            format_bytes(total),
            percent,
            format_rate(rate)
        )
    } else {
        format!("{}% {}", percent, format_rate(rate))
    };
    truncate_to_width(&mut line, width);
    line
}

fn progress_bar(percent: u64, width: usize) -> String {
    let filled = width.saturating_mul(percent as usize) / 100;
    format!(
        "[{}{}]",
        "█".repeat(filled),
        "░".repeat(width.saturating_sub(filled))
    )
}

fn truncate_to_width(value: &mut String, width: usize) {
    if value.chars().count() <= width {
        return;
    }
    let truncated: String = value.chars().take(width).collect();
    *value = truncated;
}

fn compact_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn format_bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit < UNITS.len() - 1 {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{}B", value)
    } else {
        format!("{amount:.1}{}", UNITS[unit])
    }
}

fn format_rate(value: f64) -> String {
    if value <= 0.0 {
        return "0B/s".to_owned();
    }
    format!("{}/s", format_bytes(value.min(u64::MAX as f64) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_are_atomic_and_snapshot_values() {
        let progress = Progress::new();
        progress.record_scanned_entry();
        progress.record_compared_file();
        progress.record_planned_file(4096);
        progress.record_completed_file(4096);
        progress.add_streamed_bytes(1024);
        progress.add_cloned_bytes(2048);
        progress.add_hashed_bytes(512);
        progress.record_skipped_file();
        progress.record_deleted_entry();
        progress.set_discovery_done(true);
        progress.set_prune_active(true);
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.scanned_entries, 1);
        assert_eq!(snapshot.compared_files, 1);
        assert_eq!(snapshot.planned_files, 1);
        assert_eq!(snapshot.planned_bytes, 4096);
        assert_eq!(snapshot.completed_files, 1);
        assert_eq!(snapshot.completed_bytes, 4096);
        assert_eq!(snapshot.streamed_bytes, 1024);
        assert_eq!(snapshot.cloned_bytes, 2048);
        assert_eq!(snapshot.hashed_bytes, 512);
        assert_eq!(snapshot.skipped_files, 1);
        assert_eq!(snapshot.deleted_entries, 1);
        assert!(snapshot.discovery_done);
        assert!(snapshot.prune_active);
    }

    #[test]
    fn determinate_progress_never_exceeds_requested_width() {
        let snapshot = ProgressSnapshot {
            planned_files: 10,
            planned_bytes: 100,
            completed_files: 5,
            completed_bytes: 50,
            discovery_done: true,
            ..ProgressSnapshot::default()
        };
        for width in [1, 10, 20, 42, 80, 120] {
            assert!(
                render_line(snapshot, width, Duration::from_secs(1))
                    .chars()
                    .count()
                    <= width.max(1)
            );
        }
    }

    #[test]
    fn indeterminate_and_prune_modes_include_their_phase() {
        let indeterminate = render_line(
            ProgressSnapshot {
                scanned_entries: 12,
                completed_files: 2,
                completed_bytes: 100,
                ..ProgressSnapshot::default()
            },
            80,
            Duration::from_secs(1),
        );
        assert!(indeterminate.contains("scanned"));
        let prune = render_line(
            ProgressSnapshot {
                scanned_entries: 10,
                deleted_entries: 3,
                prune_active: true,
                ..ProgressSnapshot::default()
            },
            80,
            Duration::from_secs(1),
        );
        assert!(prune.starts_with("pruning"));
        assert!(prune.contains("3 deleted"));
    }

    #[test]
    fn no_progress_is_a_true_noop() {
        let progress = Arc::new(Progress::new());
        let mut renderer = ProgressRenderer::new(progress.clone(), true);
        assert!(!renderer.enabled());
        renderer.start();
        renderer.finish(true);
        assert!(progress.finished());
    }

    #[test]
    fn short_enabled_operation_does_not_start_meter() {
        let progress = Arc::new(Progress::new());
        let mut renderer = ProgressRenderer::with_enabled(progress, true)
            .with_timing(Duration::from_secs(1), Duration::from_millis(1));
        renderer.start();
        renderer.finish(true);
        assert!(!renderer.started.load(Ordering::Acquire));
    }

    #[test]
    fn byte_and_rate_formatting_is_stable() {
        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(1024), "1.0KiB");
        assert_eq!(format_rate(0.0), "0B/s");
        assert!(format_rate(1024.0).ends_with("/s"));
    }
}
