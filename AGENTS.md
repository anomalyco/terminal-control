# AGENTS.md

## Repository

- Terminal Control is a Rust library and its `termctrl` CLI binary. Public CLI vocabulary is `show` for reading visible terminal state, `save` for explicit retained artifacts, a named `session` for the live terminal lifecycle managed by flat control commands, `logs` for readable retained output, and `video` for a recorded timeline export.
- `packages/test` publishes the TypeScript client and Vitest adapter; `src/driver.rs` is their JSON Lines transport adapter. `src/mcp.rs` publishes the stdio MCP adapter over named sessions and workspaces.
- A `workspace` is a persistent daemon containing ordered named windows and workspace-wide pane IDs
  that remain stable for its lifetime. `src/workspace.rs` owns workspace state, layout, tab chrome,
  attachment presentation, and composed recording; `src/session.rs` owns the Unix-socket adapter and
  protocol. Keep process ownership single-threaded because Ghostty terminal objects are not `Send`
  or `Sync`.
- Keep `README.md`, `skills/terminal-control/SKILL.md`, `CONTEXT.md`, and the Clap help in `src/main.rs` aligned when changing commands, formats, sessions, workspaces, recording, or OpenTUI support. Deep-dive documentation lives in `docs/` (`rust-library.md`, `driver-protocol.md`, `typescript-client.md`, `releasing.md`); the README stays install- and example-first with short pointers.
- Treat the named-session control protocol as additive. Give new response fields Serde defaults, preserve existing request shapes, bump the relevant capability constant, and return an actionable restart error when an older workspace daemon cannot support a mutation.
- Keep MCP tool names and schemas aligned with the concise tool inventory in `README.md`. Keep driver wire changes aligned with `docs/driver-protocol.md` and TypeScript client changes aligned with `docs/typescript-client.md`.
- Prefer focused fixes with unit tests in the affected module.

## Validation

Run the CI checks before finishing code changes:

```bash
bun install --frozen-lockfile
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
bun run test:npm
bun run build:npm
bun run validate:npm
cargo package --list
cargo package
```

Source builds require Rust 1.93 or newer and Zig 0.15.2. On Homebrew systems where Zig 0.15 is
keg-only, prepend `/opt/homebrew/opt/zig@0.15/bin` to `PATH` for Rust builds.

## Releases

- Use Changesets for public npm changes. Keep the fixed platform packages, TypeScript client, Cargo package version, and lockfiles synchronized when preparing a release.
- Follow `docs/releasing.md`; validate assembled packages and a clean consumer before publishing through the `npm-release.yml` trusted-publishing workflow. Do not invoke `npm publish` directly.

## Artifacts

- Do not commit generated `.ansi`, `.json`, `.svg`, or `.txt` sidecars under `docs/screenshots/`; PNG documentation images remain commit-eligible.
- Treat `.termctrl` recordings and terminal artifacts as potentially sensitive because they may contain terminal output plus client or host input.
