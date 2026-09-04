import { expect, test } from "bun:test"
import { mkdtemp, rm } from "node:fs/promises"
import { createServer } from "node:net"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { BoxRenderable } from "@opentui/core"
import { createTestRenderer } from "@opentui/core/testing"
import { elements, provideTerminalControl, semanticSnapshot } from "./index"
import { provideSemanticSnapshot } from "./provider"

test("discovers the same live interactable elements used by semantic snapshots", async () => {
  const setup = await createTestRenderer({ width: 20, height: 5 })
  const button = new BoxRenderable(setup.renderer, {
    id: "submit",
    width: 10,
    height: 3,
    onMouseDown: () => {},
  })
  setup.renderer.root.add(button)
  await setup.renderOnce()

  expect(elements(setup.renderer)).toEqual([
    {
      id: "submit",
      num: button.num,
      x: 0,
      y: 0,
      width: 10,
      height: 3,
      focusable: false,
      focused: false,
      clickable: true,
      editor: false,
    },
  ])
  expect(semanticSnapshot(setup.renderer)).toEqual({
    format: "termctrl-semantic-snapshot-v1",
    nodes: [
      {
        id: "submit",
        role: "button",
        label: "submit",
        element: button.num,
        focused: false,
        disabled: false,
      },
    ],
  })
  setup.renderer.destroy()
})

test("omits controls hidden or clipped by their ancestors", async () => {
  const setup = await createTestRenderer({ width: 20, height: 5 })
  const hidden = new BoxRenderable(setup.renderer, { id: "hidden", visible: false })
  hidden.add(new BoxRenderable(setup.renderer, { id: "hidden-child", focusable: true, width: 5, height: 1 }))
  const clipped = new BoxRenderable(setup.renderer, { id: "clipped", overflow: "scroll", width: 5, height: 1 })
  clipped.add(
    new BoxRenderable(setup.renderer, {
      id: "clipped-child",
      focusable: true,
      position: "absolute",
      left: 10,
      width: 5,
      height: 1,
    }),
  )
  setup.renderer.root.add(hidden)
  setup.renderer.root.add(clipped)
  await setup.renderOnce()

  expect(semanticSnapshot(setup.renderer).nodes).toEqual([])
  setup.renderer.destroy()
})

test("keeps nested discovery order and descendant labels attached to their renderable", async () => {
  const setup = await createTestRenderer({ width: 20, height: 5 })
  try {
    const parent = new BoxRenderable(setup.renderer, { id: "parent", width: 12, height: 3, onMouseDown: () => {} })
    const child = new BoxRenderable(setup.renderer, { id: "child", width: 10, height: 1, focusable: true })
    Reflect.set(child, "plainText", " Nested   action ")
    parent.add(child)
    setup.renderer.root.add(parent)
    await setup.renderOnce()
    expect(elements(setup.renderer).map((element) => element.num)).toEqual([parent.num, child.num])
    expect(semanticSnapshot(setup.renderer).nodes.map(({ element, label }) => ({ element, label }))).toEqual([
      { element: parent.num, label: "Nested action" },
      { element: child.num, label: "Nested action" },
    ])
  } finally {
    setup.renderer.destroy()
  }
})

test("excludes hidden descendants from labels and reports clipped geometry", async () => {
  const setup = await createTestRenderer({ width: 20, height: 5 })
  const button = new BoxRenderable(setup.renderer, { id: "button", focusable: true, width: 5, height: 1 })
  const secret = new BoxRenderable(setup.renderer, { visible: false, width: 5, height: 1 })
  Reflect.set(secret, "plainText", "secret")
  button.add(secret)
  const faded = new BoxRenderable(setup.renderer, { opacity: 0, width: 5, height: 1 })
  Reflect.set(faded, "plainText", "faded secret")
  button.add(faded)
  const clipped = new BoxRenderable(setup.renderer, { id: "clip", overflow: "hidden", width: 5, height: 1 })
  const partial = new BoxRenderable(setup.renderer, {
    id: "partial",
    focusable: true,
    position: "absolute",
    left: 4,
    width: 5,
    height: 1,
  })
  clipped.add(partial)
  setup.renderer.root.add(button)
  setup.renderer.root.add(clipped)
  await setup.renderOnce()

  expect(semanticSnapshot(setup.renderer).nodes.find((node) => node.id === "button")?.label).toBe("button")
  expect(elements(setup.renderer).find((element) => element.id === "partial")).toMatchObject({ x: 4, width: 1 })
  setup.renderer.destroy()
})

