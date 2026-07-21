# Context

## Glossary

### Capture

One explicitly saved artifact representation of a visible terminal frame. A capture can be derived from a launched command, piped command output, an ANSI/VT stream, or a live session. Routine reads of visible terminal state are called `show` operations and print to standard output rather than creating capture artifacts.

### Frame

The versioned structured visible terminal state underlying a shot. A frame contains geometry, styled cells, and optional cursor state and can be serialized as JSON for external tooling.

### Session

A named terminal application that remains available across waiting, input, resizing, log inspection, and visible-screen reads or captures. A session is `running` while accepting input, or `exited` when its application has ended but its final screen remains inspectable until explicitly stopped. A session retains bounded readable logs and the most recent bounded ANSI/VT transcript bytes; alternate-screen TUIs are read with `show` rather than logs. A session may write a recording timeline while it runs, including viewport resize events. Named CLI sessions retain non-secret launch settings so status can identify them and restart can reuse their command and working directory.

An embedded session owns the same live terminal lifecycle in-process; the named CLI session commands are an adapter for interacting with that lifecycle across invocations.

### Workspace

A persistent daemon containing ordered named windows. Each window owns embedded sessions, called
panes, in an independent recursive split layout with weighted boundaries and reversible pane zoom.
Window names are stable exact selectors, numeric indexes are mutable presentation order, and pane IDs
are workspace-wide and stable for the workspace lifetime. A private ID-only layout tree is
authoritative for geometry, focus, resizing, zoom restoration, and close promotion while pane objects
retain process ownership.

A workspace has zero or one human terminal attachment. Disconnecting preserves every window and
pane; `run NAME` or `attach NAME` adopts the new terminal's geometry and theme and repaints the
selected window. Hidden windows continue pumping output and expose unread output, bell, and
surviving-window pane-exit activity. Workspace panes receive stable environment identity, while the
workspace resolves current membership dynamically after rename or pane movement.

The one-row tab strip is workspace chrome and can move between the top and bottom. Attachment
presentation owns tab selection and reorder, prefix decoding, the command palette, last-window
history, transient notices, geometry synchronization, input modes, inherited colors, and
damage-based painting. Workspace recording serializes that composed presentation as one replayable
timeline, including while detached.

### Driver

A versioned JSON Lines stdin/stdout adapter over embedded sessions for external agent tooling and the TypeScript test client. A driver process can manage multiple isolated sessions without exposing terminal process details to its client. Its capture response includes the reason capture completed so test clients can distinguish settled screen state from deadline fallback, and can optionally include ANSI or rendered SVG failure evidence.

### Recording

A timestamped terminal event timeline containing output, client or automatic host input, viewport resize events, and named editing markers. A recording can be rendered directly to a realtime video that preserves observed timing or rendered through an explicit edit plan that stitches marker ranges with clip-specific speed, holds, and captions. The source recording remains unchanged and should be treated as potentially sensitive.

Agents inspect marker names with `termctrl markers` and inspect exact recording moments with `termctrl show --recording ... --at-marker ...` or `--at-ms ...` before committing to a video edit plan.

### ANSI/VT Stream

Raw terminal output bytes containing text and terminal control sequences. Files commonly use an `.ansi` suffix, but the suffix does not imply a separate container format.
