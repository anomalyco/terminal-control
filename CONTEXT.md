# Context

## Glossary

### Capture

One explicitly saved artifact representation of a visible terminal frame. A capture can be derived from a launched command, piped command output, an ANSI/VT stream, or a live session. Routine reads of visible terminal state are called `show` operations and print to standard output rather than creating capture artifacts.

### Frame

The versioned structured visible terminal state underlying a shot. A frame contains geometry, styled cells, and optional cursor state and can be serialized as JSON for external tooling.

### Session

A named terminal application that remains available across waiting, input, resizing, log inspection, and visible-screen reads or captures. A session is `running` while accepting input, or `exited` when its application has ended but its final screen remains inspectable until explicitly stopped. A session retains bounded readable logs and the most recent bounded ANSI/VT transcript bytes; alternate-screen TUIs are read with `show` rather than logs. A session may write a recording timeline while it runs, including viewport resize events. Named CLI sessions retain non-secret launch settings so status can identify them and restart can reuse their command and working directory.

An embedded session owns the same live terminal lifecycle in-process; the named CLI session commands are an adapter for interacting with that lifecycle across invocations. A named session daemon starts in a separate Unix session from its launcher, so shell and process-group hangups do not end it. It remains available until explicitly stopped, including after its application exits while the final screen is retained.

Mouse control addresses zero-based cells in the target application's viewport. For workspace panes,
coordinates are pane-local and exclude composed workspace chrome and borders. Live mouse input is
checked against the actual selected target before PTY input or structured recording mutation. A click
is one primary button press and release; a drag is a primary press, interpolated held-button motion,
and release.
Direct named sessions may explicitly enable structured pointer recording. That produces recording
version 2 press, move, and release events and exposes the current pointer state to opt-in live SVG or
PNG rendering. Bare pointer rendering fades after inactivity; persistent rendering retains the last
position at full opacity while keeping click feedback transient. Neither mode renders before the first
event. Secondary clicks add an optional button field to their version 2 structured events; primary
events keep the earlier representation. Default direct and composed-workspace recordings remain version 1.

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

A timestamped terminal event timeline containing output, client or automatic host input, viewport
resize events, and named editing markers. Default recordings use version 1. Explicit pointer capture
uses version 2 and adds structured press, move, and release events; replay, saved images, and video
render the high-contrast pointer only when requested. A recording can be rendered directly to a
realtime video that preserves observed timing or rendered through an explicit edit plan that stitches
marker ranges with clip-specific speed, holds, and captions. The source recording remains unchanged
and should be treated as potentially sensitive.

Agents inspect marker names with `termctrl markers` and inspect exact recording moments with `termctrl show --recording ... --at-marker ...` or `--at-ms ...` before committing to a video edit plan.

### Tape

A UTF-8, line-oriented `.tape` source program for a deterministic demo. The complete tape is parsed
and validated before its named session is launched. Header directives fix viewport, launch argv,
working directory, environment, host profile, recording path, and resource limits. Repeatable setup
actions run before launch; ordered steps use visible-text waits, paced text or key input, click and
drag controls, markers, presentation holds, bounded argv-based host actions, and a required clean
stop. Reverse-order cleanup runs only after the owned session is confirmed stopped, on both success
and safe failure paths. Relative paths resolve from the tape directory, failures identify the source
line, and cleanup failures follow rather than mask the primary failure. Mouse endpoints are validated
against the declared viewport before lifecycle effects. Action output capture and drain are finite;
escaped new-session descendants can outlive the action group but cannot retain pipes indefinitely and
block owned-session cleanup. JSON receipt paths are validated as UTF-8 before setup or launch.
Tape pointer position is execution state, not recording policy. The first unpressed Move establishes
it with one no-button SGR event; subsequent Moves interpolate from it, and Click, RightClick, or Drag updates it.
`Pointer on` only decides whether those inputs are also retained as structured v2 events.
Tape Key uses the live CLI input parser, including `shift+enter`. Wait remains substring-based unless
`Match line` requests equality with a complete visible row. Successful playback has human, JSON, and
quiet receipt modes.

A tape is executable authoring input and should be reviewed before use. It is not a recording:
`.termctrl` remains the timestamped observed timeline, and `video --edit` remains a separate explicit
rendering phase.
Tapes may opt into recording version 2 with `Pointer on`; otherwise they preserve version 1 behavior.

### ANSI/VT Stream

Raw terminal output bytes containing text and terminal control sequences. Files commonly use an `.ansi` suffix, but the suffix does not imply a separate container format.

### Semantic Snapshot

Optional application-provided structured UI state read with `show --format semantic`. For applications launched with the OpenTUI host profile, Terminal Control advertises a private `TERMCTRL_SEMANTIC_SOCKET`; a cooperating application may expose one `semantic.snapshot` capability over it. No provider produces an empty snapshot. Semantic state describes interactable application elements and complements, but does not replace, visible terminal evidence from `show`. The OpenTUI adapter package derives snapshots consistently from a live renderer.
