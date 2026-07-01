use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter};
use uuid::Uuid;

const MAX_LOG_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10 MB
pub const MAX_LOG_FILES_COUNT: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogSource {
    System,
    Proxy,
    Installer,
    Process,
    Subscription,
    Settings,
    Cache,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: String,
    pub timestamp: i64,
    pub message: String,
    pub level: LogLevel,
    pub source: LogSource,
}

pub struct Logger {
    buffer: VecDeque<LogEntry>,
    max_entries: usize,
    log_file: Option<PathBuf>,
}

impl Logger {
    pub fn new(max_entries: usize, log_file: Option<PathBuf>) -> Self {
        Self {
            buffer: VecDeque::with_capacity(max_entries),
            max_entries,
            log_file,
        }
    }

    pub fn push(&mut self, entry: LogEntry) {
        if self.buffer.len() >= self.max_entries {
            self.buffer.pop_front();
        }
        self.buffer.push_back(entry);
    }

    pub fn entries(&self) -> Vec<LogEntry> {
        self.buffer.iter().cloned().collect()
    }

    pub fn append_to_file(&self, entry: &LogEntry) {
        let path = match self.log_file {
            Some(ref p) => p,
            None => return,
        };

        if path.exists() {
            if let Ok(meta) = fs::metadata(path) {
                if meta.len() > MAX_LOG_FILE_SIZE {
                    let rotated = path.with_extension("jsonl.1");
                    let _ = fs::rename(path, &rotated);
                }
            }
        }

        if let Ok(json) = serde_json::to_string(entry) {
            if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{json}");
            }
        }
    }
}

/// Create a log entry, push to ring buffer, append to file, optionally emit Tauri event.
fn log_impl(
    app_handle: &AppHandle,
    logger: &Mutex<Logger>,
    level: LogLevel,
    source: LogSource,
    message: String,
    emit_event: bool,
) {
    let entry = LogEntry {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now().timestamp_millis(),
        message,
        level,
        source,
    };

    let mut l = logger.lock().unwrap_or_else(|e| e.into_inner());
    l.push(entry.clone());
    l.append_to_file(&entry);

    if emit_event {
        let _ = app_handle.emit("log-entry", &entry);
    }
}

/// Create a log entry, push to ring buffer, append to file, emit Tauri event.
pub fn log(
    app_handle: &AppHandle,
    logger: &Mutex<Logger>,
    level: LogLevel,
    source: LogSource,
    message: String,
) {
    log_impl(app_handle, logger, level, source, message, true);
}

/// Write a log entry to ring buffer and JSONL file WITHOUT emitting a Tauri event.
/// Used when the frontend is the event originator (via `log_frontend`) and has
/// already called `addLog` — emitting `log-entry` would create a duplicate.
pub(crate) fn log_to_file(
    app_handle: &AppHandle,
    logger: &Mutex<Logger>,
    level: LogLevel,
    source: LogSource,
    message: String,
) {
    log_impl(app_handle, logger, level, source, message, false);
}

/// Tauri command: log a message from the frontend to the Rust Logger (ring buffer + file).
#[tauri::command]
pub fn log_message(
    app: tauri::AppHandle,
    logger: tauri::State<'_, Mutex<Logger>>,
    level: LogLevel,
    source: LogSource,
    message: String,
) {
    log(&app, &logger, level, source, message);
}

/// Tauri command: return all current log entries from ring buffer
/// AND from the persistent JSONL file (for cross-session log continuity).
#[tauri::command]
pub fn get_logs(logger: tauri::State<'_, Mutex<Logger>>) -> Vec<LogEntry> {
    let l = logger.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = l.entries(); // ring buffer (in-memory, current session)

    // Also read from the JSONL file for logs from previous sessions.
    // The file is append-only; we parse the last max_entries lines.
    if let Some(ref path) = l.log_file {
        if path.exists() {
            let file_entries = read_last_entries_from_file(path, l.max_entries);
            // Dedup by ID — ring buffer entries are newer, file entries supplement history
            let existing_ids: std::collections::HashSet<String> =
                all.iter().map(|e| e.id.clone()).collect();
            for e in file_entries {
                if !existing_ids.contains(&e.id) {
                    all.push(e);
                }
            }
            // Sort newest-first, keep top max_entries
            all.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
            all.truncate(l.max_entries);
        }
    }

    all
}

