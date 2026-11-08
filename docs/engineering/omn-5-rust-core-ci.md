# OMN-5 — Rust boundaries and staged CI

## Evidence and limitations

This boundary map is based on the source tree and checked-in manifests at the OMN-5 implementation base (`dev`, 2026-11-08). The task environment provides GitHub repository inspection/editing but no local terminal or Rust build runner. Therefore `cargo metadata`, `cargo tree -e features`, local builds, cache-hit measurements and elapsed-time baselines have not been executed here. They remain verification items, not claimed results.

## Rust module boundary map

| Area | Responsibility observed in source | Boundary |
|---|---|---|
| `src-tauri/src/lib.rs` | Tauri commands, app state, events, proxy orchestration, subscription lifecycle and app startup | Desktop host |
| `src-tauri/src/proxy/mod.rs` | Proxy manager, installation, process lifecycle, PID/status and app-global runtime state | Desktop host; Core models/errors re-exported for compatibility |
| `src-tauri/src/proxy/config.rs` | Loopback port allocation, host status publication and config persistence adapter | Thin host adapter around Core config construction |
| `src-tauri/src/proxy/batcher.rs` | Tokio worker, cancellation, port pool, sing-box child process, HTTP/IP tests and mutable runtime settings | Desktop host/integration |
| `src-tauri/src/profile/mod.rs` | SQLite reads/writes, snapshot commits, app state, process/network orchestration and profile persistence | Desktop host; serializable server model is Core-owned |
| `src-tauri/src/subscription/parser.rs` | Base64/URI parsing, profile validation, SNI normalization and deterministic IDs | Core parser; host path remains a compatibility re-export |
| `src-tauri/src/settings/mod.rs` | Settings value types plus settings.json manager | Value types/defaults/normalization in Core; file I/O remains on host |
| `src-tauri/src/db/` | SQLite connection pool and migrations | Host persistence |
| `src-tauri/src/mcp_server.rs` | RMCP tool/resource schemas, JSON-RPC handler, settings/profile/process adapters | MCP protocol/adapter layer; consumes Core-owned types through compatibility exports |
| `src-tauri/src/bin/mcp_server.rs` | Stdio process entry point and platform data-directory selection | MCP executable/OS adapter |
| `src-tauri/tauri.conf.json` | Tauri build hooks and external MCP sidecar declaration | Release packaging boundary |

## Extracted Core

`src-tauri/crates/omnitun-core` is a Cargo workspace package containing:

- `error.rs`: shared error codes, structured error details and proxy error contracts.
- `profile.rs`: `ServerProfile` and domain helpers, preserving serde field names/defaults.
- `settings.rs`: `AppSettings`, `CustomCountry`, defaults and legacy-value normalization; no settings file I/O.
- `subscription/parser.rs`: subscription decoding/parsing, protocol profile parsing, validation and deterministic IDs.
- `proxy/config.rs`: deterministic sing-box config construction, outbound builders, config redaction and batch config construction.

The Core manifest contains serialization, error, base64, tracing, URL and UUID libraries only. It does not declare Tauri, Tauri plugins/build, WebView, SQLite, Tokio, reqwest, OS bindings, process management or RMCP/MCP transport. The base sing-box JSON template is a Core resource; the host retains filesystem writes and dynamic port allocation.

## Test classification

| Level | Coverage | Trigger |
|---|---|---|
| Core unit/contract | Parser tests, config/outbound tests, Core-owned models and defaults | Dev CI |
| Desktop host/component | In-memory SQLite/profile persistence and host unit tests | Promotion CI |
| MCP handler/contracts | Settings secret redaction, argument bounds, routing mode, tool/resource discovery | Promotion CI |
| Profile/sing-box config smoke | Sanitized fixture profile data and local sing-box binary | Explicit `user-data-smoke` opt-in |
| External VPN/IP smoke | Real sing-box process and external exit-IP service | Ignored manual tests only |
| Release artifact smoke | Launch already-built target sidecar and exercise JSON-RPC over stdio | Release tag, before artifact upload |

Profile-driven smoke tests require `OMNITUN_TEST_DATA_DIR`; they do not infer APPDATA, HOME or developer application data. Required fixtures/binaries and sing-box config validation fail explicitly when missing/invalid. External VPN tests are opt-in/ignored and fail when explicitly selected but the upstream service or route is unavailable. Process/temp-directory cleanup is guarded by RAII in the manual system tests.

## CI tiers

- Dev: frontend lint/format/typecheck/unit tests plus Core format/test/Clippy on one Ubuntu runner. No Vite production bundle, Tauri build, MCP executable, installers or system/network integration.
- `dev → test`: frontend regression; workspace unit/component tests and MCP handler contracts on Windows, macOS and Linux. Prepare the Tauri external-bin placeholder only for build-script path validation; do not build a production sidecar or installer.
- `test → main`: final frontend/Rust/MCP regression and application version metadata check; no installer packaging.
- Version tag: production frontend, Tauri bundles, real MCP sidecar and installers, followed by a smoke test of the built sidecar before artifact upload/publication.

## Verification still required

The first PR Actions attempt failed before any step ran: GitHub returned failed jobs with no runner name, no steps, no log URL and no job-step records. This is not evidence of a Rust/TypeScript compilation failure. Do not mark commands as passed until the relevant runner steps execute.

When runner capacity is available, capture:
1. Dev CI duration and Cargo cache hit/miss.
2. A repeat on the same runner/toolchain/lockfile to verify cache reuse.
3. Promotion PR results across Windows, macOS and Linux.
4. Test-to-main readiness checks.
5. A safe version-tag release run confirming the real sidecar and installer artifacts.

The dependency boundary above is based on manifests and inspected imports. Full Cargo metadata/tree output and timing data remain uncollected.
