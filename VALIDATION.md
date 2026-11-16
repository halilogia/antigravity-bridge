# Delivery validation — 2026-11-16

## Results

| Check | Result |
|---|---|
| `cargo test --all` | 82 passed, 0 failed; 2 opt-in Node/installed-archive tests ignored in the default run |
| `cargo test --lib patch::tests -- --include-ignored` | 6 passed, including both opt-in tests; 84 distinct tests passed across the two commands |
| `cargo clippy --all-targets` | Passed, no Clippy issues |
| `cargo fmt -- --check` | Passed |
| `cargo build --release` | Passed; MSVC emitted one informational library/export-file linker message as a Rust warning |
| `node --check src/ui/app.js` | Passed |
| `scripts/verify-release.ps1` | Passed against the release executable using an isolated local configuration and synthetic key |
| Release `patch --check` | Compatible with installed Antigravity Desktop 2.32.1; read-only |

Release artifact: `target/release/antigravity-bridge.exe`, **8,388,096 bytes**.

SHA-256:

```text
b0f17d93156b61321f6bbc2df4ac560eeb039a2475647e2dc51a634940a58028
```

## Evidence covered

- Existing translation, catalog, Google routing and admin tests remain passing.
- OpenCode tests use real HTTP/SSE connections to local mock servers. They cover Go/Zen paths, DPAPI-loaded keys, model discovery, plan-specific routing, max effort, exactly one retry for HTTP and SSE thinking-required errors, no retry after content, bounded failure behavior, conversation cache reuse/isolation, Responses/Messages text, tool repair, image/tool history and token usage.
- Premature stream termination is returned as an error instead of a successful completion.
- Patch fixture tests cover read-only planning, preservation of other ASAR members, repatch rejection, patch/restore byte equality, recovery before archive replacement, update refusal and backup-corruption refusal.
- The actual installed archive was read and patched only in memory/temporary files. Node accepted the resulting `languageServer.js` syntax.
- An isolated Node harness executed the injected hook: all three URL flags changed for a fresh live heartbeat; stale heartbeat, dead process, wrong URL and WSL retained original endpoints.
- The release smoke test verified HTTP health, served UI HTML, admin 401, cross-origin 403, model CRUD, DPAPI key source/encrypted file contents and heartbeat PID/URL. It stopped only its own temporary bridge process.

Inspected installation hashes:

```text
app.asar             5b493943e67e500638555315594d5b1acbc7a02e6dac3c786651921c1ee8519f
language_server.exe  65cdb8d132fc61056625081be7d07d4d22f57eba485de7edf0d24d98e77c891f
```

## Boundaries

Antigravity and its language server were running, so the real installation was not modified or restarted. `KUR.cmd` and `GERI_AL.cmd` are delivered for installation after saving work and closing those processes. Transaction tests bypass the process check only in Rust's unit-test build and operate exclusively on disposable fixtures; the production executable retains the process guard.

No live OpenCode completion, paid inference, real-account Google passthrough or visual Antigravity picker acceptance was performed. The local UI was checked for JavaScript syntax and HTTP delivery, not by a graphical browser test. Native Settings integration, WSL redirection, Google-native OpenCode inference and reasoning-signature replay are outside this implementation.

The repository had no commits and no configured remote at takeover. Source and delivery files are committed locally; build artifacts and disposable validation data stay under ignored `target/`. No remote push or release publication was possible without a configured destination.