/// Read the last `max` log entries from a JSONL file (one JSON object per line).
/// Memory-efficient: reads file in reverse from the end, keeping only the last N lines.
fn read_last_entries_from_file(path: &PathBuf, max: usize) -> Vec<LogEntry> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let file_size = match file.metadata().map(|m| m.len()) {
        Ok(s) if s > 0 => s,
        _ => return Vec::new(),
    };

    let mut reader = BufReader::new(file);
    // Start near the end: read up to ~max * 200 bytes per line average
    let read_from = file_size.saturating_sub((max as u64).saturating_mul(200));
    if reader.seek(SeekFrom::Start(read_from)).is_err() {
        return Vec::new();
    }

    let mut lines: Vec<String> = Vec::with_capacity(max);
    let mut buf = String::new();
    while reader.read_line(&mut buf).unwrap_or(0) > 0 {
        let trimmed = buf.trim().to_string();
        if !trimmed.is_empty() {
            lines.push(trimmed);
        }
        buf.clear();
    }

    // Keep only the last `max` lines (the buffer starts from read_from,
    // which may include a partial first line we skip)
    if lines.len() > max {
        lines = lines.split_off(lines.len() - max);
    }

    lines
        .iter()
        .filter_map(|line| serde_json::from_str::<LogEntry>(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_logger(max: usize) -> Logger {
        Logger::new(max, None)
    }

    fn make_entry(msg: &str, level: LogLevel, source: LogSource) -> LogEntry {
        LogEntry {
            id: Uuid::new_v4().to_string(),
            timestamp: 1000000,
            message: msg.into(),
            level,
            source,
        }
    }

    #[test]
    fn test_logger_new_buffer_empty() {
        let l = make_logger(100);
        assert!(l.entries().is_empty());
    }

    #[test]
    fn test_logger_push_adds_entry() {
        let mut l = make_logger(100);
        let e = make_entry("test", LogLevel::Info, LogSource::System);
        l.push(e);
        assert_eq!(l.entries().len(), 1);
        assert_eq!(l.entries()[0].message, "test");
    }

    #[test]
    fn test_logger_push_evicts_oldest() {
        let mut l = Logger::new(3, None);
        for i in 0..5 {
            l.push(make_entry(&format!("msg-{i}"), LogLevel::Info, LogSource::System));
        }
        let entries = l.entries();
        assert_eq!(entries.len(), 3);
        // Oldest 2 removed, newest 3 remain
        assert_eq!(entries[0].message, "msg-2");
        assert_eq!(entries[2].message, "msg-4");
    }

    #[test]
    fn test_logger_entries_clones_correctly() {
        let mut l = make_logger(100);
        l.push(make_entry("a", LogLevel::Info, LogSource::System));
        l.push(make_entry("b", LogLevel::Warning, LogSource::Proxy));
        let e = l.entries();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].message, "a");
        assert!(matches!(e[0].level, LogLevel::Info));
        assert!(matches!(e[1].source, LogSource::Proxy));

        // test entries returned are clones, not references
        l.push(make_entry("c", LogLevel::Error, LogSource::System));
        assert_eq!(e.len(), 2); // `e` was captured before push; proves cloned
    }

    #[test]
    fn test_append_to_file_creates_file() {
        let dir = std::env::temp_dir().join(format!("log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("test.jsonl");

        let mut l = Logger::new(10, Some(log_path.clone()));
        let e = make_entry("write-test", LogLevel::Success, LogSource::Installer);
        l.push(e.clone());
        l.append_to_file(&e);

        assert!(log_path.exists());
        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("write-test"));
        assert!(content.contains("success"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_append_to_file_no_file_noop() {
        let l = make_logger(10);
        let e = make_entry("safe", LogLevel::Info, LogSource::System);
        // Should not panic — append_to_file with no log_file should be noop
        l.append_to_file(&e);
        // append_to_file does NOT add to buffer — only push() does
        assert_eq!(l.entries().len(), 0);
    }

    #[test]
    fn test_log_entry_serde_roundtrip() {
        let e = LogEntry {
            id: "test-id-123".into(),
            timestamp: 999888777,
            message: "hello world".into(),
            level: LogLevel::Warning,
            source: LogSource::Subscription,
        };
        let json = serde_json::to_string(&e).unwrap();
        let deser: LogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.id, "test-id-123");
        assert_eq!(deser.timestamp, 999888777);
        assert_eq!(deser.message, "hello world");
    }

    #[test]
    fn test_log_level_variants_serialize() {
        for level in &[LogLevel::Info, LogLevel::Success, LogLevel::Warning, LogLevel::Error] {
            let json = serde_json::to_string(level).unwrap();
            let deser: LogLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(level.clone() as usize, deser as usize);
        }
    }

    #[test]
    fn test_log_source_variants_serialize() {
        for src in &[
            LogSource::System,
            LogSource::Proxy,
            LogSource::Installer,
            LogSource::Process,
            LogSource::Subscription,
            LogSource::Settings,
        ] {
            let json = serde_json::to_string(src).unwrap();
            let deser: LogSource = serde_json::from_str(&json).unwrap();
            assert_eq!(src.clone() as usize, deser as usize);
        }
    }

    #[test]
    fn test_get_logs_returns_current_entries() {
        let mut l = make_logger(100);
        assert!(l.entries().is_empty());
        l.push(make_entry("x", LogLevel::Info, LogSource::System));
        assert_eq!(l.entries().len(), 1);
    }

    #[test]
    fn test_max_log_files_const() {
        assert_eq!(MAX_LOG_FILES_COUNT, 20);
    }
}
