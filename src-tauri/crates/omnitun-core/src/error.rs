use serde::ser::SerializeStruct;
use thiserror::Error;

// ─── Structured errors ─────────────────────────────────────────────────────
//
// DESIGN:
//   - ProxyError variants carry ErrorDetail { code, message }.
//   - ErrorCode is serialized as camelCase (via #[serde(rename_all)]),
//     so the serialized code (e.g. "subscriptionDuplicateUrl") matches
//     i18n keys in src/i18n/{en,ru}.json directly.
//   - The frontend calls t(err.code, { defaultValue: err.message }) — no
//     intermediate mapping table needed.
//   - Io variant wraps std::io::Error via #[from] for ergonomic `?` propagation.
//     Serialized with code "ioError" + io::Error's display string.
//
// LOGGING PRESERVATION:
//   - Display for ErrorDetail returns only `message` (no variant prefix).
//   - Display for ProxyError delegates to ErrorDetail via #[error("{0}")].
//   - Result: format!("Failed to add subscription: {e}") produces
//     "Failed to add subscription: Subscription with URL '...' already exists"
//     — identical to pre-refactor output.
//
// MIGRATION SAFETY:
//   - All existing call sites use constructor pattern ProxyError::Variant(format!(...)).
//   - None pattern-match on variants, so changing inner type String → ErrorDetail
//     is mechanical and compiler-verified.
//   - Helper constructors ProxyError::{config,process,subscription,install}
//     keep call sites terse.

/// Machine-readable error codes.
///
/// SERIALIZATION: `#[serde(rename_all = "camelCase")]` converts Rust variants
/// like `SubscriptionDuplicateUrl` to JSON `"subscriptionDuplicateUrl"`.
/// This string IS the i18n key — the frontend calls `t(err.code)` directly.
///
/// NAMING CONVENTION:
///   - Variant name = `<Category><Specifics>` in PascalCase.
///   - Serialized key = `<category><Specifics>` in camelCase.
///   - Each variant MUST have a matching key in src/i18n/en.json AND ru.json.
///     If a key is missing, the frontend falls back to err.message (English dev text).
///
/// When adding a new variant:
///   1. Add it here (in the appropriate category section).
///   2. Use it at the relevant error site in Rust.
///   3. Add the camelCase key to src/i18n/en.json AND src/i18n/ru.json.
///   4. (Optional) Verify by triggering the error and checking the toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    // ── Subscription ─────────────────────────────────────────────────────
    /// URL already exists in profiles list (add_subscription).
    SubscriptionDuplicateUrl,
    /// Profile ID not found (remove, refresh).
    SubscriptionNotFound,
    /// HTTP fetch failed (network error, timeout, non-200).
    SubscriptionFetchFailed,
    /// Parser failed to decode base64 / parse URI scheme.
    SubscriptionParseFailed,
    /// Subscription content parsed but yielded 0 usable servers.
    SubscriptionNoServers,
    /// The current refresh/import operation was explicitly cancelled.
    SubscriptionCancelled,
    /// SQLite persistence (save/load profiles/servers) failed.
    SubscriptionSaveFailed,

    // ── Config ───────────────────────────────────────────────────────────
    /// sing-box config generation failed (template parse, JSON build).
    ConfigGenerationFailed,
    /// Server uses a protocol not supported by build_outbound.
    ConfigUnsupportedProtocol,
    /// No server profiles provided to generate_config.
    ConfigNoServers,

    // ── Process ──────────────────────────────────────────────────────────
    /// sing-box process already running when start was called.
    ProcessAlreadyRunning,
    /// Failed to spawn sing-box binary.
    ProcessStartFailed,
    /// Failed to kill an existing sing-box process.
    ProcessKillFailed,
    /// Process started but proxy port not reachable (health check failed).
    ProcessNotReachable,

    // ── Install ──────────────────────────────────────────────────────────
    /// Failed to download sing-box binary (network error, HTTP error).
    InstallDownloadFailed,
    /// Failed to extract zip / tar.gz archive.
    InstallExtractFailed,
    /// Binary not found inside the archive.
    InstallBinaryNotFound,
    /// Failed to determine latest version from GitHub API.
    InstallLatestVersionFailed,

    // ── IO ───────────────────────────────────────────────────────────────
    /// Wraps std::io::Error (from #[from]). Generic filesystem error.
    IoError,
}

/// Structured error detail: machine-readable code + human-readable message.
///
/// `message` is the English developer-facing text (used for logs and as
/// frontend fallback if no i18n translation exists for `code`).
#[derive(Debug, Clone)]
pub struct ErrorDetail {
    pub code: ErrorCode,
    pub message: String,
}

impl ErrorDetail {
    #[inline]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Display only the human message — variant prefix is irrelevant for logs.
/// This preserves the existing log format after migration:
/// `format!("Failed to add subscription: {e}")` →
/// `"Failed to add subscription: Subscription with URL '...' already exists"`
impl std::fmt::Display for ErrorDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Proxy error enum. Each variant wraps ErrorDetail except Io (which wraps
/// std::io::Error directly via #[from] for ergonomic `?` propagation).
///
/// Display delegates to ErrorDetail / io::Error — no variant prefix in output.
#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("{0}")]
    Config(ErrorDetail),

    #[error("{0}")]
    Process(ErrorDetail),

    #[error("{0}")]
    Subscription(ErrorDetail),

    #[error("{0}")]
    Install(ErrorDetail),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl ProxyError {
    // ── Typed constructors ──────────────────────────────────────────────
    // Keep call sites terse: `ProxyError::subscription(ErrorCode::..., "msg")`
    // instead of `ProxyError::Subscription(ErrorDetail::new(ErrorCode::..., "msg"))`.

    #[inline]
    pub fn config(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self::Config(ErrorDetail::new(code, msg))
    }

    #[inline]
    pub fn process(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self::Process(ErrorDetail::new(code, msg))
    }

    #[inline]
    pub fn subscription(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self::Subscription(ErrorDetail::new(code, msg))
    }

    #[inline]
    pub fn install(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self::Install(ErrorDetail::new(code, msg))
    }
}

/// Serialize as `{ "code": "camelCaseCode", "message": "..." }`.
///
/// IPC: when a Tauri command returns `Err(ProxyError)`, the frontend receives
/// this object as the rejection payload. The frontend's `isRustError` type guard
/// recognizes it, and `t(err.code, { defaultValue: err.message })` does
/// the translation lookup.
///
/// IMPORTANT: this changes the IPC payload shape from a plain string to an
/// object for migrated commands. Commands still returning `Result<_, String>`
/// (outlet_ip, get_real_ip, speed_test, download_ipinfo_db, update_settings,
/// get_connections) keep the old shape — `isRustError` returns false for
/// strings, and the frontend falls back to a generic message.
impl serde::Serialize for ProxyError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut st = serializer.serialize_struct("ProxyError", 2)?;
        match self {
            Self::Config(d) | Self::Process(d) | Self::Subscription(d) | Self::Install(d) => {
                st.serialize_field("code", &d.code)?;
                st.serialize_field("message", &d.message)?;
            }
            Self::Io(e) => {
                st.serialize_field("code", &ErrorCode::IoError)?;
                st.serialize_field("message", &e.to_string())?;
            }
        }
        st.end()
    }
}
