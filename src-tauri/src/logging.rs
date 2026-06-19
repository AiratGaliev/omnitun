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
            if let Ok(mut file) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
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

/// Tauri command: return all current log entries from ring buffer.
#[tauri::command]
pub fn get_logs(logger: tauri::State<'_, Mutex<Logger>>) -> Vec<LogEntry> {
    match logger.lock() {
        Ok(l) => l.entries(),
        Err(_) => Vec::new(),
    }
}
