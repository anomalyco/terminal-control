# Tape Scripting

`termctrl play FILE.tape` runs a deterministic terminal demo from readable source. A tape is distinct
from a `.termctrl` recording: the tape describes intended actions, while a recording stores the timed
terminal events that actually occurred. Video export remains a separate command after playback.

## Execution Contract

`play` reads at most 1 MiB of UTF-8, parses and validates every non-comment line, checks the working
directory, and only then performs setup. It does not partially run a syntactically invalid tape.
A setup, launch, or step failure is reported as `file:line`. The session created by that invocation is
stopped before cleanup runs; if shutdown cannot be confirmed, cleanup is skipped rather than mutating
a fixture underneath a possibly live application. A `Wait` timeout also includes the last visible
screen, truncated for diagnostics.

Relative `Cwd` and `Record` paths resolve from the directory containing the tape. The default working
directory is that directory, and host actions run in the configured working directory. `Record`
creates a private `.termctrl` timeline through the normal session recorder. Run `termctrl video RECORDING
--edit PLAN` explicitly after playback if a video is wanted.

## Syntax

Each command occupies one line. Blank lines and `#` comments are ignored. A comment starts at `#`
outside quotes. Single-quoted text is literal. Double-quoted text supports `\\`, `\"`, `\'`, `\n`,
`\r`, and `\t`; a backslash escapes the next character in unquoted text. Adjacent quoted and
unquoted segments form one argument. Command and option names are case-sensitive.

Durations use an integer followed by `ms`, `s`, or `m`, such as `35ms`, `2s`, or `1m`, and cannot
exceed ten minutes. Zero is accepted only for input pacing.

All header directives must precede `Launch`:

| Directive | Meaning |
| --- | --- |
| `Session NAME` | Required named-session identity. |
| `Viewport COLS ROWS` | Required fixed terminal size in cells. |
| `Cell WIDTH HEIGHT` | Cell size in pixels; defaults to `9 18`. |
| `MaxBytes BYTES` | Retained terminal byte limit; defaults to 16 MiB. |
| `Cwd PATH` | Launch and action working directory; defaults to the tape directory. |
| `Env KEY VALUE` | Add or override one inherited environment value; repeat for multiple keys. |
| `Record FILE.termctrl` | Opt into a recording, resolved from the tape directory. |
| `Pointer on\|off` | Opt into version 2 structured pointer recording; `on` requires `Record`. |
| `Host none\|opentui` | Terminal host profile; defaults to `none`. |
| `Color auto\|always\|never` | Color environment policy; defaults to `auto`. |
| `Setup PROGRAM [ARG ...] [Timeout DURATION]` | Repeatable fixture setup argv, run in source order before `Launch`. |
| `Cleanup PROGRAM [ARG ...] [Timeout DURATION]` | Repeatable fixture cleanup argv, run in reverse order after session stop. |
| `Launch PROGRAM [ARG ...]` | Required application argv; no implicit shell. |

Steps follow `Launch` in order:

| Step | Meaning |
| --- | --- |
| `Wait TEXT [Match MODE] [Timeout DURATION]` | Wait for visible text; MODE is `substring` or `line`, and defaults to substring. Timeout defaults to five seconds. |
| `Type TEXT [Pace DURATION]` | Send text, optionally one Unicode scalar at a time. |
| `Key KEY [KEY ...] [Pace DURATION]` | Send supported named keys in order. |
| `Click X Y` | Send a primary click at a zero-based application cell. |
| `RightClick X Y` | Send a secondary click at a zero-based application cell. |
| `Move X Y [Steps N] [Pace DURATION]` | Move the unpressed pointer; defaults to 10 points at 8 ms. |
| `Drag X1 Y1 X2 Y2 [Steps N] [Pace DURATION]` | Send primary drag events; defaults to 10 steps at 8 ms. |
| `Mark NAME` | Add a unique recording marker; requires `Record`. |
| `Sleep DURATION` | Hold the presentation deliberately without checking state. |
| `Action PROGRAM [ARG ...] [Timeout DURATION]` | Run an in-session host action using exact argv. |
| `Stop` | Required final command; stop the owned named session cleanly. |

