# AGENTS.md

<!-- Maintenance rule: Only add content that tells AI assistants WHAT TO DO or WHAT NOT TO DO.
     Implementation details, design rationale, and "how the system works" belong in ARCHITECTURE.md.
     If a section doesn't contain an actionable rule or constraint, it doesn't belong here. -->

Project-specific rules and conventions for AI assistants and contributors.

## High-Priority Rules

### Do NOT add fields to `AcpAgentManager` unless every alternative is exhausted

`AcpAgentManager` (in `crates/aionui-ai-agent/src/acp_agent.rs`) is already large and carries multiple overlapping state holders (e.g. `runtime_snapshot`, `state`, `preferred_mode`, `config`). New fields tend to duplicate semantics that `AcpRuntimeSnapshot` or `AcpState` already model, which fragments the source of truth and makes resume/new paths diverge.

Before adding a field:
1. Can the value live in `AcpRuntimeSnapshot`? (runtime/session-scoped state, including user-selected current_mode/current_model/config_selections)
2. Can it be derived from existing fields (`metadata`, `config`, `runtime_snapshot`, `state`)?
3. Can it be persisted via `acp_session.session_config` + `preload_persisted` instead of a new in-memory field?
4. If it must be in-memory and transient, can it be scoped to the call site (local variable, channel, task state) rather than the manager?

Only after exhausting the above — and explicitly documenting why each option is insufficient — add a new field. When doing so, also document its lifecycle (who writes, who reads, when it is invalidated) in a doc comment on the field.

### Logging

When planning or changing a critical path or hard-to-observe flow, evaluate whether logging needs to change. In implementation plans for such changes, briefly state whether logs will be added, existing observability is sufficient, or logs are intentionally unnecessary. Do not add logs for simple refactors, test-only changes, UI copy/style changes, or when existing tests, errors, metrics, or logs already provide enough observability.

Add structured logs only when they help make production behavior diagnosable or provide extra development detail. Production normally runs at `info`, while development runs at `debug`; therefore, information needed to troubleshoot production issues must be available at `info`, `warn`, or `error`, not only `debug`.

Use log levels as follows:
- `debug` for high-frequency or detailed development-only flow details and state transitions
- `info` for low-volume production-diagnostic lifecycle boundaries, important state changes, and non-sensitive correlation context
- `warn` for malformed or unexpected data that is safely handled
- `error` for contract violations or failed operations

Production-visible logs must not include sensitive payloads such as prompts, tool input/output, file contents, command bodies, tokens, secrets, or raw provider requests/responses. If such payloads are needed for local debugging, they must be behind explicit development-only guards and never enabled by default.

## Architecture

> For detailed background and design decisions, see [ARCHITECTURE.md](./ARCHITECTURE.md).

Cargo workspace organized in four layers: Foundation → Capability → Domain → Composition. Dependencies flow strictly downward.

### Crate Hierarchy & Dependencies

- ✅ Upper layers may depend on lower layers (including cross-layer)
- ✅ Same-layer interaction through trait abstractions only
- ❌ No lower-layer depending on upper-layer
- ❌ No circular dependencies
- Changes to foundation crates require impact assessment

### Domain Crate Structure

Every domain crate must follow:
- `lib.rs` — module exports only, no business logic
- `routes.rs` — export `domain_routes(state) -> Router`, handlers do request/response transformation only
- `service.rs` — sole location for business logic, must not import axum
- `state.rs` — `#[derive(Clone)]` RouterState holding Arc-wrapped dependencies

### API Conventions

- Route prefix: `/api/`
- Resource names: kebab-case
- Response format: `ApiResponse<T>` (success) / `ErrorResponse` (failure)
- All request/response types defined in `aionui-api-types`
- `aionui-api-types` must NOT depend on axum/tower or any HTTP framework
- Use `aionui_common::ApiError` only at API/HTTP boundaries such as routes and middleware. Service/domain code must prefer crate-owned errors (`ConversationError`, `TeamError`, etc.) and map them to `ApiError` in route modules. Do not introduce new `AppError` usages; it exists only as a temporary compatibility alias.

### WebSocket Events

- Format: `domain.camelCaseAction` (two-level structure)
- Message type: `WebSocketMessage<T>` (name + data)
- Existing kebab-case or three-level names are legacy — new events must follow the convention

### Data Layer

- Repository traits in `aionui-db`, prefixed with `I`
- Concrete implementations prefixed with `Sqlite`
- Row models in `aionui-db/src/models/`
- Params objects co-located in repository files
- Migrations: `NNN_descriptive_name.sql`, no manual DB modifications
- Services depend on traits, never on concrete implementations

