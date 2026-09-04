import { afterAll, beforeAll, describe, expect, spyOn, test } from "bun:test"
import { mkdtemp, readFile, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"

import { IncompleteCaptureError, resolveTerminalControlBinary, Session, TerminalControl } from "./index"
import { terminalControlMatchers } from "./vitest"

const binaryPath = process.env.TERMCTRL_TEST_BINARY ?? resolve(import.meta.dir, "../../../target/debug/termctrl")
let terminal: TerminalControl

beforeAll(async () => {
  terminal = await TerminalControl.make({ binaryPath })
})

afterAll(async () => {
  await terminal.close()
})

describe("isolated terminal sessions", () => {
  test("failure artifacts cannot replace the original failure", async () => {
    const original = new Error("original assertion")
    const captureFailure = new Error("driver unavailable")
    let requests = 0
    const request = async () => { requests++; throw captureFailure }
    const warning = spyOn(console, "warn").mockImplementation(() => {})
    try {
      const session = new Session(request, "fixture", { directory: "unused" })
      expect(await session.withArtifactsOnFailure("success", async () => 7)).toBe(7)
      expect(requests).toBe(0)
      await expect(session.withArtifactsOnFailure("failure", async () => { throw original })).rejects.toBe(original)
      expect(requests).toBe(1)
      expect(warning).toHaveBeenCalledWith("Terminal Control could not write failure artifacts:", captureFailure)
      for (const artifacts of [false, { directory: "unused", onFailure: false }] as const) {
        await expect(new Session(request, "disabled", artifacts).withArtifactsOnFailure("failure", async () => { throw original })).rejects.toBe(original)
      }
      expect(requests).toBe(1)
    } finally {
      warning.mockRestore()
    }
  })
  test("delivers hover, click and drag input with recorded evidence and resized bounds", async () => {
    await using session = await terminal.launch({
      command: [process.execPath, resolve(import.meta.dir, "../fixtures/mouse.ts")],
      viewport: { cols: 50, rows: 16 },
      record: true,
    })
    await session.screen.waitForText("ready")
    await session.mouse({ action: "move", x: 4, y: 2 })
    await session.screen.waitForText("mouse:35:5:3:M")
    await session.mouse({ action: "click", x: 9, y: 4, button: "right", shift: true })
    await session.screen.waitForText("mouse:6:10:5:m")
    await session.mouse({ action: "down", x: 3, y: 2 })
    await session.mouse({ action: "move", x: 12, y: 3 })
    await session.screen.waitForText("mouse:32:13:4:M")
    await expect(session.mouse({ action: "click", x: 3, y: 2 })).rejects.toThrow("held")
    await expect(session.mouse({ action: "up", x: 3, y: 2, button: "right" })).rejects.toThrow("match")
    await session.mouse({ action: "up", x: 12, y: 3 })
    await session.screen.waitForText("mouse:0:13:4:m")
    await session.resize({ cols: 20, rows: 8 })
    await expect(session.mouse({ action: "move", x: 20, y: 2 })).rejects.toThrow("outside")
    await expect(session.mouse({ action: "move", x: -1, y: 2 })).rejects.toThrow()
    await expect(session.mouse({ action: "move", x: 1.5, y: 2 })).rejects.toThrow()
    await session.mouse({ action: "click", x: 19, y: 7 })
    const entries = new TextDecoder().decode(await session.recording()).trim().split("\n").map((line) => JSON.parse(line))
    expect(entries[0].version).toBe(2)
    const mouse = entries.filter((entry) => entry.type === "mouse")
    expect(mouse.map((entry) => entry.event.action)).toEqual(["move", "click", "down", "move", "up", "click"])
    expect(new TextDecoder().decode(Uint8Array.from(mouse[1].bytes))).toBe("\x1b[<6;10;5M\x1b[<6;10;5m")
    expect(mouse.every((entry, index) => entry.at_ms >= (mouse[index - 1]?.at_ms ?? 0))).toBe(true)
  })

  test("refuses mouse input when the application has not enabled reporting", async () => {
    await using session = await terminal.launch({ command: ["sh", "-c", "printf ready; sleep 2"] })
    await session.screen.waitForText("ready")
    await expect(session.mouse({ action: "click", x: 1, y: 1 })).rejects.toThrow("not enabled mouse reporting")
  })

  test("keeps legacy drivers usable but requests an upgrade for mouse input", async () => {
    const directory = await mkdtemp(join(tmpdir(), "termctrl-legacy-driver-"))
    const binaryPath = join(directory, "driver.js")
    await writeFile(binaryPath, `#!${process.execPath}
import { createInterface } from "node:readline"
console.log(JSON.stringify({ type: "hello", protocolVersion: 2, terminalControlVersion: "1.0.0" }))
createInterface({ input: process.stdin }).on("line", (line) => {
  const request = JSON.parse(line)
  if (request.method === "mouse") throw new Error("must not send unsupported mouse request")
  console.log(JSON.stringify({ type: "response", id: request.id, result: null }))
})
`, { mode: 0o700 })
    await using legacy = await TerminalControl.make({ binaryPath })
    await using session = await legacy.launch({ command: ["unused"] })
    await session.keyboard.type("still supported")
    await expect(session.mouse({ action: "click", x: 1, y: 1 })).rejects.toThrow("update the binary and restart the driver")
  })

  test("types only implemented key descriptions", () => {
    type ImplementedKey = Parameters<import("./index").Keyboard["press"]>[0]
    const valid: ImplementedKey[] = ["Enter", "ArrowDown", "Control+C"]
    // @ts-expect-error unsupported chords must not compile as terminal input.
    const invalid: ImplementedKey = "Meta+X"

    expect(valid).toHaveLength(3)
    void invalid
  })

  test("prefers an explicitly configured native binary", () => {
    expect(resolveTerminalControlBinary(binaryPath)).toBe(binaryPath)
  })

  test("drives a visible screen through typed keyboard operations", async () => {
    await using session = await terminal.launch({
      command: [
        "sh",
        "-c",
        "printf ready; IFS= read -r line; printf '\\r\\ngot:%s' \"$line\"; sleep 1",
      ],
      viewport: { cols: 20, rows: 4 },
    })

    await session.screen.waitForText("ready", { timeoutMs: 2_000 })
    await session.keyboard.type("hello")
    await session.keyboard.press("Enter")
    await session.screen.waitForText("got:hello", { timeoutMs: 2_000 })
    await session.resize({ cols: 30, rows: 5 })

    const capture = await session.screen.capture({ settleMs: 10, deadlineMs: 2_000 })
    expect(capture.frame.version).toBe(2)
    expect(capture.frame.cursor?.style).toBe("block")
    expect(capture.frame.cols).toBe(30)
    expect(capture.text).toMatchInlineSnapshot(`
      "readyhello

      got:hello"
    `)
    expect((await session.transcript.ansi()).byteLength).toBeGreaterThan(0)
  })

  test("exposes current screen separately from logs", async () => {
    await using session = await terminal.launch({
      command: ["sh", "-c", "printf 'one\\r\\ntwo\\r\\nthree\\r\\nfour\\r\\nfive\\r\\n'; sleep 1"],
      viewport: { cols: 20, rows: 2 },
    })

    await session.screen.waitForText("five", { timeoutMs: 2_000 })

    expect(await session.screen.text({ settleMs: 10, deadlineMs: 2_000 })).not.toContain("one")
    expect(await session.logs.text()).toContain("one")
  })

  test("refuses to treat a deadline capture as stable by default", async () => {
    await using session = await terminal.launch({
      command: ["sh", "-c", "while :; do printf x; sleep 0.01; done"],
    })

    await expect(
      session.screen.capture({ settleMs: 1_000, deadlineMs: 50 }),
    ).rejects.toBeInstanceOf(IncompleteCaptureError)

    const capture = await session.screen.capture({
      settleMs: 1_000,
      deadlineMs: 50,
      allowIncomplete: true,
    })
    expect(capture.reason).toBe("deadline")
  })

  test("reports a completed command status and exit code", async () => {
    await using session = await terminal.launch({ command: ["sh", "-c", "exit 7"] })
    const result = await session.waitForExit({ timeoutMs: 2_000 })
    const status = await session.status()

    expect(result).toMatchObject({ reason: "exited", exit: { code: 7 } })
    expect(status.state).toBe("exited")
    expect(status.exit?.code).toBe(7)
  })

  test("accepts implemented control chords as typed key strings", async () => {
    await using session = await terminal.launch({
      command: [
        "sh",
        "-c",
        "stty -echo -icanon -isig; printf ready; byte=$(dd bs=1 count=1 2>/dev/null | od -An -tu1); printf ' key:%s' \"$byte\"; sleep 1",
      ],
    })

    await session.screen.waitForText("ready", { timeoutMs: 2_000 })
    await session.keyboard.press("Control+C")
    await session.screen.waitForText("key:", { timeoutMs: 2_000 })
    expect(await session.screen.text({ settleMs: 10, deadlineMs: 2_000 })).toContain("3")
  })

  test("supports isolated application environments", async () => {
    await using isolated = await TerminalControl.make({
      binaryPath,
      env: { TERMCTRL_PARENT_ONLY: "leak" },
    })
    await using session = await isolated.launch({
      command: ["/bin/sh", "-c", "printf '%s:%s' \"${TERMCTRL_PARENT_ONLY-unset}\" \"$VISIBLE\""],
      inheritEnv: false,
      env: { VISIBLE: "set" },
    })

    expect(await session.screen.text({ settleMs: 10, deadlineMs: 2_000 })).toBe("unset:set")
  })

  test("waits for regular expressions and client predicates", async () => {
    await using session = await terminal.launch({
      command: ["sh", "-c", "printf 'saved 12 files'; sleep 1"],
    })

    await session.screen.waitForText(/saved \d+ files/, { timeoutMs: 2_000 })
    const snapshot = await session.screen.waitUntil((screen) => screen.text.endsWith("files"))
    expect(snapshot.text).toBe("saved 12 files")
  })

  test("records resized sessions and saves their timelines", async () => {
    const directory = await mkdtemp(join(tmpdir(), "termctrl-recording-test-"))
    const path = join(directory, "resize.termctrl")
    await using session = await terminal.launch({
      command: ["sh", "-c", "printf ready; sleep 1"],
      record: true,
    })
    await session.screen.waitForText("ready", { timeoutMs: 2_000 })
    await session.resize({ cols: 100, rows: 32 })
    await session.saveRecording(path)

    const recording = await readFile(path, "utf8")
    expect(recording).toContain('"type":"resize"')
  })

  test("writes explicit failure artifacts with snapshot and recording evidence", async () => {
    const directory = await mkdtemp(join(tmpdir(), "termctrl-artifacts-test-"))
    await using reporting = await TerminalControl.make({
      binaryPath,
      artifacts: { directory, includeTranscript: true, includeRecording: true },
    })
    await using session = await reporting.launch({
      command: ["sh", "-c", "printf evidence; sleep 1"],
      record: "on-failure",
      env: { API_TOKEN: "sensitive" },
    })
    await session.screen.waitForText("evidence", { timeoutMs: 2_000 })

    const result = await terminalControlMatchers.toHaveScreenText(session, "different")
    expect(result.pass).toBe(false)
    expect(await readFile(join(directory, "screen-text", "screen.txt"), "utf8")).toBe("evidence")
    expect((await readFile(join(directory, "screen-text", "screen.svg"), "utf8")).startsWith("<svg")).toBe(true)
    expect(await readFile(join(directory, "screen-text", "transcript.ansi"), "utf8")).toContain("evidence")
    expect(await readFile(join(directory, "screen-text", "recording.termctrl"), "utf8")).toContain("output")
    const metadata = await readFile(join(directory, "screen-text", "metadata.json"), "utf8")
    expect(metadata).toContain('"API_TOKEN": "[redacted]"')
    expect(metadata).not.toContain("sensitive")
    const original = new Error("original assertion")
    await expect(session.withArtifactsOnFailure("original", async () => { throw original })).rejects.toBe(original)
    expect(await readFile(join(directory, "original", "screen.txt"), "utf8")).toBe("evidence")
  })

  test("drives an alternate-screen terminal workflow and snapshots its selected view", async () => {
    await using session = await terminal.launch({
      command: [
        "bash",
        "-c",
        "stty -echo -icanon; printf '\\033[?1049h\\033[2J\\033[HMenu\\r\\n> First\\r\\n  Second'; IFS= read -r -n 3 key; printf '\\033[2J\\033[HMenu\\r\\n  First\\r\\n> Second'; IFS= read -r -n 1 key; printf '\\033[2J\\033[HSelected: Second'; sleep 1",
      ],
      viewport: { cols: 24, rows: 5 },
    })

    await session.screen.waitForText("First", { timeoutMs: 2_000 })
    await session.keyboard.sequence(["ArrowDown", "Enter"])
    await session.screen.waitForText("Selected: Second", { timeoutMs: 2_000 })

    expect(await session.screen.text({ settleMs: 10, deadlineMs: 2_000 })).toMatchInlineSnapshot(`"Selected: Second"`)
  })
})
