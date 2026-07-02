use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::sync::broadcast;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

// ── Log entry types (kept for backward compat — frontend expects these) ────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

// ── Legacy Logger (DEPRECATED — will be removed after callers migrate to tracing) ─
//
// Still present so existing code compiles. Delegates real output to tracing.
// Do NOT add new uses — use `tracing::info!()` etc. directly.

pub struct Logger {
    buffer: VecDeque<LogEntry>,
    max_entries: usize,
    _log_file: Option<PathBuf>,
}

impl Logger {
    pub fn new(max_entries: usize, log_file: Option<PathBuf>) -> Self {
        Self {
            buffer: VecDeque::with_capacity(max_entries),
            max_entries,
            _log_file: log_file,
        }
    }

    pub fn push(&mut self, entry: LogEntry) {
        if self.buffer.len() >= self.max_entries {
            self.buffer.pop_front();
        }
        self.buffer.push_back(entry);
        // UI delivery is handled by UiLogLayer from tracing events
    }

    pub fn entries(&self) -> Vec<LogEntry> {
        self.buffer.iter().cloned().collect()
    }

    /// Legacy file append is a no-op — tracing handles file output now.
    pub fn append_to_file(&self, _entry: &LogEntry) {}
}

// ── Global broadcast channel ────────────────────────────────────────────────

static UI_LOG_TX: OnceLock<broadcast::Sender<LogEntry>> = OnceLock::new();

/// Get a clone of the global UI log sender.
pub fn get_ui_tx() -> broadcast::Sender<LogEntry> {
    UI_LOG_TX
        .get()
        .cloned()
        .unwrap_or_else(|| broadcast::channel(1).0)
}

// ── Legacy log() helpers (DEPRECATED, prefer tracing macros) ─────────────────

/// Create a log entry, push to ring buffer, emit to UI via broadcast.
/// Also forwards to tracing for file output — this bridges the legacy
/// `log()` API with the new tracing infrastructure.
pub fn log(
    _app_handle: &tauri::AppHandle,
    logger: &std::sync::Mutex<Logger>,
    level: LogLevel,
    source: LogSource,
    message: String,
) {
    // Forward to tracing with a structured field that UiLogLayer can use
    // to derive the correct LogSource. UiLogLayer extracts the `log_source`
    // field; if absent, it falls back to event.metadata().target().
    let target_str = match source {
        LogSource::Proxy => "proxy",
        LogSource::Subscription => "subscription",
        LogSource::Cache => "cache",
        LogSource::Installer => "installer",
        LogSource::Process => "process",
        LogSource::Settings => "settings",
        LogSource::System => "system",
    };
    match level {
        LogLevel::Error => tracing::error!(log_source = target_str, log_level = "error", "{message}"),
        LogLevel::Warning => tracing::warn!(log_source = target_str, log_level = "warning", "{message}"),
        LogLevel::Info => tracing::info!(log_source = target_str, log_level = "info", "{message}"),
        LogLevel::Success => tracing::info!(log_source = target_str, log_level = "success", "{message}"),
    }

    let entry = LogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: Utc::now().timestamp_millis(),
        message,
        level,
        source,
    };

    // Push to ring buffer (also sends via broadcast for UI)
    if let Ok(mut l) = logger.lock() {
        l.push(entry);
    }
}

/// Same as `log()` but without the Tauri event emit.
/// Now identical to `log()` since broadcast/Tauri emit is decoupled.
#[allow(dead_code)]
pub(crate) fn log_to_file(
    _app_handle: &tauri::AppHandle,
    _logger: &std::sync::Mutex<Logger>,
    _level: LogLevel,
    _source: LogSource,
    _message: String,
) {
}

/// Tauri command: log a message from the frontend.
#[tauri::command]
pub fn log_message(
    _app: tauri::AppHandle,
    _logger: tauri::State<'_, std::sync::Mutex<Logger>>,
    level: LogLevel,
    source: LogSource,
    message: String,
) {
    let target_str = match source {
        LogSource::Proxy => "proxy",
        LogSource::Subscription => "subscription",
        LogSource::Cache => "cache",
        LogSource::Installer => "installer",
        LogSource::Process => "process",
        LogSource::Settings => "settings",
        LogSource::System => "system",
    };
    match level {
        LogLevel::Error => tracing::error!(log_source = target_str, "{message}"),
        LogLevel::Warning => tracing::warn!(log_source = target_str, "{message}"),
        LogLevel::Info | LogLevel::Success => tracing::info!(log_source = target_str, "{message}"),
    }
}