test("omits controls with zero effective opacity", async () => {
  const setup = await createTestRenderer({ width: 20, height: 5 })
  const transparent = new BoxRenderable(setup.renderer, {
    id: "transparent",
    focusable: true,
    opacity: 0,
    width: 5,
    height: 1,
  })
  const parent = new BoxRenderable(setup.renderer, { id: "transparent-parent", opacity: 0, width: 5, height: 1 })
  parent.add(new BoxRenderable(setup.renderer, { id: "transparent-child", focusable: true, width: 5, height: 1 }))
  setup.renderer.root.add(transparent)
  setup.renderer.root.add(parent)
  await setup.renderOnce()

  expect(semanticSnapshot(setup.renderer).nodes).toEqual([])
  setup.renderer.destroy()
})

test("keeps mouse controls reachable outside an occluded midpoint", async () => {
  const setup = await createTestRenderer({ width: 20, height: 5 })
  const button = new BoxRenderable(setup.renderer, {
    id: "wide-button",
    width: 10,
    height: 1,
    onMouseDown: () => {},
  })
  const overlay = new BoxRenderable(setup.renderer, {
    id: "overlay",
    position: "absolute",
    left: 4,
    width: 2,
    height: 1,
    zIndex: 1,
    onMouseDown: () => {},
  })
  setup.renderer.root.add(button)
  setup.renderer.root.add(overlay)
  await setup.renderOnce()

  expect(elements(setup.renderer).find((element) => element.id === "wide-button")?.clickable).toBe(true)
  setup.renderer.destroy()
})

test("keeps generated semantic node ids unique", async () => {
  const setup = await createTestRenderer({ width: 20, height: 5 })
  const first = new BoxRenderable(setup.renderer, { focusable: true, width: 1, height: 1 })
  const second = new BoxRenderable(setup.renderer, { focusable: true, width: 1, height: 1 })
  const third = new BoxRenderable(setup.renderer, { focusable: true, width: 1, height: 1 })
  first.id = `same-${third.num}`
  second.id = "same"
  third.id = "same"
  setup.renderer.root.add(first)
  setup.renderer.root.add(second)
  setup.renderer.root.add(third)
  await setup.renderOnce()

  const ids = semanticSnapshot(setup.renderer).nodes.map((node) => node.id)
  expect(new Set(ids).size).toBe(ids.length)
  setup.renderer.destroy()
})

test("provides semantic snapshots over the discovered Terminal Control socket", async () => {
  const setup = await createTestRenderer({ width: 20, height: 5 })
  setup.renderer.root.add(
    new BoxRenderable(setup.renderer, {
      id: "submit",
      width: 10,
      height: 3,
      onMouseDown: () => {},
    }),
  )
  await setup.renderOnce()
  const directory = await mkdtemp(join(tmpdir(), "termctrl-opentui-"))
  const socketPath = join(directory, "semantic.sock")
  let resolveResult!: (value: unknown) => void
  const result = new Promise((resolve) => {
    resolveResult = resolve
  })
  const server = createServer((socket) => {
    socket.setEncoding("utf8")
    let buffer = ""
    socket.on("data", (chunk) => {
      buffer += chunk
      const lines = buffer.split("\n")
      buffer = lines.pop() ?? ""
      for (const line of lines) {
        const message = JSON.parse(line)
        if (message.type === "hello") {
          expect(message.capabilities).toEqual(["semantic.snapshot"])
          socket.write(`${JSON.stringify({ type: "ready", protocolVersion: 1 })}\n`)
          socket.write(`${JSON.stringify({ type: "semantic.snapshot", id: 1 })}\n`)
        }
        if (message.type === "result") resolveResult(message.value)
      }
    })
  })
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject)
    server.listen(socketPath, resolve)
  })
  const provider = provideTerminalControl(setup.renderer, {
    application: { name: "fixture", version: "1.0.0" },
    socketPath,
  })

  try {
    expect(await provider.ready).toBe(true)
    expect(await result).toEqual(semanticSnapshot(setup.renderer))
  } finally {
    provider.close()
    setup.renderer.destroy()
    await new Promise<void>((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())))
    await rm(directory, { recursive: true, force: true })
  }
})

