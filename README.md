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


## Development workflow and contributor rules

These rules apply to every contributor and AI coding agent working in this repository. They define the expected development process; a green CI run or a successful local build does not authorize bypassing review or promotion steps.

### Project direction and sources of truth

- Keep OmniTun a maintainable cross-platform desktop proxy client. Preserve the separation between the React/TypeScript frontend, Tauri desktop host, Rust application logic, sing-box runtime and optional stdio MCP server.
- Before changing behaviour, inspect the implementation, related tests, Cargo/npm manifests and lockfiles, Tauri configuration, and the affected GitHub Actions workflow. Treat the checked-out code and workflow files as authoritative for what is implemented today.
- Keep implementation scope focused. Prefer correcting the root cause over adding workarounds, duplicated logic or speculative abstractions.
- Treat the [OmniTun Product Development project](https://linear.app/omnitun/project/omnitun-product-development-1323720f6a88) as the shared roadmap and task home. Engineering policy and CI target architecture are documented in the [OmniTun Engineering Handbook](https://linear.app/omnitun/document/omnitun-engineering-handbook-workflow-and-ci-dd0e2d9b14b5). The Rust Core extraction and staged CI redesign are tracked by [OMN-5](https://linear.app/omnitun/issue/OMN-5/razdelit-rust-core-bystryj-dev-ci-i-release-build-prilozheniyamcp).
- Distinguish verified capabilities from planned work. Do not describe a proposed architecture, workflow or feature as implemented until the code and CI demonstrate it.

### AI-assisted development

Start each coding session by reading this README and the relevant source files, tests and workflow definitions. Do not rely on a previous chat, an issue title or an earlier implementation summary as a substitute for inspecting the current repository state.

For each task:

1. Identify the current behaviour, user-visible problem and root cause before editing.
2. Check the relevant architecture boundaries and dependency versions; verify version-sensitive Tauri, Rust, React or MCP APIs against their authoritative documentation when needed.
3. Agree the implementation scope with the linked Linear issue. Avoid unrelated cleanup, broad rewrites and unrequested product changes.
4. Add or update regression tests with the implementation. Keep fast unit tests deterministic and independent from real user data, external IP services and uncontrolled network conditions.
5. Run the checks appropriate to the change, inspect the diff, and correct failures at their source. Do not weaken or remove a test merely to obtain a green result.
6. Report exact commands and outcomes. State which checks were not run, which were skipped, and what still requires manual OS or real-network validation.

Do not claim that a command, test, build, review, commit, push or merge succeeded unless its result was actually observed. Do not mark a Linear issue Done before its completion criteria are met.

### Permanent branches

| Branch | Purpose | What may be merged into it |
|---|---|---|
| \`dev\` | Daily development and integration branch | Focused topic branches through PRs |
| \`test\` | Integrated release-candidate and acceptance branch | Only \`dev\` through a promotion PR |
| \`main\` | Stable release line | Only \`test\` through a release-promotion PR; an emergency hotfix is the sole documented exception |

The normal route is **\`topic branch → dev → test → main → version tag/release\`**. Do not skip a stage for routine feature work or bug fixes.

### Non-negotiable development rules

1. **Do not push directly to \`dev\`, \`test\` or \`main\`.** Create a topic branch and open a PR, including for documentation-only changes. Review the exact diff before merging.
2. **Do not send routine feature or fix branches directly to \`test\` or \`main\`.** Do not open \`dev → main\` or \`test → dev\` PRs. Normal promotion is \`dev → test → main\`.
3. **Start ordinary work from the latest \`dev\`.** Fetch and fast-forward the local branch before creating a topic branch. Do not base new work on stale local state, another unrelated feature branch, \`test\` or \`main\`.
4. **Keep each PR focused on one coherent task.** Do not combine unrelated refactors, speculative cleanup, dependency upgrades or unfinished work. Split independently reviewable changes.
5. **Promote only a coherent candidate.** A \`dev → test\` PR must contain the completed work intended for the candidate and must be internally consistent. Do not use promotion PRs to move partial work or to pause ongoing development.
6. **Never bypass a failing check.** Do not disable a workflow, weaken a test, remove a required status or retarget a PR solely to make a red check disappear. Find and fix the cause, or document a genuine infrastructure failure and rerun the affected check.
7. **Keep code, tests and documentation aligned.** Changes to behaviour, configuration, data formats, setup, security boundaries or developer workflow must include the corresponding tests and documentation in the same PR.
8. **Protect user data and credentials.** Never use a developer's real profiles, settings, tokens or databases as test fixtures. Do not commit API tokens, private keys, subscription URLs, proxy credentials, personal IP data or unredacted diagnostics.
9. **Preserve data compatibility.** Changes to SQLite schema or persisted settings must include safe migrations, failure handling and regression coverage. Do not delete or rewrite user data as an incidental part of a code change.
10. **Keep evidence honest.** Passing unit tests does not prove real VPN connectivity, system proxy behaviour, installer correctness or MCP host compatibility. State the exact coverage and remaining manual checks.
11. **Avoid long-lived branches without a clear reason.** Keep topic branches small and aligned with current \`dev\`; remove them only after they are no longer needed and repository permissions allow it.

**GitHub enforcement note:** the GitHub API currently reports \`dev\`, \`test\` and \`main\` as unprotected branches. The rules above are therefore the repository's required working agreement, but branch protection and required status checks are not yet enforced server-side. Do not claim that GitHub blocks direct pushes until that configuration is actually enabled and verified.

### Creating a topic branch

Start from an up-to-date \`dev\` and a clean working tree:

~~~bash
git status --short
git fetch origin
git switch dev
git pull --ff-only origin dev
git switch -c feat/<short-name>
~~~

Use a descriptive branch name and one of these prefixes:

| Prefix | Use |
|---|---|
| \`feat/*\` or \`feature/*\` | New user-facing functionality |
| \`fix/*\` | Normal bug fix |
| \`refactor/*\` | Internal restructuring without intended behaviour change |
| \`test/*\` | Test coverage or test infrastructure |
| \`docs/*\` | Documentation-only changes |
| \`chore/*\` | Maintenance, dependencies or tooling |
| \`hotfix/*\` | Emergency release-blocking fix only; not a substitute for a normal \`fix/*\` |

Avoid vague branch names such as \`work\`, \`changes\` or \`update\`. If a topic branch has fallen behind \`dev\`, update it deliberately and inspect the resulting diff before opening or updating a PR.

### Allowed Pull Request routes

| PR source → target | Type | Rule |
|---|---|---|
| \`feat/*\`, \`feature/*\`, \`fix/*\`, \`refactor/*\`, \`test/*\`, \`docs/*\`, \`chore/*\` → \`dev\` | Standard development | Use for focused, reviewable implementation work |
| \`dev → test\` | Promotion | Only a complete, internally consistent release candidate |
| \`test → main\` | Release promotion | Only after candidate checks and acceptance are complete |
| \`hotfix/* → main\` | Emergency exception | Only for a release-blocking defect that cannot wait for normal promotion; explain urgency and test impact |

An emergency hotfix merged to \`main\` must be brought back into \`dev\` (and then through the normal \`test\` promotion) by a follow-up PR unless the fix is already present there. Include the backport plan in the hotfix PR. Do not use this exception for ordinary feature work.

### Promotion gates

Before opening **\`dev → test\`**:

- All changes intended for the candidate are merged into \`dev\`; known release-blocking defects are resolved or explicitly assessed.
- Frontend checks, Rust checks and the applicable regression tests pass on the candidate SHA.
- Integration checks for proxy lifecycle, sing-box configuration, persistence and MCP are run where affected, using controlled fixtures rather than real user data or unstable external services.
- Relevant setup, security and developer documentation is updated.
- The PR description identifies the candidate scope, exact CI evidence, known limitations and any manual validation still required.

Before opening **\`test → main\`**:

- The candidate has been checked on the integrated \`test\` branch.
- Required CI checks pass and there are no unresolved release-blocking defects.
- The release notes, application version, updater metadata and documented behaviour match what will ship.
- The target platforms and release artifacts are understood; do not imply support for a platform that has not been built and validated.

If a promotion gate fails, stop the promotion. Fix the issue in a topic branch, merge the fix into \`dev\`, and prepare a new candidate when it is ready. Do not bypass the failed gate or silently change the PR target.

### CI and release policy

The intended distinction is: quick feedback during daily development, broader integration validation during promotion, and production packaging only for a release. Keep the following distinction visible until the staged CI redesign in OMN-5 is implemented.

**Current workflow behaviour (verified in \`.github/workflows/ci.yml\` and \`.github/workflows/release.yml\`):**

| Workflow | Trigger | Current checks |
|---|---|---|
| CI — frontend | Push to \`dev\`, \`test\`, \`main\`; PR targeting these branches | \`npm ci\`, ESLint, Prettier, Vitest and \`npm run build\` (TypeScript + Vite production frontend build) on Ubuntu |
| CI — Rust | Same push/PR triggers | Rust format and Clippy on Ubuntu; \`cargo test\` on Windows, macOS and Ubuntu; Rust cache is enabled per runner |
| CI — MCP sidecar preparation | Same CI runs | Creates a placeholder external-binary path for Tauri build-script validation; it does **not** compile the real MCP executable |
| Release | Push of a \`v*\` tag | Runs \`npm run tauri:build\` and builds the real MCP sidecar through the Tauri \`beforeBuildCommand\`; current release matrix builds Windows x86_64 and Linux x86_64 bundles |

The current CI does not run \`tauri build\`, create installers or execute \`npm run test:mcp\` on every push. However, it still runs a frontend production build and the Rust test/Clippy targets; \`cargo test\` must compile the test executables and their dependency graph. A Cargo dependency rebuild is not the same as a Tauri installer build.

**Target policy tracked by [OMN-5](https://linear.app/omnitun/issue/OMN-5/razdelit-rust-core-bystryj-dev-ci-i-release-build-prilozheniyamcp):**

| Stage | Intended checks |
|---|---|
| \`dev\` topic PRs | Fast frontend lint/format/type-check and unit tests; Rust formatting, independent Core unit tests and Core Clippy. No Tauri application build, installer packaging, real MCP executable build or system/integration suite |
| \`dev → test\` | Full applicable Rust/desktop/MCP integration tests, isolated sing-box fixtures and platform-specific validation. No production installers |
| \`test → main\` | Release-readiness regression, configuration, version and updater checks; avoid rebuilding installers or duplicating expensive work without a reason |
| Version tag / Release | Production frontend, Tauri application, real MCP executable/sidecar, supported installers, signatures and updater artifacts |

The target fast Rust pipeline depends on extracting a Tauri-independent \`omnitun-core\` crate and classifying the existing integration tests. Until OMN-5 is implemented and verified, do not assume that today's CI already follows the target stage separation. Do not simply delete slow tests to make CI faster: move them to the correct stage and preserve their coverage.

### Local checks before a PR

Run only the checks relevant to the change, but do not omit a check required by its acceptance criteria.

**Documentation-only changes** do not require npm installation, Rust compilation or an application build. Review the patch and run:

~~~bash
git diff --check
~~~

**Frontend changes** (from the repository root):

~~~bash
npm ci
npm run lint
npm run format:check
npm test
npm run build
~~~

**Rust changes** (from \`src-tauri/\`):

~~~bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
~~~

**MCP changes:** \`npm run test:mcp\` builds/stages the target-specific MCP sidecar and then runs isolated JSON-RPC functional checks. Run it for MCP changes and understand that it performs a real Rust MCP build; it is not a lightweight frontend/unit-test command.

**Desktop packaging:** \`npm run tauri:build\` creates the desktop application bundle/installers and builds the MCP sidecar through Tauri's \`beforeBuildCommand\`. Treat it as a release/package validation, not a routine check for every small change.

**Local integration fixtures:** start the isolated Docker test environment with:

~~~bash
docker compose -f test-server/docker-compose.yml up -d
~~~

Stop it after use with \`docker compose -f test-server/docker-compose.yml down\`. Use only the fixtures and credentials documented under \`test-server/\`; never replace them with real subscription profiles or credentials.

### Test isolation and system-level validation

- Unit tests must not require a running sing-box process, a real proxy account, a public IP lookup service or a developer's local profile.
- Integration tests must use dedicated fixtures, temporary directories, controlled local listeners and deterministic cleanup.
- The current data-driven Rust integration tests can inspect the application data directory (including \`profiles.json\`) and some system-level scenarios may use external connectivity. Until those tests are fully fixture-isolated, run them only with a disposable test application-data directory. Never point them at the normal user profile.
- A missing optional external binary or fixture must be reported explicitly as skipped/ignored with a visible reason. A required fixture missing from CI must fail the test; it must not silently turn the test green.
- Proxy startup/shutdown, OS system-proxy changes, process cleanup, SQLite persistence and failure recovery require tests appropriate to their platform and risk.
- Real VPN connectivity, DNS behaviour, OS proxy settings and installer/update flows may require manual validation on a real target OS. Record that evidence separately from automated CI.

### Pull Request description checklist

Every PR must include:

- **Goal:** the defect or requirement and why the change is needed.
- **Scope:** the key changes and explicitly excluded work.
- **Tests:** exact commands and observed results.
- **CI:** relevant workflow/check results, including failures and reruns.
- **Data and security:** impact on user data, credentials, permissions, processes and system proxy state.
- **Gaps:** checks not run, environmental limitations, risks and manual validation still required.
- **Documentation:** updated files or the reason no documentation change was necessary.
- **Routing:** why the source and target branches are correct; for promotion PRs, candidate scope and acceptance evidence; for hotfixes, urgency and backport plan.

Do not treat a successful build as proof that all OS-level behaviours or real proxy connections work.

### Linear task lifecycle

All OmniTun product and engineering work is tracked in the **Omnitun (OMN)** team and grouped under the [OmniTun — Product Development](https://linear.app/omnitun/project/omnitun-product-development-1323720f6a88) project where appropriate.

Use the existing team statuses:

\`Backlog → Todo → In Progress → Done\`

The system statuses \`Canceled\` and \`Duplicate\` remain available. Use the existing labels \`Bug\`, \`Feature\` and \`Improvement\`; do not create near-duplicates.

Each engineering issue must describe the problem/current behaviour, expected outcome, scope and exclusions, test plan, and verifiable acceptance criteria. Link related tasks instead of copying the same requirements into multiple issues.

For ordinary engineering delivery, **Done** means the implementation meets its acceptance criteria, scoped automated checks have passed, relevant documentation is updated, and the reviewed PR has been merged into \`dev\`. Promotion work is complete only when its own target-branch and acceptance criteria are satisfied. Creating a PR or writing code alone is not completion.


## Project status

The repository contains an implemented desktop client and backend workflows. Frontend build/test/lint checks and Rust unit/integration checks are available through the commands above. Passing automated checks does not replace manual validation of real VPN connectivity, OS-specific proxy behavior or release bundles on each supported platform.
