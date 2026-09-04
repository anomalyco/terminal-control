# @kitlangton/terminal-control

## 1.2.1

### Patch Changes

- 286e7ff: Avoid cloning cached terminal frames for internal text/status inspection and duplicate recording states. Borrow unedited video states instead of copying the entire timeline, and borrow socket paths during named-session request polling. Public captures remain independent owned snapshots; terminal behavior, recording formats, and rendered output are unchanged.

## 1.2.0

### Minor Changes

- d4518f4: Add typed mouse input for hover, clicks, and drags through the CLI, Rust sessions, TypeScript client, and MCP. Encode input using the application's negotiated mouse reporting mode and validate current viewport coordinates.

  Add opt-in video pointer overlays with smooth travel, press feedback, fades, and reduced motion. Keep animation aligned through edits and resizes without changing real input timing. New recordings use format v2 for typed mouse evidence; current readers continue to accept v1 recordings.

### Patch Changes

- Build distributed native binaries for a baseline CPU instead of the build runner's instruction set, preventing illegal-instruction crashes on older CPUs.
- 138ea31: Materialize only the requested recording screenshot instead of retaining every intermediate frame. Video replay no longer constructs an unused ANSI transcript. Preserve recording order, cutoff/marker behavior, and reflow across resizes.
- b0a4d8b: Wake named-session control requests as soon as they arrive instead of waiting for an idle polling sleep, and wake text-readiness checks on PTY output. Clarify immediate screen reads, readiness-driven demo workflows, and intentional capture/typing delays without changing stable-capture defaults.

  Reuse a bounded base raster during pointer video animation instead of repeating terminal text layout for every pointer position. Preserve frame pixels, recording timing, and screenshot behavior.

- d4518f4: Preserve color-query replies when terminal escape prefixes arrive across separate output chunks, exclude invisible text when trimming video startup, and keep the original test failure when automatic failure-artifact capture fails. Simplify shared input encoding, terminal ownership, and recording rendering without changing public interfaces.

## 1.1.1

### Patch Changes

- 96c90e6: Detach background session daemons from their launcher's process group so named sessions started or restarted with the CLI survive launcher-group hangups.
- 4de9fec: Answer cursor-position queries with the actual query-time cursor position when using the OpenTUI host profile, including split and repeated queries, without injecting unsolicited startup cursor reports.

## 1.1.0

### Minor Changes

- 861ad0f: Add state, command, and working-directory filters to CLI and MCP session discovery.

### Patch Changes

- ca75e4e: Render shade blocks and Powerline separators as exact SVG geometry for smooth gradients and seam-free captures.
- e54baad: Preserve terminal cursor styles in structured frames, screenshots, and videos.

## 1.0.0

### Major Changes

- 9e83677: Remove persistent multiplexed workspaces, including attachment, window, pane, layout, and workspace MCP controls. Named background sessions and foreground `run` sessions remain available for terminal automation.

## 0.6.0

### Minor Changes

- 5a0a277: Create a private application semantic socket for commands launched with the OpenTUI host profile and add `show --format semantic` for optional application-provided UI snapshots.

## 0.5.0

### Minor Changes

- cc0be0e: Add persistent, reattachable workspaces with named windows, reorderable tabs, movable split panes, pane
  zoom, a command palette, and workspace-wide pane IDs that remain stable for the workspace lifetime.
  Agents can inspect and control hidden windows and panes through typed CLI and MCP operations, capture
  pane/window/workspace PNGs, record the composed workspace, and discover their current pane context.

### Patch Changes

- 033f0d7: Replace the terminal state engine with Ghostty for accurate reflow, Unicode, attributes, and PTY responses.

## 0.4.1

### Patch Changes

- 1d41583: Update package README: remove stale pre-publication phrasing and point to the full client documentation in the repository docs.
- c1d37db: Make MCP screen reads and interactions return immediately by default, preventing animated terminal output from delaying control requests until the capture deadline.

## 0.4.0

### Minor Changes

- 797b975: Add `termctrl run` for visible foreground sessions, including optional names inferred from the executable basename, and add `termctrl mcp` for structured agent control through the official Rust MCP SDK.

## 0.3.1

### Patch Changes

- Refresh dependencies and make retained-output byte-limit checks overflow-safe.

## 0.3.0

### Minor Changes

- 43acebe: Add an optional `termctrl video --footer` overlay for polished terminal recordings, and reorganize the README around agent-first terminal-control usage.

## 0.2.0

### Minor Changes

- Add marker-based recording inspection and video edit plans for polished terminal demos.