`Type` and `Key` accept the same input parser as `termctrl send`, including modifier chords such as
`shift+enter`. `Click`, `RightClick`, `Move`, and `Drag`
reuse the normal SGR mouse implementation, so the application must enable the corresponding mouse
tracking. Coordinates are application-cell coordinates, not screen pixels. The first `Move`
establishes the tape pointer position with one honest no-button motion event; it does not invent a
path from an unknown origin. Later Moves interpolate `Steps` events from the authoritative position,
including the destination. Click and RightClick set the position to their target and Drag sets it to
its endpoint. Every endpoint is checked against the declared `Viewport` while the complete tape is
validated, before setup or launch.

`Wait` searches for a substring by default. Add `Match line` for equality with one complete visible
terminal row after trailing cell padding is removed. This prevents `Wait "history entry 1" Match line`
from succeeding on `history entry 10`. `Match substring` is an explicit spelling of the default;
`Match` and `Timeout` modifiers may appear in either order.

With `Pointer on`, clicks, move, and drag also produce structured press, move, and release events in
an opt-in version 2 recording. RightClick events include `button: "secondary"`; the optional field is
omitted for primary events. Render them with `save --recording ... --pointer` or `video --pointer`.
Without that directive, tape recordings remain version 1 and existing Click/Drag behavior is
unchanged; Move still sends its explicit no-button input to the application. `Pointer on` controls
capture only. Bare `--pointer` uses the fading renderer, while `--pointer=persistent` keeps the most
recent event visible without prolonging press or click feedback. No overlay exists before the first
event.

`Setup`, `Action`, and `Cleanup` have a finite 30-second default timeout. A trailing
`Timeout DURATION` overrides it. Each action starts in its own Unix process group; timeout terminates
the group, and both stdout and stderr are drained with bounded diagnostic retention and a finite
grace. If an escaped process retains either pipe after that grace, the action fails and owned-session
cleanup proceeds; processes that deliberately create a new Unix session are outside the action's
owned process group and may remain. Successful actions must exit zero. Cleanup runs even after setup,
launch, or playback failure when no live owned session remains, and all cleanup entries are attempted
in reverse declaration order. Cleanup errors
follow rather than replace the primary error. Write cleanup actions to tolerate partially completed
setup and repeated use.

Successful `play --json` receipts require UTF-8 tape and recording paths. On Unix, an incompatible
path is rejected after complete source validation but before Setup or Launch, so receipt encoding
cannot fail after lifecycle side effects.

## Example

```text
# demos/ttt.tape
Session ttt-demo
Viewport 80 24
Cell 9 18
Cwd ".."
Env TTT_SEED "fixed-demo"
Record "captures/ttt.termctrl"
Pointer on
Color always
Setup "/usr/bin/cp" "fixtures/clean.json" "fixtures/game.json" Timeout 5s
Cleanup "/usr/bin/rm" "-f" "fixtures/game.json"
Launch "cargo" "run" "--release" "--bin" "ttt"

Wait "Choose a square" Timeout 10s
Move 8 8
Move 12 8 Steps 8 Pace 16ms
Click 12 8
RightClick 20 8
Wait "Your turn"
Type "middle" Pace 35ms
Key shift+enter
Action "/usr/bin/touch" "fixtures/opponent-ready" Timeout 2s
Wait "You won"
Mark result
Sleep 750ms
Stop
```

On success, `termctrl play FILE.tape` prints the canonical tape path and recording path, if any.
Use `--json` for the stable fields `status`, `tape`, `session`, and `recording`, or `--quiet` when a
caller requires empty stdout.

Host actions intentionally support external fixture mutation without shell parsing. They can still
run arbitrary executables, so treat tapes as executable project code. If shell behavior is genuinely the
application under demonstration, make that choice explicit in argv, for example `Launch /bin/sh -c
"..."`; `play` itself never joins arguments or evaluates them through a shell.
