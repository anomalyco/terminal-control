# Context

## Glossary

### Capture

One explicitly saved artifact representation of a visible terminal frame. A capture can be derived from a launched command, piped command output, an ANSI/VT stream, or a live session. Routine reads of visible terminal state are called `show` operations and print to standard output rather than creating capture artifacts.

### Frame

The versioned structured visible terminal state underlying a shot. A frame contains geometry, styled cells, and optional cursor state and can be serialized as JSON for external tooling.

### Session

A named terminal application that remains available across waiting, input, resizing, log inspection, and visible-screen reads or captures. A session is `running` while accepting input, or `exited` when its application has ended but its final screen remains inspectable until explicitly stopped. Session discovery defaults to running entries and can filter by state, command, or launch working directory. A session retains bounded readable logs and the most recent bounded ANSI/VT transcript bytes; alternate-screen TUIs are read with `show` rather than logs. A session may write a recording timeline while it runs, including viewport resize events. Named CLI sessions retain non-secret launch settings so status can identify them and restart can reuse their command and working directory.

An embedded session owns the same live terminal lifecycle in-process; the named CLI session commands are an adapter for interacting with that lifecycle across invocations.

Named `show` reads are immediate; explicit readiness waits end when their text appears. Quiet-time
settling and input pacing are separate, intentional policies—not mandatory delays between actions.
The TypeScript client's stable captures retain a quiet-period default for snapshot tests; demos
can wait for readiness and explicitly capture immediately.

### Driver

A versioned JSON Lines stdin/stdout adapter over embedded sessions for external agent tooling and the TypeScript test client. A driver process can manage multiple isolated sessions without exposing terminal process details to its client. Its capture response includes the reason capture completed so test clients can distinguish settled screen state from deadline fallback, and can optionally include ANSI or rendered SVG failure evidence.

### Recording

A timestamped terminal event timeline containing output, client or automatic host input, viewport resize events, and named editing markers. A recording can be rendered directly to a realtime video that preserves observed timing or rendered through an explicit edit plan that stitches marker ranges with clip-specific speed, holds, and captions. The source recording remains unchanged and should be treated as potentially sensitive.

Agents inspect marker names with `termctrl markers` and inspect exact recording moments with `termctrl show --recording ... --at-marker ...` or `--at-ms ...` before committing to a video edit plan.

### Mouse Input And Pointer Overlay

Mouse input is a real, typed action (`move`, `down`, `up`, or `click`) at zero-based terminal cell
coordinates. The embedded session validates its current viewport and held button, and Ghostty
encodes the action using the application's negotiated mouse protocol. A `move` without a held
button is a hover; `down`/`move`/`up` is a drag. Disabled reporting fails rather than injecting
escape sequences into an unsuspecting shell. Clicks send press/release together with no added delay.

A pointer overlay is an opt-in video presentation of successfully delivered typed mouse input,
not a terminal cell or the text cursor. Format v2 recordings store each typed mouse event and its
actual bytes on the recording clock; readers retain v1 support. Raw input is not reverse-engineered
into mouse events. Source-time sampling aligns animation with edited video and freezes it during
holds. Reduced motion keeps opacity feedback without travel or press compression. Ordinary
screenshots and screen text do not include the overlay.

### ANSI/VT Stream

Raw terminal output bytes containing text and terminal control sequences. Files commonly use an `.ansi` suffix, but the suffix does not imply a separate container format.

### Semantic Snapshot

Optional application-provided structured UI state read with `show --format semantic`. For applications launched with the OpenTUI host profile, Terminal Control advertises a private `TERMCTRL_SEMANTIC_SOCKET`; a cooperating application may expose one `semantic.snapshot` capability over it. No provider produces an empty snapshot. Semantic state describes interactable application elements and complements, but does not replace, visible terminal evidence from `show`. The OpenTUI adapter package derives snapshots consistently from a live renderer.
