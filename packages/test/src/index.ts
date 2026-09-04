import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process"
import { randomUUID } from "node:crypto"
import { createRequire } from "node:module"
import { mkdir, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { dirname, join } from "node:path"
import { createInterface } from "node:readline"

export type Color = {
  r: number
  g: number
  b: number
}

export type Attributes = {
  bold: boolean
  italic: boolean
  faint: boolean
  invisible: boolean
  strikethrough: boolean
  overline: boolean
  underline: "single" | null
}

export type Cell = {
  x: number
  y: number
  text: string
  width: number
  foreground: Color
  background: Color
  attributes: Attributes
}

export type Cursor = {
  x: number
  y: number
  color: Color
  blinking: boolean
  style: "block" | "hollow" | "underline" | "bar"
}

export type Frame = {
  version: 2
  cols: number
  rows: number
  foreground: Color
  background: Color
  cursor: Cursor | null
  cells: Cell[]
}

export type ScreenSnapshot = {
  reason: CaptureReason
  frame: Frame
  text: string
  ansi?: Uint8Array
  svg?: string
}

export type CaptureReason = "idle" | "deadline" | "exited" | "outputclosed"

export type CaptureResult = ScreenSnapshot

export type ProcessExit = {
  code: number
  signal: string | null
  success: boolean
}

export type SessionStatus = {
  state: "running" | "exited"
  exit: ProcessExit | null
  cols: number
  rows: number
  cellWidth: number
  cellHeight: number
  idleForMs: number | null
  hasVisibleContent: boolean
  recording: boolean
  logsTruncated: boolean
}

type WireShot = { frame: Frame; text: string; ansi: number[]; svg?: string }

type WireCaptureResult = { reason: CaptureReason; shot: WireShot }

type WireSessionStatus = {
  state: "running" | "exited"
  exit: ProcessExit | null
  cols: number
  rows: number
  cell_width: number
  cell_height: number
  idle_for_ms: number | null
  has_visible_content: boolean
  recording: boolean
  logs_truncated: boolean
}

export type ControlLetter =
  | "a" | "b" | "c" | "d" | "e" | "f" | "g" | "h" | "i"
  | "j" | "k" | "l" | "m" | "n" | "o" | "p" | "q" | "r"
  | "s" | "t" | "u" | "v" | "w" | "x" | "y" | "z"

export type Key =
  | "Enter"
  | "Escape"
  | "ArrowUp"
  | "ArrowDown"
  | "ArrowLeft"
  | "ArrowRight"
  | "Tab"
  | "Shift+Tab"
  | "Backspace"
  | "Delete"
  | "Home"
  | "End"
  | "PageUp"
  | "PageDown"
  | `Control+${Uppercase<ControlLetter>}`

export type Viewport = {
  cols: number
  rows: number
  cellWidth?: number
  cellHeight?: number
}

/** Real mouse input in zero-based cells. Move hovers; down/move/up drags. */
export type MouseEvent = {
  action: "move" | "down" | "up" | "click"
  x: number
  y: number
  button?: "left" | "middle" | "right"
  shift?: boolean
  alt?: boolean
  ctrl?: boolean
}

export type LaunchOptions = {
  command: readonly [string, ...string[]]
  cwd?: string
  record?: string | true | "on-failure"
  viewport?: Viewport
  host?: "opentui"
  color?: "auto" | "always" | "never"
  maxBytes?: number
  env?: Readonly<Record<string, string>>
  inheritEnv?: boolean
}

export type ArtifactOptions = {
  directory: string
  onFailure?: boolean
  includeTranscript?: boolean
  includeRecording?: boolean
}

export type DriverOptions = {
  binaryPath?: string
  cwd?: string
  env?: Record<string, string | undefined>
  artifacts?: false | ArtifactOptions
}

export type WaitOptions = {
  timeoutMs?: number
}

export type IdleOptions = WaitOptions & {
  quietForMs?: number
}

export type CaptureOptions = {
  settleMs?: number
  deadlineMs?: number
  allowIncomplete?: boolean
  includeAnsi?: boolean
  includeSvg?: boolean
}

export type StableCaptureOptions = Omit<CaptureOptions, "allowIncomplete">

export type WaitForExitResult =
  | { reason: "exited"; exit: ProcessExit }
  | { reason: "deadline" }

export type ArtifactManifest = {
  directory: string
  screenText: string
  screenFrame: string
  screenSvg: string
  metadata: string
  logsText: string
  transcript?: string
  recording?: string
}

type ProtocolKey =
  | "enter"
  | "escape"
  | "arrowUp"
  | "arrowDown"
  | "arrowLeft"
  | "arrowRight"
  | "tab"
  | "shiftTab"
  | "backspace"
  | "delete"
  | "home"
  | "end"
  | "pageUp"
  | "pageDown"

type InputAtom =
  | { type: "text"; value: string }
  | { type: "key"; value: ProtocolKey }
  | { type: "control"; value: ControlLetter }
  | { type: "bytes"; value: number[] }

type ProtocolHello = {
  type: "hello"
  protocolVersion: number
  terminalControlVersion: string
  capabilities?: string[]
}

type ProtocolResponse = {
  type: "response"
  id: number
  result: unknown
}

type ProtocolFailure = {
  type: "error"
  id: number | null
  error: {
    code: string
    message: string
  }
}

type Pending = {
  resolve: (value: unknown) => void
  reject: (error: Error) => void
}

type DriverRequest = <T>(method: string, params?: unknown, sessionId?: string) => Promise<T>

export class TerminalControlError extends Error {
  readonly code: string

  constructor(code: string, message: string) {
    super(message)
    this.name = "TerminalControlError"
    this.code = code
  }
}

export class IncompleteCaptureError extends Error {
  readonly capture: CaptureResult

  constructor(capture: CaptureResult) {
    super(`screen capture completed with ${capture.reason} instead of a settled frame`)
    this.name = "IncompleteCaptureError"
    this.capture = capture
  }
}

const nativePackages: Record<string, string> = {
  "darwin-arm64": "@kitlangton/terminal-control-darwin-arm64",
  "darwin-x64": "@kitlangton/terminal-control-darwin-x64",
  "linux-arm64": "@kitlangton/terminal-control-linux-arm64-gnu",
  "linux-x64": "@kitlangton/terminal-control-linux-x64-gnu",
}

export function resolveTerminalControlBinary(explicit?: string): string {
  if (explicit) return explicit
  if (process.env.TERMCTRL_BINARY) return process.env.TERMCTRL_BINARY
  const target = `${process.platform}-${process.arch}`
  const packageName = nativePackages[target]
  if (!packageName) {
    throw new Error(`no packaged termctrl binary is available for ${target}; provide binaryPath`)
  }
  try {
    return createRequire(import.meta.url).resolve(`${packageName}/bin/termctrl`)
  } catch {
    throw new Error(`termctrl native package ${packageName} is not installed; provide binaryPath`)
  }
}

export class TerminalControl implements AsyncDisposable {
  private readonly child: ChildProcessWithoutNullStreams
  private readonly pending = new Map<number, Pending>()
  private readonly ready: Promise<ProtocolHello>
  private nextRequestId = 1
  private nextSessionId = 1
  private closed = false
  private stderr = ""
  private readySettled = false
  private rejectReady: (error: Error) => void = () => {}
  private readonly artifacts: false | ArtifactOptions

  private constructor(options: DriverOptions) {
    this.artifacts = options.artifacts ?? false
    this.child = spawn(resolveTerminalControlBinary(options.binaryPath), ["driver"], {
      cwd: options.cwd,
      env: { ...process.env, ...options.env },
      stdio: "pipe",
    })
    this.ready = new Promise<ProtocolHello>((resolve, reject) => {
      this.rejectReady = reject
      const lines = createInterface({ input: this.child.stdout })
      lines.on("line", (line) => {
        let message: ProtocolHello | ProtocolResponse | ProtocolFailure
        try {
          message = JSON.parse(line) as ProtocolHello | ProtocolResponse | ProtocolFailure
        } catch (error) {
          this.abort(new Error(`invalid termctrl driver response: ${String(error)}`))
          return
        }
        if (message.type === "hello") {
          if (this.readySettled) {
            this.abort(new Error("termctrl driver sent more than one hello message"))
            return
          }
          if (message.protocolVersion !== 2) {
            this.abort(new Error(`unsupported termctrl protocol version ${message.protocolVersion}`))
            return
          }
          this.readySettled = true
          resolve(message)
          return
        }
        if (message.type === "error") {
          if (message.id === null) {
            this.abort(new TerminalControlError(message.error.code, message.error.message))
            return
          }
          const pending = this.pending.get(message.id)
          if (!pending) return
          this.pending.delete(message.id)
          pending.reject(new TerminalControlError(message.error.code, message.error.message))
          return
        }
        const pending = this.pending.get(message.id)
        if (!pending) return
        this.pending.delete(message.id)
        pending.resolve(message.result)
      })
      this.child.once("error", (error) => this.abort(error))
    })
    this.child.stderr.on("data", (chunk: Uint8Array) => {
      this.stderr = (this.stderr + new TextDecoder().decode(chunk)).slice(-8_192)
    })
    this.child.once("exit", (code, signal) => {
      if (this.closed) return
      const detail = this.stderr.trim()
      const error = new Error(
        `termctrl driver exited (${signal ?? code ?? "unknown"})${detail ? `: ${detail}` : ""}`,
      )
      this.abort(error)
    })
  }

  static async make(options: DriverOptions = {}): Promise<TerminalControl> {
    const client = new TerminalControl(options)
    await client.ready
    return client
  }

  async launch(options: LaunchOptions): Promise<Session> {
    const sessionId = `session-${this.nextSessionId++}`
    const temporaryRecording = options.record === true || options.record === "on-failure"
      ? join(tmpdir(), `termctrl-${process.pid}-${randomUUID()}.termctrl`)
      : undefined
    const record = temporaryRecording ?? options.record
    await this.request("launch", {
      command: [...options.command],
      cwd: options.cwd,
      record,
      cols: options.viewport?.cols,
      rows: options.viewport?.rows,
      cellWidth: options.viewport?.cellWidth,
      cellHeight: options.viewport?.cellHeight,
      host: options.host,
      color: options.color,
      maxBytes: options.maxBytes,
      env: options.env,
      inheritEnv: options.inheritEnv,
    }, sessionId)
    return new Session(
      async (method, params, id) => {
        if (method === "mouse" && !(await this.ready).capabilities?.includes("mouse")) {
          throw new Error("termctrl driver predates mouse input; update the binary and restart the driver")
        }
        return this.request(method, params, id)
      },
      sessionId,
      this.artifacts,
      temporaryRecording,
      options,
    )
  }

  async close(): Promise<void> {
    if (this.closed) return
    await this.request("shutdown")
    this.child.stdin.end()
    this.closed = true
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.close()
  }

  private async request<T>(method: string, params?: unknown, sessionId?: string): Promise<T> {
    await this.ready
    if (this.closed) throw new Error("termctrl driver is closed")
    const id = this.nextRequestId++
    const result = new Promise<unknown>((resolve, reject) => {
      this.pending.set(id, { resolve, reject })
    })
    const request = JSON.stringify({ id, method, sessionId, ...(params === undefined ? {} : { params }) })
    this.child.stdin.write(`${request}\n`, (error) => {
      if (!error) return
      const pending = this.pending.get(id)
      this.pending.delete(id)
      pending?.reject(error)
      this.abort(error)
    })
    return result as Promise<T>
  }

  private failAll(error: Error): void {
    for (const pending of this.pending.values()) pending.reject(error)
    this.pending.clear()
  }

  private abort(error: Error): void {
    if (this.closed) return
    this.closed = true
    if (!this.readySettled) {
      this.readySettled = true
      this.rejectReady(error)
    }
    this.failAll(error)
    this.child.stdin.destroy()
    this.child.kill()
  }
}

export class Session implements AsyncDisposable {
  readonly screen: Screen
  readonly keyboard: Keyboard
  readonly logs: Logs
  readonly transcript: Transcript
  private stopped = false

  constructor(
    private readonly request: DriverRequest,
    readonly id: string,
    private readonly artifacts: false | ArtifactOptions = false,
    private readonly temporaryRecording?: string,
    private readonly launchOptions?: LaunchOptions,
  ) {
    this.screen = new Screen(request, id)
    this.keyboard = new Keyboard(request, id)
    this.logs = new Logs(request, id)
    this.transcript = new Transcript(request, id)
  }

  status(): Promise<SessionStatus> {
    return this.request<WireSessionStatus>("status", undefined, this.id).then((status) => ({
      state: status.state,
      exit: status.exit,
      cols: status.cols,
      rows: status.rows,
      cellWidth: status.cell_width,
      cellHeight: status.cell_height,
      idleForMs: status.idle_for_ms,
      hasVisibleContent: status.has_visible_content,
      recording: status.recording,
      logsTruncated: status.logs_truncated,
    }))
  }

  waitForExit(options: WaitOptions = {}): Promise<WaitForExitResult> {
    return this.request("waitForExit", { timeoutMs: options.timeoutMs }, this.id)
  }

  resize(viewport: Viewport): Promise<void> {
    return this.request("resize", {
      cols: viewport.cols,
      rows: viewport.rows,
      cellWidth: viewport.cellWidth,
      cellHeight: viewport.cellHeight,
    }, this.id)
  }

  mouse(event: MouseEvent): Promise<void> {
    return this.request("mouse", event, this.id)
  }

  async recording(): Promise<Uint8Array> {
    const result = await this.request<{ bytes: number[] }>("recording", undefined, this.id)
    return Uint8Array.from(result.bytes)
  }

  async saveRecording(path: string): Promise<void> {
    await mkdir(dirname(path), { recursive: true })
    await writeFile(path, await this.recording())
  }

  shouldWriteFailureArtifacts(): boolean {
    return this.artifacts !== false && this.artifacts.onFailure !== false
  }

  async writeArtifacts(name = this.id): Promise<ArtifactManifest> {
    if (!this.artifacts) throw new Error("configure Terminal Control artifacts before writing failure evidence")
    const directory = join(this.artifacts.directory, artifactName(name))
    const includeTranscript = this.artifacts.includeTranscript === true
    const capture = await this.screen.capture({
      allowIncomplete: true,
      includeAnsi: includeTranscript,
      includeSvg: true,
      deadlineMs: 250,
    })
    const [logs, status] = await Promise.all([this.logs.text(), this.status()])
    await mkdir(directory, { recursive: true })
    const manifest: ArtifactManifest = {
      directory,
      screenText: join(directory, "screen.txt"),
      screenFrame: join(directory, "screen.json"),
      screenSvg: join(directory, "screen.svg"),
      metadata: join(directory, "metadata.json"),
      logsText: join(directory, "logs.txt"),
    }
    await Promise.all([
      writeFile(manifest.screenText, capture.text),
      writeFile(manifest.screenFrame, JSON.stringify(capture.frame, null, 2)),
      writeFile(manifest.screenSvg, capture.svg ?? ""),
      writeFile(manifest.logsText, logs),
      writeFile(manifest.metadata, JSON.stringify({
        sessionId: this.id,
        captureReason: capture.reason,
        launch: artifactLaunchOptions(this.launchOptions),
        status,
      }, null, 2)),
    ])
    if (includeTranscript && capture.ansi) {
      manifest.transcript = join(directory, "transcript.ansi")
      await writeFile(manifest.transcript, capture.ansi)
    }
    if (this.artifacts.includeRecording && status.recording) {
      manifest.recording = join(directory, "recording.termctrl")
      await writeFile(manifest.recording, await this.recording())
    }
    return manifest
  }

  async withArtifactsOnFailure<T>(name: string, run: () => Promise<T>): Promise<T> {
    try {
      return await run()
    } catch (error) {
      if (this.shouldWriteFailureArtifacts()) {
        try {
          await this.writeArtifacts(name)
        } catch (artifactError) {
          console.warn("Terminal Control could not write failure artifacts:", artifactError)
        }
      }
      throw error
    }
  }

  async stop(): Promise<void> {
    if (this.stopped) return
    await this.request("stop", undefined, this.id)
    this.stopped = true
    if (this.temporaryRecording) await rm(this.temporaryRecording, { force: true })
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.stop()
  }
}

export class Screen {
  constructor(private readonly request: DriverRequest, private readonly sessionId: string) {}

  waitForText(text: string | RegExp, options: WaitOptions = {}): Promise<void> {
    if (typeof text === "string") {
      return this.request("waitForText", { text, timeoutMs: options.timeoutMs }, this.sessionId)
    }
    return this.waitUntil((snapshot) => {
      text.lastIndex = 0
      return text.test(snapshot.text)
    }, options).then(() => undefined)
  }

  waitForIdle(options: IdleOptions = {}): Promise<void> {
    return this.request("waitForIdle", {
      quietForMs: options.quietForMs,
      timeoutMs: options.timeoutMs,
    }, this.sessionId)
  }

  async waitUntil(
    predicate: (snapshot: ScreenSnapshot) => boolean | Promise<boolean>,
    options: WaitOptions = {},
  ): Promise<ScreenSnapshot> {
    const timeoutMs = options.timeoutMs ?? 5_000
    const deadline = Date.now() + timeoutMs
    while (true) {
      const snapshot = await this.capture({ allowIncomplete: true, settleMs: 0, deadlineMs: 0 })
      if (await predicate(snapshot)) return snapshot
      if (Date.now() >= deadline) throw new Error("timed out waiting for visible terminal predicate")
      await new Promise((resolve) => setTimeout(resolve, 10))
    }
  }

  async capture(options: CaptureOptions = {}): Promise<ScreenSnapshot> {
    const wire = await this.request<WireCaptureResult>("capture", {
      settleMs: options.settleMs,
      deadlineMs: options.deadlineMs,
      includeAnsi: options.includeAnsi === true,
      includeSvg: options.includeSvg === true,
    }, this.sessionId)
    const capture: ScreenSnapshot = {
      reason: wire.reason,
      frame: wire.shot.frame,
      text: wire.shot.text,
      ...(options.includeAnsi ? { ansi: Uint8Array.from(wire.shot.ansi) } : {}),
      ...(wire.shot.svg ? { svg: wire.shot.svg } : {}),
    }
    if (!options.allowIncomplete && (capture.reason === "deadline" || capture.reason === "outputclosed")) {
      throw new IncompleteCaptureError(capture)
    }
    return capture
  }

  async frame(options: StableCaptureOptions = {}): Promise<Frame> {
    return (await this.capture(options)).frame
  }

  async text(options: StableCaptureOptions = {}): Promise<string> {
    return (await this.capture(options)).text
  }
}

export class Keyboard {
  constructor(private readonly request: DriverRequest, private readonly sessionId: string) {}

  type(value: string, options: { paceMs?: number } = {}): Promise<void> {
    return this.send([{ type: "text", value }], options.paceMs)
  }

  press(key: Key): Promise<void> {
    return this.send([keyAtom(key)])
  }

  sequence(keys: readonly Key[], options: { paceMs?: number } = {}): Promise<void> {
    return this.send(keys.map(keyAtom), options.paceMs)
  }

  write(value: Uint8Array): Promise<void> {
    return this.send([{ type: "bytes", value: Array.from(value) }])
  }

  private send(input: InputAtom[], paceMs = 0): Promise<void> {
    return this.request("send", { input, paceMs }, this.sessionId)
  }
}

export class Logs {
  constructor(private readonly request: DriverRequest, private readonly sessionId: string) {}

  async text(): Promise<string> {
    const result = await this.request<{ bytes: number[] }>("logs", { ansi: false }, this.sessionId)
    return new TextDecoder().decode(Uint8Array.from(result.bytes))
  }
}

export class Transcript {
  constructor(private readonly request: DriverRequest, private readonly sessionId: string) {}

  async ansi(): Promise<Uint8Array> {
    const result = await this.request<{ bytes: number[] }>("logs", { ansi: true }, this.sessionId)
    return Uint8Array.from(result.bytes)
  }
}

const namedKeys = {
  Enter: "enter",
  Escape: "escape",
  ArrowUp: "arrowUp",
  ArrowDown: "arrowDown",
  ArrowLeft: "arrowLeft",
  ArrowRight: "arrowRight",
  Tab: "tab",
  "Shift+Tab": "shiftTab",
  Backspace: "backspace",
  Delete: "delete",
  Home: "home",
  End: "end",
  PageUp: "pageUp",
  PageDown: "pageDown",
} as const satisfies Record<Exclude<Key, `Control+${string}`>, ProtocolKey>

function keyAtom(key: Key): InputAtom {
  if (isControlKey(key)) {
    return { type: "control", value: key.slice("Control+".length).toLowerCase() as ControlLetter }
  }
  return { type: "key", value: namedKeys[key] }
}

function isControlKey(key: Key): key is `Control+${Uppercase<ControlLetter>}` {
  return key.startsWith("Control+")
}

function artifactName(name: string): string {
  const safe = name.replace(/[^A-Za-z0-9_.-]+/g, "-").replace(/^-+|-+$/g, "")
  return safe || "failure"
}

function artifactLaunchOptions(options?: LaunchOptions): LaunchOptions | undefined {
  if (!options?.env) return options
  return {
    ...options,
    env: Object.fromEntries(Object.keys(options.env).map((key) => [key, "[redacted]"])),
  }
}