test("reconnects after an established Terminal Control connection closes", async () => {
  const directory = await mkdtemp(join(tmpdir(), "termctrl-opentui-reconnect-"))
  const socketPath = join(directory, "semantic.sock")
  let connections = 0
  let resolveResult!: (value: unknown) => void
  const result = new Promise((resolve) => {
    resolveResult = resolve
  })
  const server = createServer((socket) => {
    connections++
    const connection = connections
    socket.setEncoding("utf8")
    let buffer = ""
    socket.on("data", (chunk) => {
      buffer += chunk
      const lines = buffer.split("\n")
      buffer = lines.pop() ?? ""
      for (const line of lines) {
        const message = JSON.parse(line)
        if (message.type === "hello") {
          socket.write(`${JSON.stringify({ type: "ready", protocolVersion: 1 })}\n`)
          if (connection === 1) setTimeout(() => socket.destroy(), 10)
          else socket.write(`${JSON.stringify({ type: "semantic.snapshot", id: 2 })}\n`)
        }
        if (message.type === "result") resolveResult(message.value)
      }
    })
  })
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject)
    server.listen(socketPath, resolve)
  })
  const provider = provideSemanticSnapshot({
    application: { name: "fixture" },
    socketPath,
    snapshot: () => ({ format: "termctrl-semantic-snapshot-v1", nodes: [] }),
  })

  try {
    expect(await provider.ready).toBe(true)
    expect(
      await Promise.race([
        result,
        Bun.sleep(2_000).then(() => {
          throw new Error("provider did not reconnect")
        }),
      ]),
    ).toEqual({ format: "termctrl-semantic-snapshot-v1", nodes: [] })
    expect(connections).toBe(2)
  } finally {
    provider.close()
    await new Promise<void>((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())))
    await rm(directory, { recursive: true, force: true })
  }
})

test("keeps retrying when the first reconnect attempt fails", async () => {
  const directory = await mkdtemp(join(tmpdir(), "termctrl-opentui-retry-"))
  const socketPath = join(directory, "semantic.sock")
  let firstSocket: import("node:net").Socket | undefined
  let resolveFailedRetry!: () => void
  const failedRetry = new Promise<void>((resolve) => {
    resolveFailedRetry = resolve
  })
  const firstServer = createServer((socket) => {
    firstSocket = socket
    socket.setEncoding("utf8")
    socket.once("data", () => socket.write(`${JSON.stringify({ type: "ready", protocolVersion: 1 })}\n`))
  })
  await new Promise<void>((resolve, reject) => {
    firstServer.once("error", reject)
    firstServer.listen(socketPath, resolve)
  })
  const provider = provideSemanticSnapshot({
    application: { name: "fixture" },
    socketPath,
    snapshot: () => ({ recovered: true }),
    onError: () => resolveFailedRetry(),
  })

  try {
    expect(await provider.ready).toBe(true)
    firstSocket?.destroy()
    await new Promise<void>((resolve, reject) => firstServer.close((error) => (error ? reject(error) : resolve())))
    await failedRetry

    let resolveResult!: (value: unknown) => void
    const result = new Promise((resolve) => {
      resolveResult = resolve
    })
    const secondServer = createServer((socket) => {
      socket.setEncoding("utf8")
      let buffer = ""
      socket.on("data", (chunk) => {
        buffer += chunk
        const lines = buffer.split("\n")
        buffer = lines.pop() ?? ""
        for (const line of lines) {
          const message = JSON.parse(line)
          if (message.type === "hello") {
            socket.write(`${JSON.stringify({ type: "ready", protocolVersion: 1 })}\n`)
            socket.write(`${JSON.stringify({ type: "semantic.snapshot", id: 3 })}\n`)
          }
          if (message.type === "result") resolveResult(message.value)
        }
      })
    })
    await new Promise<void>((resolve, reject) => {
      secondServer.once("error", reject)
      secondServer.listen(socketPath, resolve)
    })
    try {
      expect(
        await Promise.race([
          result,
          Bun.sleep(2_000).then(() => {
            throw new Error("provider stopped retrying")
          }),
        ]),
      ).toEqual({ recovered: true })
    } finally {
      provider.close()
      await new Promise<void>((resolve, reject) =>
        secondServer.close((error) => (error ? reject(error) : resolve())),
      )
    }
  } finally {
    provider.close()
    await rm(directory, { recursive: true, force: true })
  }
})
