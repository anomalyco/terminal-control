# TypeScript Client

`@kitlangton/terminal-control` exposes the `termctrl driver` as isolated typed test sessions. The source repository is hosted at `anomalyco/terminal-control`; the npm packages intentionally remain under the `@kitlangton` scope for install continuity.

The npm distribution includes an optional native package for macOS or GNU/Linux on arm64 or x64, so consumers do not need a Rust toolchain or a separate `termctrl` installation.

```bash
bun add -d @kitlangton/terminal-control vitest
```

## Launch And Interact

```ts
import { TerminalControl } from "@kitlangton/terminal-control"

await using terminal = await TerminalControl.make({
  artifacts: {
    directory: ".termctrl-artifacts",
    onFailure: true,
    includeTranscript: false,
    includeRecording: true,
  },
})
await using session = await terminal.launch({
  command: ["/absolute/path/to/my-terminal-app"],
  viewport: { cols: 100, rows: 30 },
  inheritEnv: false,
  env: { TERM: "xterm-256color", HOME: "/tmp/test-home" },
  record: "on-failure",
})

await session.screen.waitForText(/Ready/)
await session.keyboard.type("help")
await session.keyboard.press("Enter")

const text = await session.screen.text()
const frame = await session.screen.frame()
const logs = await session.logs.text()
const transcript = await session.transcript.ansi()

expect(text).toMatchSnapshot()
expect(frame).toMatchSnapshot()

const exit = await session.waitForExit({ timeoutMs: 5_000 })
expect(exit).toMatchObject({ reason: "exited", exit: { code: 0 } })
```

When working directly from this repository before installing the npm artifacts, pass `binaryPath: "./target/release/termctrl"` or set `TERMCTRL_BINARY`.

## Stable Snapshots By Default

`session.screen.text()` and `session.screen.frame()` wait for a settled capture and reject deadline or output-closed fallback by default. A test that intentionally needs an intermediate frame can request it explicitly:

```ts
const capture = await session.screen.capture({ allowIncomplete: true })
console.log(capture.reason, capture.text, capture.frame)
```

## Keyboard Input

Keyboard presses are typed as the sequences Terminal Control encodes exactly, such as `"Enter"`, `"ArrowDown"`, or `"Control+C"`. Use `session.keyboard.write(bytes)` when a test deliberately needs exact terminal bytes outside that supported key set.

## Mouse Input

```ts
await session.mouse({ action: "move", x: 12, y: 4 }) // hover
await session.mouse({ action: "click", x: 12, y: 4 })
await session.mouse({ action: "down", x: 12, y: 4 })
await session.mouse({ action: "move", x: 20, y: 4 }) // drag
await session.mouse({ action: "up", x: 20, y: 4 })
```

The exported `MouseEvent` type accepts `button: "left" | "middle" | "right"` (default left)
and optional `shift`, `alt`, `ctrl` booleans. Positions are zero-based integer cells, validated
against the current viewport. The app must enable mouse reporting; hover requires any-event
tracking. A click is an immediate press/release. No visual-animation delays are added to tests.
An older binary without the driver `mouse` capability produces an actionable update error.

With recording enabled, typed mouse evidence can be visualized later with
`termctrl video recording.termctrl --pointer-overlay --out demo.mp4`.
Add `--pointer-reduced-motion` for fades without travel or press scaling. Screenshots stay clean.

## Vitest Matchers And Failure Evidence

Standard `toMatchSnapshot()` and `toMatchInlineSnapshot()` remain the simplest snapshot format because visible text is reviewable in source control. A screen-aware assertion writes configured artifacts on failure:

```ts
import { expect } from "vitest"
import { extendTerminalControlMatchers } from "@kitlangton/terminal-control/vitest"

extendTerminalControlMatchers(expect)

await expect(session).toHaveScreenText("Ready\n\nChoose an option")
await expect(session.screen.text()).resolves.toMatchInlineSnapshot()
```

`session.writeArtifacts(name)` and failing `toHaveScreenText(...)` assertions can write `screen.txt`, `screen.json`, `screen.svg`, `logs.txt`, and `metadata.json`. Environment variable values are redacted in metadata. `transcript.ansi` and `recording.termctrl` are opt-in because terminal streams and typed input may contain secrets.

`withArtifactsOnFailure` preserves the original failure if evidence capture itself fails, and
prints a warning for the secondary artifact error. Direct `writeArtifacts` calls still reject
when capture or writing fails.

Wrap ordinary snapshot assertions when evidence should be saved on any thrown assertion:

```ts
await session.withArtifactsOnFailure("settings-snapshot", async () => {
  await expect(session.screen.text()).resolves.toMatchSnapshot()
})
```

## Recordings And Resize

Enable a recording with `record: true` or `record: "on-failure"`; a test may explicitly save it before disposing the session:

```ts
await session.resize({ cols: 120, rows: 40 })
await session.saveRecording("artifacts/navigation.termctrl")
```
