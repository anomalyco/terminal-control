const binary = process.env.TERMCTRL_BIN ?? "./target/release/termctrl"
const runs = Number(process.env.RUNS ?? 9)
const suffix = `${process.pid}-${Date.now()}`
const workspace = `bench-idle-${suffix}`
const bootstrap = `bench-idle-host-${suffix}`

if (process.platform !== "darwin") {
  throw new Error("workspace idle benchmark currently requires macOS top")
}

function spawn(command: ReadonlyArray<string>, allowFailure = false) {
  const result = Bun.spawnSync(command, {
    cwd: process.cwd(),
    stdout: "pipe",
    stderr: "pipe",
  })
  if (!allowFailure && result.exitCode !== 0) {
    throw new Error(
      `${command.join(" ")} failed: ${result.stderr.toString().trim()}`,
    )
  }
  return result.stdout.toString()
}

function termctrl(args: ReadonlyArray<string>, allowFailure = false) {
  return spawn([binary, ...args], allowFailure)
}

function median(values: ReadonlyArray<number>) {
  const sorted = [...values].sort((left, right) => left - right)
  const middle = Math.floor(sorted.length / 2)
  return sorted.length % 2 === 0
    ? (sorted[middle - 1]! + sorted[middle]!) / 2
    : sorted[middle]!
}

function cleanup(name: string) {
  termctrl(["stop", name], true)
}

function waitForSession(name: string) {
  const deadline = performance.now() + 5_000
  while (performance.now() < deadline) {
    try {
      termctrl(["status", name])
      return
    } catch {
      Bun.sleepSync(10)
    }
  }
  throw new Error(`timed out waiting for ${name}`)
}

try {
  termctrl([
    "start",
    bootstrap,
    "--cols",
    "120",
    "--rows",
    "40",
    "--",
    binary,
    "run",
    workspace,
    "--",
    "sh",
    "-c",
    "exec sleep 300",
  ])
  waitForSession(workspace)
  cleanup(bootstrap)
  waitForSession(workspace)
  Bun.sleepSync(600)

  const uid = spawn(["id", "-u"]).trim()
  const runtime = process.env.TERMCTRL_RUNTIME_DIR ?? `/tmp/termctrl-${uid}`
  const socket = `${runtime}/${workspace}.sock`
  const daemon = spawn(["lsof", "-n", "-t", socket])
    .trim()
    .split("\n")[0]
  if (!daemon) throw new Error(`could not find daemon listening on ${socket}`)

  const output = spawn([
    "top",
    "-l",
    String(runs + 1),
    "-s",
    "1",
    "-pid",
    daemon,
    "-stats",
    "pid,cpu,csw",
  ])
  const samples = output
    .split("\n")
    .map((line) => line.trim().split(/\s+/))
    .filter((fields) => fields[0] === daemon)
    .map((fields) => ({
      cpu: Number(fields[1]),
      contextSwitches: Number(fields[2]?.replace(/\+$/, "")),
    }))
  if (samples.length !== runs + 1) {
    throw new Error(`expected ${runs + 1} top samples, received ${samples.length}`)
  }

  const cpu = samples.slice(1).map((sample) => sample.cpu)
  const contextSwitches = samples
    .slice(1)
    .map((sample, index) => sample.contextSwitches - samples[index]!.contextSwitches)
  const cpuMedian = median(cpu)
  const switchesMedian = median(contextSwitches)
  console.log(`detached workspace idle (${runs} measured seconds)`)
  console.log(`context switches/s median: ${switchesMedian.toFixed(1)}`)
  console.log(`idle CPU median: ${cpuMedian.toFixed(2)}%`)
  console.log(`METRIC workspace_idle_context_switches_per_s=${switchesMedian.toFixed(1)}`)
  console.log(`METRIC workspace_idle_cpu_percent=${cpuMedian.toFixed(2)}`)
} finally {
  cleanup(bootstrap)
  cleanup(workspace)
}