/// Tauri command: return all current log entries.
#[tauri::command]
pub fn get_logs(logger: tauri::State<'_, std::sync::Mutex<Logger>>) -> Vec<LogEntry> {
    let l = logger.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = l.entries();

    // Also read from tracing JSONL file for cross-session continuity
    if let Some(ref path) = l._log_file {
        if path.exists() {
            let file_entries = read_tracing_log_file(path, l.max_entries);
            let existing_ids: std::collections::HashSet<String> =
                all.iter().map(|e| e.id.clone()).collect();
            for e in file_entries {
                if !existing_ids.contains(&e.id) {
                    all.push(e);
                }
            }
        }
    }

    all.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    all.truncate(l.max_entries);
    all
}

// ── Custom tracing Layer for UI log delivery ───────────────────────────────

struct UiLogLayer {
    sender: broadcast::Sender<LogEntry>,
}

impl<S> tracing_subscriber::Layer<S> for UiLogLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = StringVisitor::new();
        event.record(&mut visitor);
        let message = visitor.message;
        if message.is_empty() {
            return;
        }

        // Derive LogLevel: prefer structured 'log_level' field (from legacy
        // log() bridge which distinguishes Info vs Success), fall back to
        // mapping from tracing::Level (where both map to INFO).
        let level = if let Some(ref log_level) = visitor.log_level {
            match log_level.as_str() {
                "error" => LogLevel::Error,
                "warning" => LogLevel::Warning,
                "info" => LogLevel::Info,
                "success" => LogLevel::Success,
                _ => return,
            }
        } else {
            match *event.metadata().level() {
                tracing::Level::ERROR => LogLevel::Error,
                tracing::Level::WARN => LogLevel::Warning,
                tracing::Level::INFO => LogLevel::Info,
                _ => return,
            }
        };

        // Derive LogSource: prefer structured 'log_source' field from
        // legacy log() bridge, fall back to event target module path.
        let source = match visitor.log_source.as_deref() {
            Some(s) => match s {
                "proxy" => LogSource::Proxy,
                "subscription" => LogSource::Subscription,
                "cache" => LogSource::Cache,
                "installer" => LogSource::Installer,
                "process" => LogSource::Process,
                "settings" => LogSource::Settings,
                _ => LogSource::System,
            },
            None => target_to_source(event.metadata().target()),
        };

        let entry = LogEntry {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: Utc::now().timestamp_millis(),
            message,
            level,
            source,
        };
        let _ = self.sender.send(entry);
    }
}

fn target_to_source(target: &str) -> LogSource {
    if target.contains("proxy") {
        LogSource::Proxy
    } else if target.contains("subscription") {
        LogSource::Subscription
    } else if target.contains("cache") || target.contains("batcher") {
        LogSource::Cache
    } else if target.contains("installer") {
        LogSource::Installer
    } else if target.contains("process") {
        LogSource::Process
    } else if target.contains("settings") {
        LogSource::Settings
    } else {
        LogSource::System
    }
}

struct StringVisitor {
    message: String,
    log_source: Option<String>,
    log_level: Option<String>,
}
impl StringVisitor {
    fn new() -> Self {
        Self {
            message: String::new(),
            log_source: None,
            log_level: None,
        }
    }
}
impl tracing::field::Visit for StringVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let formatted = format!("{:?}", value);
            if formatted.starts_with('"') && formatted.ends_with('"') && formatted.len() >= 2 {
                self.message = formatted[1..formatted.len() - 1].to_string();
            } else {
                self.message = formatted;
            }
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "log_source" {
            self.log_source = Some(value.to_string());
        }
        if field.name() == "log_level" {
            self.log_level = Some(value.to_string());
        }
    }
}

// ── Read back tracing JSONL for get_logs ────────────────────────────────────

/// Parse a single line from tracing-appender JSON format into a LogEntry.
fn parse_tracing_jsonl_line(line: &str) -> Option<LogEntry> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let obj = v.as_object()?;

    // tracing JSON format: {"timestamp":"...","level":"INFO","fields":{"message":"..."},"target":"proxy"}
    let level_str = obj.get("level")?.as_str()?;
    let level = match level_str {
        "ERROR" => LogLevel::Error,
        "WARN" => LogLevel::Warning,
        "INFO" => LogLevel::Info,
        _ => return None,
    };

    let target = obj.get("target").and_then(|t| t.as_str()).unwrap_or("");

    // Derive source: prefer structured 'log_source' field (from legacy
    // log() bridge), fall back to target-based mapping.
    let source = obj
        .get("fields")
        .and_then(|f| f.get("log_source"))
        .and_then(|v| v.as_str())
        .and_then(|s| match s {
            "proxy" => Some(LogSource::Proxy),
            "subscription" => Some(LogSource::Subscription),
            "cache" => Some(LogSource::Cache),
            "installer" => Some(LogSource::Installer),
            "process" => Some(LogSource::Process),
            "settings" => Some(LogSource::Settings),
            _ => None,
        })
        .unwrap_or_else(|| target_to_source(target));

    // Extract message from fields.message
    let message = obj
        .get("fields")
        .and_then(|f| f.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    if message.is_empty() {
        return None;
    }

    let timestamp = obj
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or_else(|| Utc::now().timestamp_millis());

    Some(LogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp,
        message,
        level,
        source,
    })
}

