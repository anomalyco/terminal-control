import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { basename, join, resolve } from "node:path"
import { pack, run } from "./native-packages.mjs"

const temp = await mkdtemp(join(tmpdir(), "termctrl-opentui-validation-"))
try {
  const tarball = await inputTarball(temp)
  const manifest = JSON.parse(run("tar", ["-xOf", tarball, "package/package.json"]))
  if (manifest.name !== "@kitlangton/terminal-control-opentui") {
    throw new Error(`unexpected OpenTUI adapter package name ${manifest.name}`)
  }
  const client = JSON.parse(await readFile(new URL("../packages/test/package.json", import.meta.url), "utf8"))
  if (manifest.version !== client.version) {
    throw new Error(`OpenTUI adapter is ${manifest.version}, but TypeScript client is ${client.version}; prepare aligned versions before validation`)
  }
  for (const opentuiVersion of ["0.4.1", "0.4.5"]) {
    const consumer = join(temp, `opentui-${opentuiVersion}`)
    await mkdir(consumer, { recursive: true })
    await writeFile(
      join(consumer, "package.json"),
      `${JSON.stringify(
        {
          private: true,
          type: "module",
          dependencies: {
            "@kitlangton/terminal-control-opentui": `file:${tarball}`,
            "@opentui/core": opentuiVersion,
            "@types/node": "^24.0.0",
            typescript: "^5.9.0",
          },
        },
        null,
        2,
      )}\n`,
    )
    await writeFile(
      join(consumer, "tsconfig.json"),
      `${JSON.stringify(
        {
          compilerOptions: {
            strict: true,
            noEmit: true,
            target: "ESNext",
            module: "NodeNext",
            moduleResolution: "NodeNext",
            skipLibCheck: true,
            types: ["node"],
          },
          include: ["consumer.ts"],
        },
        null,
        2,
      )}\n`,
    )
    await writeFile(
      join(consumer, "consumer.ts"),
      `import { elements, provideTerminalControl, semanticSnapshot } from "@kitlangton/terminal-control-opentui"\nvoid elements\nvoid provideTerminalControl\nvoid semanticSnapshot\n`,
    )
    await writeFile(
      join(consumer, "consumer.mjs"),
      `import { elements, provideTerminalControl, semanticSnapshot } from "@kitlangton/terminal-control-opentui"\nfor (const value of [elements, provideTerminalControl, semanticSnapshot]) {\n  if (typeof value !== "function") throw new Error("missing OpenTUI adapter export")\n}\n`,
    )
    run("npm", ["install", "--ignore-scripts"], consumer)
    run("npm", ["exec", "--", "tsc", "--noEmit"], consumer)
    run("node", ["consumer.mjs"], consumer)
  }
  console.log(`validated ${basename(tarball)} with OpenTUI 0.4.1 and 0.4.5 consumers`)
} finally {
  await rm(temp, { recursive: true, force: true })
}

async function inputTarball(temp) {
  if (!process.argv[2]) return pack(resolve(import.meta.dirname, "../packages/opentui"), temp)
  const input = resolve(process.argv[2])
  const files = await readdir(input)
  const matches = files.filter((file) => file.startsWith("kitlangton-terminal-control-opentui-") && file.endsWith(".tgz"))
  if (matches.length !== 1) {
    throw new Error(`expected one OpenTUI adapter tarball in ${input}, found ${matches.length}`)
  }
  return join(input, matches[0])
}
