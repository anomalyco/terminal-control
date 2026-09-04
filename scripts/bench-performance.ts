// Release-binary, real-PTY benchmarks. No arbitrary sleep/readiness guesses.
import { mkdtemp, mkdir, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"
import { parseArgs } from "node:util"
import { TerminalControl } from "../packages/test/src/index"

const screen = "\x1b[?25l\x1b[2J" + Array.from({ length: 22 }, (_, row) =>
  `\x1b[${row + 1};1H\x1b[38;5;${110 + row}m` + `Row ${row}: terminal rendering benchmark — 界 ━ `.padEnd(76, ".")
).join("") + "\x1b[0m\x1b[24;1HREADY"

if (process.argv.includes("--fixture")) {
  process.stdin.setRawMode(true)
  let pending = ""
  process.stdin.on("data", (data) => {
    pending += data.toString().replaceAll("\r", "\n")
    for (let end; (end = pending.indexOf("\n")) >= 0;) {
      const value = pending.slice(0, end)
      pending = pending.slice(end + 1)
      const acknowledge = () => process.stdout.write(`\x1b[24;1H\x1b[2KACK:${value}`)
      // A known 1ms application response exposes latency in a wait already in flight.
      if (process.argv.includes("--async-output")) setTimeout(acknowledge, 1)
      else acknowledge()
    }
  })
  process.stdout.write(screen)
} else {
  const { values } = parseArgs({ options: {
    binary: { type: "string", default: "target/release/termctrl" },
    mode: { type: "string", default: "all" },
    runs: { type: "string", default: "7" },
    "out-dir": { type: "string" },
  } })
  const runs = Number(values.runs)
  if (!Number.isInteger(runs) || runs < 1) throw new Error("runs must be a positive integer")
  if (!["all", "agent", "video", "replay"].includes(values.mode!)) throw new Error("mode must be all, agent, video, or replay")
  const binary = resolve(values.binary!)
  const temp = await mkdtemp(join(tmpdir(), "tp-"))
  const output = values["out-dir"] ? resolve(values["out-dir"]) : temp
  await mkdir(output, { recursive: true })
  const env = { ...process.env, TERMCTRL_RUNTIME_DIR: temp }
  const fixture = [process.execPath, import.meta.path, "--fixture"] as const
  const median = (numbers: number[]) => [...numbers].sort((a, b) => a - b)[Math.floor(numbers.length / 2)]!
  async function measure(name: string, run: (iteration: number) => Promise<Record<string, number>>) {
    const results = []
    for (let i = -1; i < runs; i++) {
      const result = await run(i)
      if (i >= 0) results.push(result)
    }
    for (const metric of Object.keys(results[0]!)) {
      const samples = results.map((result) => result[metric]!)
      const value = median(samples)
      console.log(JSON.stringify({ name: `${name}.${metric}`, median: value,
        mad: median(samples.map((sample) => Math.abs(sample - value))), samples }))
    }
  }
  async function cli(args: string[]) {
    const process = Bun.spawn([binary, ...args], { env, stdout: "pipe", stderr: "pipe" })
    const [text, error, code] = await Promise.all([
      new Response(process.stdout).text(), new Response(process.stderr).text(), process.exited,
    ])
    if (code !== 0) throw new Error(error)
    return text
  }
  try {
    const bytes = (text: string) => Array.from(new TextEncoder().encode(text))
    if (values.mode === "all" || values.mode === "agent") {
      await cli(["start", "p", "--", ...fixture])
      try {
        await cli(["wait", "p", "READY"])
        await measure("cli_show", async () => {
          const start = performance.now()
          if (!(await cli(["show", "p"])).includes("READY")) throw new Error("missing ready screen")
          return { ms: performance.now() - start }
        })
        await measure("cli_interact", async (i) => {
          const start = performance.now()
          await cli(["send", "p", `text:${i}`, "enter"])
          await cli(["wait", "p", `ACK:${i}`])
          if (!(await cli(["show", "p"])).includes(`ACK:${i}`)) throw new Error("stale screen")
          return { ms: performance.now() - start }
        })
      } finally { await cli(["stop", "p"]) }
      await using driver = await TerminalControl.make({ binaryPath: binary })
      await using session = await driver.launch({ command: fixture })
      await session.screen.waitForText("READY")
      await measure("driver_interact", async (i) => {
        const start = performance.now()
        await session.keyboard.type(`${i}\n`)
        await session.screen.waitForText(`ACK:${i}`)
        const snapshot = await session.screen.capture({ settleMs: 0, deadlineMs: 0 })
        if (!snapshot.text.includes(`ACK:${i}`)) throw new Error("stale screen")
        return { ms: performance.now() - start }
      })
      await using reactive = await driver.launch({ command: [...fixture, "--async-output"] })
      await reactive.screen.waitForText("READY")
      await measure("driver_reactive_wait", async (i) => {
        const start = performance.now()
        await reactive.keyboard.type(`${i}\n`)
        await reactive.screen.waitForText(`ACK:${i}`)
        return { ms: performance.now() - start }
      })
    }
    if (values.mode === "all" || values.mode === "replay") {
      const entries = [
        { type: "header", version: 2, cols: 80, rows: 24, cell_width: 9, cell_height: 18 },
        { type: "output", at_ms: 0, bytes: bytes(screen) },
        ...Array.from({ length: 1000 }, (_, i) => ({ type: "output", at_ms: i + 1,
          bytes: bytes(`\x1b[24;1Hframe:${String(i).padStart(4, "0")}`) })),
      ]
      const recording = join(temp, "replay.termctrl")
      await Bun.write(recording, entries.map((entry) => JSON.stringify(entry)).join("\n") + "\n")
      await measure("recording_final_screen", async () => {
        const start = performance.now()
        if (!(await cli(["show", "--recording", recording])).includes("frame:0999")) throw new Error("incorrect replay")
        return { ms: performance.now() - start }
      })
    }
    if (values.mode === "all" || values.mode === "video") {
      const entries: object[] = [
        { type: "header", version: 2, cols: 80, rows: 24, cell_width: 9, cell_height: 18 },
        { type: "output", at_ms: 0, bytes: bytes(screen) },
      ]
      for (let i = 1; i <= 10; i++) entries.push({ type: "mouse", at_ms: i * 200,
        event: { action: i % 3 === 0 ? "click" : "move", x: i * 6, y: i % 18 }, bytes: [] })
      entries.push({ type: "output", at_ms: 2000, bytes: bytes("\x1b[24;1HDONE") })
      const recording = join(temp, "fixture.termctrl")
      await Bun.write(recording, entries.map((entry) => JSON.stringify(entry)).join("\n") + "\n")
      for (const [name, flags] of [
        ["video_plain", []], ["video_pointer", ["--pointer-overlay"]],
        ["video_pointer_footer", ["--pointer-overlay", "--footer"]],
      ] as const) {
        await measure(name, async () => {
          const start = performance.now()
          let renderStart = start, encodeStart = start
          const child = Bun.spawn([binary, "video", recording, "--fps", "60", "--tail-ms", "0",
            "--hide-cursor", ...flags, "--out", join(output, `${name}.mp4`)], { env, stdout: "ignore", stderr: "pipe" })
          let error = ""
          for await (const chunk of child.stderr) {
            const line = new TextDecoder().decode(chunk)
            error += line
            if (line.includes("Rendering ")) renderStart = performance.now()
            if (line.includes("Encoding ")) encodeStart = performance.now()
          }
          if (await child.exited !== 0) throw new Error(error)
          return { total_ms: performance.now() - start, render_ms: encodeStart - renderStart }
        })
      }
    }
  } finally { await rm(temp, { recursive: true, force: true }) }
}
