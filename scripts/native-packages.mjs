import { chmod, cp, mkdir, mkdtemp, readFile, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"
import { spawnSync } from "node:child_process"
import { pathToFileURL } from "node:url"

const repository = resolve(import.meta.dirname, "..")

export const nativePackages = [
  { target: "darwin-arm64", name: "@kitlangton/terminal-control-darwin-arm64" },
  { target: "darwin-x64", name: "@kitlangton/terminal-control-darwin-x64" },
  { target: "linux-arm64-gnu", name: "@kitlangton/terminal-control-linux-arm64-gnu" },
  { target: "linux-x64-gnu", name: "@kitlangton/terminal-control-linux-x64-gnu" },
]

export const runtimeTargets = {
  "darwin-arm64": "darwin-arm64",
  "darwin-x64": "darwin-x64",
  "linux-arm64": "linux-arm64-gnu",
  "linux-x64": "linux-x64-gnu",
}

export function nativePackage(target) {
  const entry = nativePackages.find((entry) => entry.target === target)
  if (!entry) throw new Error(`unsupported native npm package target ${target}`)
  return entry
}

export async function packNativePackage(target, binary, output) {
  const native = nativePackage(target)
  const packageDirectory = join(repository, "packages", target)
  const manifest = JSON.parse(await readFile(join(packageDirectory, "package.json"), "utf8"))
  const binaryVersion = run(binary, ["--version"], repository).trim().split(/\s+/).at(-1)
  if (binaryVersion !== manifest.version) {
    throw new Error(
      `${native.name}@${manifest.version} cannot package termctrl ${binaryVersion ?? "with no version"}`,
    )
  }
  const staging = await mkdtemp(join(tmpdir(), `termctrl-${target}-package-`))
  try {
    await mkdir(join(staging, "bin"), { recursive: true })
    await mkdir(output, { recursive: true })
    await cp(join(packageDirectory, "package.json"), join(staging, "package.json"))
    await cp(join(packageDirectory, "README.md"), join(staging, "README.md"))
    await cp(join(repository, "LICENSE"), join(staging, "LICENSE"))
    await cp(
      join(repository, "THIRD_PARTY_LICENSES.md"),
      join(staging, "THIRD_PARTY_LICENSES.md"),
    )
    await cp(binary, join(staging, "bin", "termctrl"))
    await chmod(join(staging, "bin", "termctrl"), 0o755)
    return pack(staging, output)
  } finally {
    await rm(staging, { recursive: true, force: true })
  }
}

export function pack(directory, output, ignoreScripts = false) {
  const flags = ["pack", "--json", "--pack-destination", output]
  if (ignoreScripts) flags.push("--ignore-scripts")
  const stdout = run("npm", flags, directory)
  const [{ filename }] = JSON.parse(stdout)
  return join(output, filename)
}

export function run(command, args, cwd) {
  const result = spawnSync(command, args, { cwd, encoding: "utf8", stdio: "pipe" })
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed:\n${result.stdout}\n${result.stderr}`)
  }
  return result.stdout
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const [, , target, binary, output] = process.argv
  if (!target || !binary || !output) {
    throw new Error("usage: node scripts/native-packages.mjs TARGET BINARY OUTPUT_DIRECTORY")
  }
  console.log(await packNativePackage(target, resolve(binary), resolve(output)))
}
