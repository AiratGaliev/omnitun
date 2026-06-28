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
    Vpn,
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

/// Create a log entry, push to ring buffer, append to file, emit Tauri event.
pub fn log(
    app_handle: &AppHandle,
    logger: &Mutex<Logger>,
    level: LogLevel,
    source: LogSource,
    message: String,
) {
    let entry = LogEntry {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now().timestamp_millis(),
        message,
        level,
        source,
    };

    if let Ok(mut l) = logger.lock() {
        l.push(entry.clone());
        l.append_to_file(&entry);
    }

    let _ = app_handle.emit("log-entry", &entry);
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

/// Tauri command: return all current log entries from ring buffer.
#[tauri::command]
pub fn get_logs(logger: tauri::State<'_, Mutex<Logger>>) -> Vec<LogEntry> {
    match logger.lock() {
        Ok(l) => l.entries(),
        Err(_) => Vec::new(),
    }
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
        l.push(make_entry("b", LogLevel::Warning, LogSource::Vpn));
        let e = l.entries();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].message, "a");
        assert!(matches!(e[0].level, LogLevel::Info));
        assert!(matches!(e[1].source, LogSource::Vpn));

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
            LogSource::Vpn,
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
