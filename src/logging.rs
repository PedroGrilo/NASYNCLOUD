//! Logger: writes to stderr and keeps the recent lines for the interface's
//! activity view.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};
use serde::Serialize;

/// How many lines the interface can scroll back through.
const KEEP: usize = 1000;

#[derive(Clone, Serialize)]
pub struct Line {
    pub seq: u64,
    /// Unix time, seconds.
    pub ts: i64,
    pub level: &'static str,
    pub msg: String,
}

struct Logger {
    /// Level for nasdrive's own messages; other crates only report warnings.
    level: LevelFilter,
    lines: Mutex<(u64, VecDeque<Line>)>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

pub fn init(verbose: bool) {
    let logger = LOGGER.get_or_init(|| Logger { level: if verbose { LevelFilter::Debug } else { LevelFilter::Info }, lines: Mutex::new((0, VecDeque::new())) });
    if log::set_logger(logger).is_ok() {
        log::set_max_level(logger.level.max(LevelFilter::Warn));
    }
}

/// Kept lines with a sequence number above `after`.
pub fn since(after: u64) -> Vec<Line> {
    let Some(logger) = LOGGER.get() else { return Vec::new() };
    let lines = logger.lines.lock().unwrap_or_else(|e| e.into_inner());
    lines.1.iter().filter(|l| l.seq > after).cloned().collect()
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Error => "error",
        Level::Warn => "warn",
        Level::Info => "info",
        Level::Debug => "debug",
        Level::Trace => "trace",
    }
}

impl Log for Logger {
    fn enabled(&self, meta: &Metadata) -> bool {
        let target = meta.target();
        if target.starts_with("nasdrive") {
            meta.level() <= self.level
        } else if target.starts_with("fuser") {
            meta.level() <= Level::Error
        } else {
            meta.level() <= Level::Warn
        }
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let msg = record.args().to_string();
        let stamp = nasdrive::remote::timestamp(ts); // "2026-11-12 153012"
        let iso = format!("{}T{}:{}:{}Z", &stamp[..10], &stamp[11..13], &stamp[13..15], &stamp[15..17]);
        let _ = writeln!(std::io::stderr(), "[{iso} {:<5} {}] {msg}", record.level(), record.target());
        if record.level() <= Level::Info {
            let mut lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
            lines.0 += 1;
            let seq = lines.0;
            lines.1.push_back(Line { seq, ts, level: level_name(record.level()), msg });
            if lines.1.len() > KEEP {
                lines.1.pop_front();
            }
        }
    }

    fn flush(&self) {}
}
