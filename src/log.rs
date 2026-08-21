//! Process-wide log buffer behind the log panel.
//!
//! A global rather than state on the workspace: the planner runs on a
//! background thread and terminals report from their own tasks, and none of
//! them hold a handle to the UI. The panel polls [`version`] to notice new
//! output.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Oldest entries are dropped once the buffer grows past this.
const CAPACITY: usize = 500;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Error,
}

#[derive(Clone)]
pub struct Entry {
    /// `HH:MM:SS`, UTC — see [`timestamp`].
    pub time: String,
    pub level: Level,
    pub message: String,
}

static ENTRIES: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
/// Bumped on every change, so the panel can poll for new output cheaply
/// instead of every writer needing a way to wake the UI.
static VERSION: AtomicUsize = AtomicUsize::new(0);

pub fn info(message: impl Into<String>) {
    push(Level::Info, message.into());
}

pub fn error(message: impl Into<String>) {
    push(Level::Error, message.into());
}

fn push(level: Level, message: String) {
    // Mirrored to stderr so the same information is available when the app is
    // run from a terminal or headlessly.
    eprintln!("[harmonium] {message}");
    if let Ok(mut entries) = ENTRIES.lock() {
        entries.push(Entry {
            time: timestamp(),
            level,
            message,
        });
        let overflow = entries.len().saturating_sub(CAPACITY);
        if overflow > 0 {
            entries.drain(..overflow);
        }
    }
    VERSION.fetch_add(1, Ordering::Relaxed);
}

pub fn entries() -> Vec<Entry> {
    ENTRIES
        .lock()
        .map(|entries| entries.clone())
        .unwrap_or_default()
}

pub fn version() -> usize {
    VERSION.load(Ordering::Relaxed)
}

pub fn clear() {
    if let Ok(mut entries) = ENTRIES.lock() {
        entries.clear();
    }
    VERSION.fetch_add(1, Ordering::Relaxed);
}

/// Wall clock as `HH:MM:SS`. UTC, because turning epoch seconds into local
/// time needs a timezone database and no dependency here is worth that for a
/// debug panel.
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let day = secs % 86_400;
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}
