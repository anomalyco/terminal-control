import type { CliRenderer, Renderable } from "@opentui/core"
import { provideSemanticSnapshot, type Provider } from "./provider.js"

export type InteractableElement = {
  readonly id: string
  readonly num: number
  readonly x: number
  readonly y: number
  readonly width: number
  readonly height: number
  readonly focusable: boolean
  readonly focused: boolean
  readonly clickable: boolean
  readonly editor: boolean
}

export type SemanticNode = {
  readonly id: string
  readonly role: string
  readonly label?: string
  readonly element: number
  readonly focused: boolean
  readonly disabled: boolean
}

export type SemanticSnapshot = {
  readonly format: "termctrl-semantic-snapshot-v1"
  readonly nodes: ReadonlyArray<SemanticNode>
}

function children(renderable: Renderable) {
  return renderable.getChildren().filter((child): child is Renderable => "num" in child)
}

function all(renderable: Renderable): Renderable[] {
  return [renderable, ...children(renderable).flatMap(all)]
}

function hasMouseListeners(renderable: Renderable) {
  const listener = Reflect.get(renderable, "_mouseListener")
  const listeners = Reflect.get(renderable, "_mouseListeners")
  return Boolean(listener) || (isRecord(listeners) && Object.keys(listeners).length > 0)
}

function receivesClick(renderer: CliRenderer, renderable: Renderable, bounds: Pick<InteractableElement, "x" | "y" | "width" | "height">) {
  const targets = new Set(all(renderable).map((item) => item.num))
  for (let y = Math.floor(bounds.y); y < Math.ceil(bounds.y + bounds.height); y++) {
    for (let x = Math.floor(bounds.x); x < Math.ceil(bounds.x + bounds.width); x++) {
      if (targets.has(renderer.hitTest(x, y))) return true
    }
  }
  return false
}

export function elements(renderer: CliRenderer): InteractableElement[] {
  return discover(renderer).map(({ element }) => element)
}

function discover(renderer: CliRenderer) {
  return all(renderer.root).flatMap((renderable) => {
    const bounds = visibleBounds(renderer, renderable)
    if (!bounds) return []
    const element: InteractableElement = {
      id: renderable.id,
      num: renderable.num,
      ...bounds,
      focusable: renderable.focusable,
      focused: renderable.focused,
      clickable: hasMouseListeners(renderable) && receivesClick(renderer, renderable, bounds),
      editor: renderer.currentFocusedEditor === renderable,
    }
    return element.focusable || element.clickable || element.editor ? [{ renderable, element }] : []
  })
}

export function semanticSnapshot(renderer: CliRenderer): SemanticSnapshot {
  const ids = new Set<string>()
  return {
    format: "termctrl-semantic-snapshot-v1",
    nodes: discover(renderer).map(({ element, renderable }) => {
      const preferred = element.id || `renderable-${element.num}`
      let id = preferred
      for (let suffix = element.num; ids.has(id); suffix++) id = `${preferred}-${suffix}`
      ids.add(id)
      return {
        id,
        role: element.editor ? "textbox" : element.clickable ? "button" : "control",
        label: label(renderer, renderable, element),
        element: element.num,
        focused: element.focused || element.editor,
        disabled: false,
      }
    }),
  }
}

function visibleBounds(renderer: CliRenderer, renderable: Renderable) {
  if (
    renderable.isDestroyed ||
    !renderable.visible ||
    renderable.opacity <= 0 ||
    renderable.width <= 0 ||
    renderable.height <= 0
  ) {
    return undefined
  }
  let left = Math.max(0, renderable.screenX)
  let top = Math.max(0, renderable.screenY)
  let right = Math.min(renderer.width, renderable.screenX + renderable.width)
  let bottom = Math.min(renderer.height, renderable.screenY + renderable.height)
  for (let parent = renderable.parent; parent; parent = parent.parent) {
    if (parent.isDestroyed || !parent.visible || parent.opacity <= 0) return undefined
    if (parent.overflow !== "visible") {
      left = Math.max(left, parent.screenX)
      top = Math.max(top, parent.screenY)
      right = Math.min(right, parent.screenX + parent.width)
      bottom = Math.min(bottom, parent.screenY + parent.height)
    }
  }
  if (right <= left || bottom <= top) return undefined
  return { x: left, y: top, width: right - left, height: bottom - top }
}

export function provideTerminalControl(
  renderer: CliRenderer,
  options: {
    readonly application: { readonly name: string; readonly version?: string }
    readonly socketPath?: string
    readonly onError?: (error: unknown) => void
  },
): Provider {
  return provideSemanticSnapshot({
    ...options,
    snapshot: async () => {
      renderer.requestRender()
      await renderer.idle()
      return semanticSnapshot(renderer)
    },
  })
}

function label(renderer: CliRenderer, renderable: Renderable, element: InteractableElement) {
  if (element.editor) return element.id || undefined
  const text = all(renderable)
    .filter((item) => visibleBounds(renderer, item) !== undefined)
    .map((item) => Reflect.get(item, "plainText"))
    .find((value): value is string => typeof value === "string" && value.trim().length > 0)
  return text?.replaceAll(/\s+/g, " ").trim() || element.id || undefined
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}
