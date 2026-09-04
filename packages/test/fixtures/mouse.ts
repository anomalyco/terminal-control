// Real raw-mode PTY fixture: echo the negotiated SGR mouse reports visibly.
process.stdin.setRawMode(true)
process.stdout.write("\x1b[?1003h\x1b[?1006hready\r\n")
let pending = ""
process.stdin.on("data", (data) => {
  pending += data.toString()
  const report = /\x1b\[<(\d+);(\d+);(\d+)([Mm])/g
  let consumed = 0
  for (const match of pending.matchAll(report)) {
    process.stdout.write(`mouse:${match[1]}:${match[2]}:${match[3]}:${match[4]}\r\n`)
    consumed = match.index + match[0].length
  }
  pending = pending.slice(consumed)
})