/// Read the last `max` log entries from tracing-appender's JSONL file.
fn read_tracing_log_file(path: &Path, max: usize) -> Vec<LogEntry> {
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
    let read_from = file_size.saturating_sub((max as u64).saturating_mul(200));
    if reader.seek(SeekFrom::Start(read_from)).is_err() {
        return Vec::new();
    }

    let mut entries: Vec<LogEntry> = Vec::with_capacity(max);
    let mut buf = String::new();
    while reader.read_line(&mut buf).unwrap_or(0) > 0 {
        let trimmed = buf.trim().to_string();
        if !trimmed.is_empty() {
            if let Some(entry) = parse_tracing_jsonl_line(&trimmed) {
                entries.push(entry);
            }
        }
        buf.clear();
    }

    if entries.len() > max {
        entries = entries.split_off(entries.len() - max);
    }
    entries
}

// ── Initialisation ──────────────────────────────────────────────────────────

/// Set up the global tracing subscriber with:
///   1. JSON file appender (per-session file: `logs-{timestamp}.jsonl`)
///   2. Console output (for dev builds)
///   3. UI broadcast layer — delivers LogEntry to Tauri events
///
/// Also installs a panic hook that logs panics via `tracing::error!`.
///
/// Returns `(broadcast::Receiver, PathBuf)` — the receiver for UI log delivery
/// and the path to the log file (for backward-compat `get_logs`).
pub fn setup_logging(log_dir: &Path) -> (broadcast::Receiver<LogEntry>, PathBuf) {
    let (tx, rx) = broadcast::channel(1024);
    let _ = UI_LOG_TX.set(tx.clone());

    // Create log file with old naming pattern: logs-YYYYMMDD-HHMMSS.jsonl
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let log_file = log_dir.join(format!("logs-{timestamp}.jsonl"));

    // Cleanup old log files — keep at most 20
    if let Ok(entries) = std::fs::read_dir(log_dir) {
        let mut log_files: Vec<_> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("logs-"))
            .collect();
        log_files.sort_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok()));
        if log_files.len() > 20 {
            for file in log_files.drain(..log_files.len() - 20) {
                let _ = std::fs::remove_file(file.path());
            }
        }
    }

    // Open file for appending
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .expect("Failed to create log file");

    let (non_blocking_file, _guard) = tracing_appender::non_blocking(file);
    // Leak the guard so it lives for the process lifetime
    std::mem::forget(_guard);

    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(non_blocking_file)
        // No span events — only log messages
        .with_target(true)
        .with_thread_ids(false);

    // 2. UI broadcast layer
    let ui_layer = UiLogLayer { sender: tx.clone() };

    tracing_subscriber::registry()
        .with(
            EnvFilter::builder()
                .with_default_directive(tracing::Level::INFO.into())
                .from_env_lossy(),
        )
        .with(file_layer)
        .with(ui_layer)
        .init();

    // Bridge the `log` crate to `tracing` — all `log::info!()`, `log::error!()`,
    // etc. calls throughout the codebase (cache.rs, geoip.rs, persistence.rs,
    // profile/mod.rs, installer.rs, process.rs, port_pool.rs, lib.rs) are
    // forwarded to tracing and appear in JSONL + UI.
    let _ = tracing_log::LogTracer::init();

    // Panic hook
    std::panic::set_hook(Box::new(|panic_info| {
        let payload = panic_info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| {
                panic_info
                    .payload()
                    .downcast_ref::<String>()
                    .map(|s| s.as_str())
            })
            .unwrap_or("Unknown panic");
        let location = panic_info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        tracing::error!(
            target = "system",
            panic.location = %location,
            "CRITICAL PANIC: {payload}",
        );
    }));

    (rx, log_file)
}

// ── Direct UI log helper (for non-tracing callers like log_frontend) ─────────

