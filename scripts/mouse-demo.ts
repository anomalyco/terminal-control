// Interactive raw-mode specimen for pointer video review. No network or private data.
// termctrl start pointer-review --cols 72 --rows 20 --record captures/pointer.termctrl -- bun scripts/mouse-demo.ts
const out = (text: string) => process.stdout.write(text)
const at = (x: number, y: number, text: string) => out(`\x1b[${y + 1};${x + 1}H${text}`)
let hover = -1
let selected = 0
let held = false
let level = 8
let status = "Ready"
const labels = ["Overview", "Activity", "Settings"]

function draw() {
  const cols = process.stdout.columns ?? 72
  const rows = process.stdout.rows ?? 20
  out("\x1b[0m\x1b[48;2;20;23;30m\x1b[38;2;220;224;233m\x1b[2J")
  at(4, 2, "\x1b[1mTERMINAL CONTROL\x1b[22m")
  at(4, 4, "\x1b[38;2;148;155;172mReal mouse input. Presentation-only motion.")
  labels.forEach((label, index) => {
    const x = 4 + index * 20
    if (x + 16 >= cols) return
    const background = hover === index ? "60;68;86" : "34;40;52"
    at(x, 7, `\x1b[48;2;${background}m\x1b[38;2;240;242;246m ${label.padEnd(12)} ${selected === index ? "●" : " "} `)
    at(x, 8, ` ${" ".repeat(14)} `)
  })
  out("\x1b[48;2;20;23;30m")
  at(4, 11, "\x1b[38;2;148;155;172mDrag to adjust")
  at(4, 13, `\x1b[38;2;180;198;228m${"━".repeat(level)}●\x1b[38;2;66;75;92m${"━".repeat(32 - level)}`)
  at(4, Math.min(16, rows - 2), `\x1b[38;2;220;224;233m${status}\x1b[K`)
}

process.stdin.setRawMode(true)
out("\x1b[?1049h\x1b[?25l\x1b[?1003h\x1b[?1006h")
draw()
process.stdout.on("resize", draw)
let pending = ""
process.stdin.on("data", (data) => {
  if (data.includes(3) || data.includes(113)) {
    out("\x1b[?1003l\x1b[?1006l\x1b[?25h\x1b[?1049l")
    process.exit(0)
  }
  pending += data.toString()
  let consumed = 0
  for (const match of pending.matchAll(/\x1b\[<(\d+);(\d+);(\d+)([Mm])/g)) {
    const code = Number(match[1])
    const x = Number(match[2]) - 1
    const y = Number(match[3]) - 1
    const release = match[4] === "m"
    hover = y >= 7 && y <= 8 && x >= 4 && x < 60 && (x - 4) % 20 < 16 ? Math.floor((x - 4) / 20) : -1
    if (!(code & 32)) held = !release
    if (hover >= 0 && release) selected = hover
    if (held && y === 13) level = Math.max(0, Math.min(32, x - 4))
    status = held ? `Dragging · ${level}` : hover >= 0 ? `${release ? "Selected" : "Hover"}: ${labels[hover]}` : "Ready"
    consumed = match.index + match[0].length
    draw()
  }
  pending = pending.slice(consumed)
})
