# OmniTun

OmniTun is a cross-platform desktop proxy client built with Tauri 2, React, TypeScript and Rust. It uses sing-box as the local proxy runtime and focuses on subscription management, server diagnostics, reliable persistence and transparent connection status.

## Capabilities

- Import subscription profiles from HTTP(S) URLs, pasted content and local files.
- Parse VLESS, VMess, Trojan, Shadowsocks, Hysteria2, TUIC, WireGuard, SOCKS5 and HTTP proxy entries.
- Start and stop the local sing-box process and manage the application's proxy inbound.
- Run Clash API preflight checks and parallel deep server tests.
- Maintain positive and negative server cache entries with TTL/backoff and stale-while-revalidate behavior.
- Enrich servers with GeoIP, country flags, city data and CDN detection.
- Filter servers by country and protocol.
- Display connection state, traffic statistics, refresh progress and application logs.
- Persist profiles, tested servers, cache entries and test history in SQLite.
- Configure network behavior, DNS, routing, anti-DPI, TLS verification and language.
- Expose an optional MCP server for headless proxy/profile workflows.

## Technology

| Layer | Stack |
|---|---|
| Desktop shell | Tauri 2 |
| Frontend | React 19, TypeScript, Vite 7 |
| UI and state | Tailwind CSS 4, Framer Motion, Zustand, Lucide |
| Backend | Rust, Tokio |
| Proxy runtime | sing-box |
| Persistence | SQLite, WAL, tokio-rusqlite |
| Diagnostics | Clash API, GeoIP MMDB, CDN range detection |
| Frontend tests | Vitest, Testing Library |
| Backend tests | Rust unit and integration tests |

## Architecture

The frontend invokes Tauri commands and subscribes to backend events. Rust owns the proxy process lifecycle, subscription processing, server testing, cache and persistence.

~~~~text
src/
  App.tsx
  store.ts
  types.ts
  components/
    screens/
      Dashboard.tsx
      Profiles.tsx
      Settings.tsx
      Logs.tsx
      Onboarding.tsx
      About.tsx
      Terms.tsx
  i18n/

src-tauri/src/
  lib.rs                 Tauri commands, application setup and orchestration
  events.rs              Typed backend-to-frontend events
  logging.rs             Structured logs and log persistence
  db/                    SQLite schema and migrations
  profile/mod.rs         Profile manager and incremental server pipeline
  proxy/
    mod.rs               Proxy domain types and errors
    config.rs             sing-box configuration generation
    process.rs            Cross-platform process lifecycle
    installer.rs          sing-box binary download and installation
    batcher.rs            Parallel server testing engine
    cache.rs              Server cache and batch-test orchestration
    port_pool.rs          RAII port allocation
    geoip.rs               GeoIP and CDN enrichment
  settings/mod.rs        Application settings
  subscription/          Subscription fetchers and protocol parsers
  mcp_server.rs           Optional MCP server implementation
  bin/mcp_server.rs       MCP server executable entry point

test-server/
  Docker-based local integration fixtures and protocol test servers
~~~~

## Development setup

Prerequisites:

- Node.js 22 or newer.
- Rust stable toolchain and Cargo.
- Tauri platform prerequisites for the target OS.
- Docker Desktop for the local multi-protocol test environment.

Install frontend dependencies:

~~~~bash
npm ci
~~~~

Start the desktop application in development mode:

~~~~bash
npm run tauri
~~~~

Start only the Vite frontend:

~~~~bash
npm run dev
~~~~

## Common commands

| Command | Purpose |
|---|---|
| npm run dev | Vite development server |
| npm run build | TypeScript check and frontend production build |
| npm run tauri | Run the Tauri desktop application |
| npm run tauri:build | Build the desktop application bundle |
| npm test | Run Vitest |
| npm run lint | Run ESLint |
| npm run format:check | Check frontend formatting |
| npm run mcp:build | Build and stage the target-specific MCP sidecar |
| npm run mcp:run | Build and run the stdio MCP server |
| cargo fmt -- --check | Check Rust formatting |
| cargo test | Run Rust unit and integration tests |
| cargo clippy --all-targets -- -D warnings | Run Clippy for all targets with warnings treated as errors |
| npm run test:mcp | Build the MCP sidecar and run isolated JSON-RPC functional checks |

## MCP server integration

OmniTun exposes one standalone stdio MCP server. It is intended to be launched by an MCP-capable host; it is not an HTTP service and does not listen on a TCP port.

~~~~bash
npm run mcp:build
npm run mcp:run
~~~~

mcp:build prepares omnitun-mcp-<target-triple> in src-tauri/binaries. Tauri release bundles include this target-specific executable as an external binary. Configure the MCP host to launch that executable directly with no arguments; keep the user's normal environment so the server can access the same %APPDATA%\\com.omnitun.desktop data directory as the desktop client. For local development, npm run mcp:run starts the Rust MCP binary with the current user's application data.

The server exposes proxy lifecycle, subscription/profile, settings, cache, GeoIP and diagnostics tools, plus read-only status/settings/profile/log resources. MCP settings updates are partial; accept_terms is intentionally not writable, and ipinfo_token is write-only (the token is never returned by settings tools/resources). A running GUI or MCP proxy is discovered through the shared PID file and sing-box config; MCP can inspect and stop that process, and rejects a second start rather than replacing it silently. Stopping a system-proxy session also resets Windows Internet Settings and WinHTTP.

npm run test:mcp exercises the JSON-RPC handshake, tool/resource discovery, settings validation and secret redaction, and shared-process detection/stop in a temporary application-data directory. It does not connect to a real VPN server or modify the real user's system proxy.

## Local integration environment

Start the Docker fixtures:

~~~~bash
docker compose -f test-server/docker-compose.yml up -d
~~~~

The environment includes sing-box protocol listeners, a mock HTTP server and local test profiles. See test-server/TEST-SERVERS.md for the current addresses and credentials. Test fixtures are intended for local development only.

A separate lightweight subscription fixture is available at test-server/sub_server.py. It serves plain and Base64 subscription content on 127.0.0.1:8095.

## Data locations

On Windows, application data is stored under:

~~~~text
%APPDATA%\com.omnitun.desktop\
~~~~

The directory contains the SQLite database, settings, logs, GeoIP data and the downloaded sing-box binary. SQLite uses WAL mode; when making a manual backup while the application is running, use SQLite's backup API or stop the application and preserve the database together with its WAL/SHM files.

## Operational and security notes

- The server list is kept as a last-known-good snapshot during refresh. A new snapshot is committed only after processing has produced persistable server records.
- Cancellation must propagate through preflight, DNS enrichment and batch testing. A cancelled test must not be recorded as a negative server result.
- TLS certificate verification is configurable. Disabling verification can make some endpoints connect, but it removes certificate authenticity checks and increases man-in-the-middle risk.
- Do not commit API tokens, private keys, user subscription URLs or real credentials.
- sing-box configuration and logs can contain sensitive connection metadata; redact them before sharing diagnostics.

## Project status

The repository contains an implemented desktop client and backend workflows. Frontend build/test/lint checks and Rust unit/integration checks are available through the commands above. Passing automated checks does not replace manual validation of real VPN connectivity, OS-specific proxy behavior or release bundles on each supported platform.
