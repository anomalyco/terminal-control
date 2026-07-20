const binary = process.env.TERMCTRL_BIN ?? "./target/release/termctrl"
const runs = Number(process.env.RUNS ?? 9)
const suffix = `${process.pid}-${Date.now()}`
const workspace = `bench-attach-${suffix}`
const bootstrap = `bench-bootstrap-${suffix}`

function command(args: ReadonlyArray<string>, allowFailure = false) {
  const result = Bun.spawnSync([binary, ...args], {
    cwd: process.cwd(),
    stdout: "pipe",
    stderr: "pipe",
  })
  if (!allowFailure && result.exitCode !== 0) {
    throw new Error(
      `${binary} ${args.join(" ")} failed: ${result.stderr.toString().trim()}`,
    )
  }
  return result.stdout.toString()
}

function waitForScreen(name: string) {
  const deadline = performance.now() + 5_000
  while (performance.now() < deadline) {
    const screen = command(["show", name], true)
    if (screen.trim().length > 0) return
    Bun.sleepSync(2)
  }
  throw new Error(`timed out waiting for a visible frame in ${name}`)
}

function median(values: ReadonlyArray<number>) {
  const sorted = [...values].sort((left, right) => left - right)
  const middle = Math.floor(sorted.length / 2)
  return sorted.length % 2 === 0
    ? (sorted[middle - 1]! + sorted[middle]!) / 2
    : sorted[middle]!
}

function cleanup(name: string) {
  command(["stop", name], true)
}

const clients: Array<string> = []
try {
  command([
    "start",
    bootstrap,
    "--cols",
    "160",
    "--rows",
    "44",
    "--",
    binary,
    "run",
    workspace,
  ])
  waitForScreen(bootstrap)
  cleanup(bootstrap)
  Bun.sleepSync(20)

  const measured: Array<number> = []
  const startPhases: Array<number> = []
  const framePhases: Array<number> = []
  for (let index = 0; index <= runs; index++) {
    const client = `bench-client-${suffix}-${index}`
    clients.push(client)
    const started = performance.now()
    command([
      "start",
      client,
      "--cols",
      "160",
      "--rows",
      "44",
      "--",
      binary,
      "attach",
      workspace,
    ])
    const startDone = performance.now()
    waitForScreen(client)
    const elapsed = performance.now() - started
    cleanup(client)
    Bun.sleepSync(20)
    if (index > 0) {
      measured.push(elapsed)
      startPhases.push(startDone - started)
      framePhases.push(elapsed - (startDone - started))
      console.log(
        `run ${index}: total=${elapsed.toFixed(1)}ms start=${(startDone - started).toFixed(1)}ms frame=${(elapsed - (startDone - started)).toFixed(1)}ms`,
      )
    }
  }

  const result = median(measured)
  const deviations = measured.map((value) => Math.abs(value - result))
  console.log(`workspace attach-to-first-frame (${runs} measured runs)`)
  console.log(`median: ${result.toFixed(1)} ms`)
  console.log(`MAD: ${median(deviations).toFixed(1)} ms`)
  console.log(`best: ${Math.min(...measured).toFixed(1)} ms`)
  console.log(`worst: ${Math.max(...measured).toFixed(1)} ms`)
  console.log(`METRIC workspace_attach_median_ms=${result.toFixed(1)}`)
  console.log(
    `METRIC workspace_attach_mad_ms=${median(deviations).toFixed(1)}`,
  )
  console.log(
    `METRIC workspace_attach_start_median_ms=${median(startPhases).toFixed(1)}`,
  )
  console.log(
    `METRIC workspace_attach_frame_median_ms=${median(framePhases).toFixed(1)}`,
  )
} finally {
  for (const client of clients) cleanup(client)
  cleanup(bootstrap)
  cleanup(workspace)
}