/// Send a pre-formatted log entry to the UI broadcast channel directly.
/// Used by the `log_frontend` command bridge.
pub fn log_to_ui(message: String, level: LogLevel, source: LogSource) {
    let entry = LogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: Utc::now().timestamp_millis(),
        message,
        level,
        source,
    };
    let _ = get_ui_tx().send(entry);
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_level_variants_serialize() {
        assert_eq!(
            serde_json::to_value(&LogLevel::Info).unwrap(),
            serde_json::Value::String("info".into())
        );
        assert_eq!(
            serde_json::to_value(&LogLevel::Success).unwrap(),
            serde_json::Value::String("success".into())
        );
        assert_eq!(
            serde_json::to_value(&LogLevel::Warning).unwrap(),
            serde_json::Value::String("warning".into())
        );
        assert_eq!(
            serde_json::to_value(&LogLevel::Error).unwrap(),
            serde_json::Value::String("error".into())
        );
    }

    #[test]
    fn test_log_source_variants_serialize() {
        assert_eq!(
            serde_json::to_value(&LogSource::System).unwrap(),
            serde_json::Value::String("system".into())
        );
        assert_eq!(
            serde_json::to_value(&LogSource::Proxy).unwrap(),
            serde_json::Value::String("proxy".into())
        );
        assert_eq!(
            serde_json::to_value(&LogSource::Cache).unwrap(),
            serde_json::Value::String("cache".into())
        );
        assert_eq!(
            serde_json::to_value(&LogSource::Installer).unwrap(),
            serde_json::Value::String("installer".into())
        );
        assert_eq!(
            serde_json::to_value(&LogSource::Process).unwrap(),
            serde_json::Value::String("process".into())
        );
        assert_eq!(
            serde_json::to_value(&LogSource::Subscription).unwrap(),
            serde_json::Value::String("subscription".into())
        );
        assert_eq!(
            serde_json::to_value(&LogSource::Settings).unwrap(),
            serde_json::Value::String("settings".into())
        );
    }

    #[test]
    fn test_target_to_source_mapping() {
        assert_eq!(target_to_source("proxy::batcher"), LogSource::Proxy);
        assert_eq!(
            target_to_source("subscription::parser"),
            LogSource::Subscription
        );
        assert_eq!(target_to_source("cache"), LogSource::Cache);
        assert_eq!(target_to_source("installer"), LogSource::Installer);
        assert_eq!(target_to_source("process"), LogSource::Process);
        assert_eq!(target_to_source("settings"), LogSource::Settings);
        assert_eq!(target_to_source("lib"), LogSource::System);
    }

    #[test]
    fn test_log_entry_serde_roundtrip() {
        let e = LogEntry {
            id: "test-id".into(),
            timestamp: 1000,
            message: "hello world".into(),
            level: LogLevel::Warning,
            source: LogSource::Subscription,
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: LogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "test-id");
        assert_eq!(back.timestamp, 1000);
        assert_eq!(back.message, "hello world");
        assert!(json.contains("warning"));
        assert!(json.contains("subscription"));
    }

    #[test]
    fn test_logger_push_and_entries() {
        let mut l = Logger::new(10, None);
        assert!(l.entries().is_empty());
        l.push(LogEntry {
            id: "1".into(),
            timestamp: 1,
            message: "test".into(),
            level: LogLevel::Info,
            source: LogSource::System,
        });
        assert_eq!(l.entries().len(), 1);
    }

    #[test]
    fn test_logger_eviction() {
        let mut l = Logger::new(3, None);
        for i in 0..5 {
            l.push(LogEntry {
                id: i.to_string(),
                timestamp: i,
                message: format!("msg-{i}"),
                level: LogLevel::Info,
                source: LogSource::System,
            });
        }
        let entries = l.entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].message, "msg-2");
        assert_eq!(entries[2].message, "msg-4");
    }

    #[test]
    fn test_parse_tracing_jsonl_valid() {
        let line = r#"{"timestamp":"2025-01-01T00:00:00Z","level":"INFO","fields":{"message":"hello"},"target":"proxy::batcher"}"#;
        let entry = parse_tracing_jsonl_line(line);
        assert!(entry.is_some());
        let e = entry.unwrap();
        assert_eq!(e.message, "hello");
        assert_eq!(e.level, LogLevel::Info);
        assert_eq!(e.source, LogSource::Proxy);
    }

    #[test]
    fn test_parse_tracing_jsonl_error_level() {
        let line = r#"{"timestamp":"2025-01-01T00:00:00Z","level":"ERROR","fields":{"message":"boom"},"target":"system"}"#;
        let entry = parse_tracing_jsonl_line(line);
        assert!(entry.is_some());
        let e = entry.unwrap();
        assert_eq!(e.message, "boom");
        assert_eq!(e.level, LogLevel::Error);
    }

    #[test]
    fn test_parse_tracing_jsonl_invalid() {
        // DEBUG level is skipped
        let line = r#"{"timestamp":"2025-01-01T00:00:00Z","level":"DEBUG","fields":{"message":"debug"},"target":"system"}"#;
        assert!(parse_tracing_jsonl_line(line).is_none());
    }
}