### Dependency Injection

- `AppServices` is the sole service construction center
- Domain crates only define RouterState, never construct their own dependencies
- All assembly happens in `aionui-app`'s `build_*_state()` functions

### Security

- New endpoints must be evaluated for auth middleware requirement
- State-changing operations must be CSRF-protected
- Sensitive operations should have rate limiting
- Error responses must not leak internal details
- Secrets must never be hardcoded

## Code Style

- Rust 2024 edition, stable toolchain (pinned in `rust-toolchain.toml`)
- Use machine-level Rust tools from `/Users/richard/Coding Tools/bin`
  (`rustup`, `cargo`, `rustc`, `rustfmt`, `cargo-clippy`,
  `clippy-driver`, `rust-analyzer`). CI parity tools are `cargo-nextest`
  and `cargo-audit`. Let `rustup` honor this repo's `rust-toolchain.toml`;
  do not install repo-local Rust toolchains.
- Run `just` recipes with `PATH="/Users/richard/Coding Tools/bin:$PATH"` so
  recipe-internal commands resolve to the machine-level toolchain.
- Comments in English, commit messages in English
- Each `.rs` file follows single responsibility — one module, one concern
- Max 1000 lines per `.rs` file; split into submodules when approaching the limit

## Development Workflow

### Subprocess Spawning

