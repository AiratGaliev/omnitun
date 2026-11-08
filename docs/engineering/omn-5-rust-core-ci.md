# OMN-5 — Rust boundaries and staged CI

## Evidence and limitations

This boundary map is based on the source tree and checked-in manifests at the OMN-5 implementation base (`dev`, 2026-11-08). The initial implementation pass was remote-only; a subsequent local Windows audit and GitHub Actions run added the verification evidence below. Items not executed remain explicitly listed as outstanding.

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

## Verification evidence (2026-11-08)

- Dev CI run `42485317188` on commit `1742fa7`: both frontend and Rust Core jobs completed successfully. The workflow executed the configured lint, formatting, typecheck, Vitest, Rust format, Core tests and Core Clippy steps.
- Local Windows frontend checks passed: `npm run lint`, `npm run format:check`, `npm run typecheck`, and `npm test` (89 passed, 6 skipped).
- Local Windows Rust checks passed: workspace tests (168 passed, 1 ignored), Core tests (35 passed), workspace/Core Clippy with warnings denied, and `cargo fmt --all -- --check`.
- `cargo metadata --locked` and `cargo tree -p omnitun-core` completed. The Core dependency graph contains only `base64`, `serde`, `serde_json`, `thiserror`, `tracing`, `url` and `uuid`; no Tauri, MCP transport, Tokio, SQLite or OS process dependency.
- The optional data-driven integration target compiled with `user-data-smoke`; the DNS-template test was corrected to read the Core-owned template used by production config generation.
- Local Windows `npm run test:mcp:artifact` passed against the built MCP sidecar: 27 tools, 8 resources, all resource reads, settings persistence/validation, secret redaction and cross-process process lifecycle checks.
- The first successful GitHub Rust job reported a Cargo cache miss. A subsequent same-lockfile run is still needed to confirm cache restoration.

## Verification still outstanding

1. A promotion PR `dev → test` running the Windows/macOS/Linux matrix.
2. A promotion PR `test → main` validating the final release-readiness workflow.
3. A safe version-tag run proving production Tauri bundles, the real target MCP sidecar and installer artifacts; only the Windows MCP artifact smoke was run locally.
4. Profile-driven sing-box fixture tests remain opt-in through `user-data-smoke` and are not automatically invoked by the promotion workflow. The external VPN/IP tests remain manual/ignored.
5. A repeated Actions run with the same lockfile to capture a Cargo cache hit and elapsed-time comparison.
6. Branch protection: the repository rulesets endpoint returned an empty list during this audit; the legacy branch-protection endpoint returned HTTP 403 for the connected integration, so required branch protection settings could not be confirmed.

The dependency boundary is now supported by Cargo metadata/tree output as well as manifest and import inspection. The historical billing-related runner admission failure was resolved before the successful run above; it was not a code failure.
