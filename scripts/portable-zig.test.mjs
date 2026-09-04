import { strict as assert } from "node:assert"
import { spawnSync } from "node:child_process"
import { mkdtemp, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"
import { test } from "node:test"

test("portable Zig selects baseline only for Ghostty and preserves arguments", async () => {
  const directory = await mkdtemp(join(tmpdir(), "termctrl-zig-test-"))
  try {
    const real = join(directory, "real zig")
    await writeFile(real, '#!/bin/sh\nprintf "%s\\n" "$@"\n', { mode: 0o700 })
    const shim = resolve(import.meta.dirname, "portable-zig/zig")
    const run = (args) => spawnSync(shim, args, { env: { ...process.env, TERMCTRL_REAL_ZIG: real }, encoding: "utf8" })
    for (const args of [["version"], ["build", "-Dcpu=native", "--prefix", "path with spaces"], ["build", "-Demit-lib-vt=true", "-Dcpu=baseline"]]) {
      const result = run(args)
      assert.equal(result.status, 0, result.stderr)
      assert.equal(result.stdout, args.join("\n") + "\n")
    }
    const args = ["build", "-Demit-lib-vt=true", "-Doptimize=ReleaseFast", "--prefix", "path with spaces"]
    const result = run(args)
    assert.equal(result.status, 0, result.stderr)
    assert.equal(result.stdout, [...args, "-Dcpu=baseline"].join("\n") + "\n")
    assert.notEqual(run(["build", "-Demit-lib-vt=true", "-Dcpu=native"]).status, 0)
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})