New subprocess spawn sites must use `aionui_runtime::Builder::agent(program)` or `aionui_runtime::Builder::clean_cli(program)`. Do NOT use raw `tokio::process::Command`. See [ARCHITECTURE.md § Runtime Infrastructure](./ARCHITECTURE.md#runtime-infrastructure) for details.

### Pushing Code

Always use `just push` instead of `git push`.
It runs fmt → clippy → test before pushing, preventing CI failures.
Supports the same arguments as `git push` (e.g. `just push -u origin feat/branch`).

### Add Endpoint to Existing Crate

1. Request/response types → `aionui-api-types/src/{domain}.rs`
2. Handler function → `crates/aionui-{domain}/src/routes.rs`
3. Business logic → `crates/aionui-{domain}/src/service.rs`
4. Register route in `domain_routes()` function
5. Add test → `crates/aionui-{domain}/tests/` or `crates/aionui-app/tests/`

### Add Migration

1. Next number → `ls crates/aionui-db/migrations/`
2. Create `NNN_descriptive_name.sql` with `IF NOT EXISTS`

### Add WebSocket Event

1. Event type → `aionui-api-types`
2. Emit via `event_bus.broadcast()` in service
3. Naming: `domain.camelCaseAction`

## Test Organization

| Location                                 | What goes there                        |
| ---------------------------------------- | -------------------------------------- |
| Inline `#[cfg(test)]` in each `.rs` file | Unit tests for that module's internals |
| `crates/<crate>/tests/`                  | Integration / E2E tests for that crate |

### Testing Rules

- Database tests use `init_database_memory()`
- Prefer real in-memory DB over mocks; mock only to isolate unneeded dependencies
- New features must include tests

### Test Scope Requirements

**Happy Path (Critical Paths)**

Every new or modified feature must have integration tests covering its normal flow. Critical paths that always require test coverage:
- Authentication flow (login, token refresh, permission checks)
- Message sending and retrieval
- Agent session creation and interaction
- File upload/download
- WebSocket connection and event delivery

**Bad Path (Error Paths)**

New endpoints or business logic must include tests for these scenarios:
- Invalid input (missing fields, wrong types, oversized content)
- Resource not found (404)
- Insufficient permissions (unauthenticated, accessing another user's resources)
- Business rule violations (duplicate creation, operations not allowed in current state)

Bad path tests must assert specific error codes or error messages — asserting merely "not success" is not acceptable.

**Security Tests**

Endpoints involving authentication, authorization, or data isolation must include security tests:
- Unauthenticated requests are rejected (401)
- Cross-user data isolation (user A cannot access user B's resources)
- State-changing requests are rejected when CSRF token is missing or invalid
- Sensitive fields (passwords, tokens) never appear in responses

**WebSocket Event Tests**

New WebSocket events must verify:
- The event is emitted after the correct business operation
- Event payload conforms to `WebSocketMessage<T>` structure
- Events are only delivered to authorized subscribers (no leakage to unrelated users)

### Test Failure Handling

When a test fails, do NOT modify the test to make it pass. First determine:

1. **Test assertion still represents correct behavior** → fix implementation, not the test
2. **Requirements/interface intentionally changed** → may update test, but must confirm:
   - The change is intentional (not an unintended side effect)
   - New assertions still validate meaningful behavior
3. **Uncertain** → stop, trace back the change, clarify before proceeding

Prohibited:
- ❌ Deleting failing tests to "fix" the problem
- ❌ Weakening specific assertions to vague ones (e.g., `assert_eq!(status, 201)` → `assert!(status.is_success())`)

## Verification Strategy

> ⚠️ **When to run what:**
> - During development: only test the crate you're working on → `cargo test -p aionui-<crate>`
> - After implementation complete: full verification → `cargo test --workspace`
> - Do NOT run `cargo test --workspace` at the start of a task.
>
> ⚠️ **Performance:**
> - `cargo clippy --workspace` takes several minutes — use `run_in_background: true`.
> - `cargo test --workspace` takes 10+ minutes. MUST use `run_in_background: true` when calling via Bash tool, otherwise it will timeout.
> - `cargo clippy -p aionui-<crate>` and `cargo test -p aionui-<crate>` typically complete in under 1 minute.

### During Development (fast feedback loop)

```bash
cargo test -p aionui-<crate>                          # Test the crate you changed
cargo clippy -p aionui-<crate> -- -D warnings         # Lint the crate you changed
```

### Before Commit (affected crates)

```bash
cargo fmt --all -- --check                                                      # Format gate (instant)
cargo clippy -p aionui-<crate1> -p aionui-<crate2> -- -D warnings              # Lint affected crates
cargo test -p aionui-<crate1> -p aionui-<crate2>                               # Test affected crates
```

### Before Push (full workspace)

```bash
just push                                             # fmt → clippy → test → git push
```

<!-- ALFRED-CODING-TOOLS:START -->

## Canonical Coding Tools

Common agent coding tools are installed centrally on `laptop`, `server`, and `study` under:

`/Users/richard/Coding Tools`

Use `/Users/richard/Coding Tools/bin/<tool>` before repo-local installs, Homebrew paths, or ad hoc downloads. Each tool also has an owned subfolder at `/Users/richard/Coding Tools/tools/<tool>/bin/<tool>`, and the per-machine manifest is at:

`/Users/richard/Coding Tools/manifests/coding-tools-manifest.md`

Daily agent workbench paths:

| Category | Tools |
| --- | --- |
| File create/remove/edit | `apply_patch`, `mkdir`, `rmdir`, `rm`, `cp`, `mv`, `touch`, `ln`, `chmod`, `chown`, `chgrp`, `stat` |
| File/path inspection | `cat`, `ls`, `pwd`, `tree`, `bat`, `find`, `fd`, `realpath`, `basename`, `dirname`, `du`, `df`, `mktemp` |
| Search/replace/text | `rg`, `grep`, `egrep`, `fgrep`, `sed`, `sd`, `awk`, `xargs`, `sort`, `uniq`, `head`, `tail`, `wc`, `tee` |
| Diff/patch | `diff`, `patch`, `cmp`, `comm`, `diff3`, `sdiff` |
| Git/GitHub | `git`, `git-lfs`, `gh`, `ssh`, `gitignore` |
| Downloads/sync | `curl`, `wget`, `rsync` |
| Data/config | `jq`, `yq`, `plutil` |
| JavaScript/TypeScript | `node`, `npm`, `npx`, `pnpm`, `bun`, `bunx`, `prettier`, `eslint`, `prek` |
| Rust | `rustup`, `cargo`, `rustc`, `rustfmt`, `cargo-fmt`, `cargo-clippy`, `clippy-driver`, `rust-analyzer`, `cargo-nextest`, `cargo-audit` |
| Python/tool runners | `python`, `python3`, `uv`, `uvx` |
| Shell/tool quality | `shellcheck`, `shfmt`, `pwsh`, `sh`, `bash`, `zsh` |
| Archives/compression | `tar`, `zip`, `unzip`, `gzip`, `gunzip`, `bzip2`, `bunzip2` |
| Browser/app verification | `playwright` |
| Build/platform basics | `make`, `xcodebuild`, `swift`, `just`, `openssl`, `perl`, `ruby`, `codesign`, `security`, `xcrun`, `productbuild`, `hdiutil`, `ditto`, `lipo`, `otool` |

Important stable executable paths:

| Tool | Stable path |
| --- | --- |
| `apply_patch` | `/Users/richard/Coding Tools/bin/apply_patch` |
| `gitignore` | `/Users/richard/Coding Tools/bin/gitignore` |
| `git` | `/Users/richard/Coding Tools/bin/git` |
| `gh` | `/Users/richard/Coding Tools/bin/gh` |
| `git-lfs` | `/Users/richard/Coding Tools/bin/git-lfs` |
| `ssh` | `/Users/richard/Coding Tools/bin/ssh` |
| `rg` | `/Users/richard/Coding Tools/bin/rg` |
| `grep` | `/Users/richard/Coding Tools/bin/grep` |
| `find` | `/Users/richard/Coding Tools/bin/find` |
| `fd` | `/Users/richard/Coding Tools/bin/fd` |
| `sed` | `/Users/richard/Coding Tools/bin/sed` |
| `sd` | `/Users/richard/Coding Tools/bin/sd` |
| `awk` | `/Users/richard/Coding Tools/bin/awk` |
| `diff` | `/Users/richard/Coding Tools/bin/diff` |
| `patch` | `/Users/richard/Coding Tools/bin/patch` |
| `jq` | `/Users/richard/Coding Tools/bin/jq` |
| `yq` | `/Users/richard/Coding Tools/bin/yq` |
| `node` | `/Users/richard/Coding Tools/bin/node` |
| `npm` | `/Users/richard/Coding Tools/bin/npm` |
| `npx` | `/Users/richard/Coding Tools/bin/npx` |
| `pnpm` | `/Users/richard/Coding Tools/bin/pnpm` |
| `bun` | `/Users/richard/Coding Tools/bin/bun` |
| `bunx` | `/Users/richard/Coding Tools/bin/bunx` |
| `playwright` | `/Users/richard/Coding Tools/bin/playwright` |
| `prek` | `/Users/richard/Coding Tools/bin/prek` |
| `rustup` | `/Users/richard/Coding Tools/bin/rustup` |
| `cargo` | `/Users/richard/Coding Tools/bin/cargo` |
| `rustc` | `/Users/richard/Coding Tools/bin/rustc` |
| `rustfmt` | `/Users/richard/Coding Tools/bin/rustfmt` |
| `cargo-fmt` | `/Users/richard/Coding Tools/bin/cargo-fmt` |
| `cargo-clippy` | `/Users/richard/Coding Tools/bin/cargo-clippy` |
| `cargo-nextest` | `/Users/richard/Coding Tools/bin/cargo-nextest` |
| `cargo-audit` | `/Users/richard/Coding Tools/bin/cargo-audit` |
| `clippy-driver` | `/Users/richard/Coding Tools/bin/clippy-driver` |
| `rust-analyzer` | `/Users/richard/Coding Tools/bin/rust-analyzer` |
| `pwsh` | `/Users/richard/Coding Tools/bin/pwsh` |
| `python` | `/Users/richard/Coding Tools/bin/python` |
| `python3` | `/Users/richard/Coding Tools/bin/python3` |
| `codesign` | `/Users/richard/Coding Tools/bin/codesign` |
| `security` | `/Users/richard/Coding Tools/bin/security` |
| `xcrun` | `/Users/richard/Coding Tools/bin/xcrun` |
| `productbuild` | `/Users/richard/Coding Tools/bin/productbuild` |
| `hdiutil` | `/Users/richard/Coding Tools/bin/hdiutil` |
| `ditto` | `/Users/richard/Coding Tools/bin/ditto` |
| `lipo` | `/Users/richard/Coding Tools/bin/lipo` |
| `otool` | `/Users/richard/Coding Tools/bin/otool` |
| `prettier` | `/Users/richard/Coding Tools/bin/prettier` |
| `eslint` | `/Users/richard/Coding Tools/bin/eslint` |
| `shellcheck` | `/Users/richard/Coding Tools/bin/shellcheck` |
| `shfmt` | `/Users/richard/Coding Tools/bin/shfmt` |
| `tree` | `/Users/richard/Coding Tools/bin/tree` |
| `bat` | `/Users/richard/Coding Tools/bin/bat` |
| `curl` | `/Users/richard/Coding Tools/bin/curl` |
| `wget` | `/Users/richard/Coding Tools/bin/wget` |
| `rsync` | `/Users/richard/Coding Tools/bin/rsync` |

For any listed tool, the stable path is `/Users/richard/Coding Tools/bin/<tool>` and the owned subfolder path is `/Users/richard/Coding Tools/tools/<tool>/bin/<tool>`.

Specialized machine-level tool note: Ghidra is installed on `study` only. Use `/Users/richard/.local/bin/ghidra` for the GUI wrapper, `/Users/richard/.local/bin/ghidra-headless` for headless analysis, and `/Users/richard/.local/share/alfred-tools/ghidra/ghidra_12.1.2_PUBLIC` for the underlying install. Do not document or assume Ghidra on `server` or `laptop` unless it is installed there in a later maintenance slice.

<!-- ALFRED-CODING-TOOLS:END -->
