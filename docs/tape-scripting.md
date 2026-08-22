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
| `Wait TEXT [Timeout DURATION]` | Wait for visible text; timeout defaults to five seconds. |
| `Type TEXT [Pace DURATION]` | Send text, optionally one Unicode scalar at a time. |
| `Key KEY [KEY ...] [Pace DURATION]` | Send supported named keys in order. |
| `Click X Y` | Send a primary click at a zero-based application cell. |
| `Drag X1 Y1 X2 Y2 [Steps N] [Pace DURATION]` | Send primary drag events; defaults to 10 steps at 8 ms. |
| `Mark NAME` | Add a unique recording marker; requires `Record`. |
| `Sleep DURATION` | Hold the presentation deliberately without checking state. |
| `Action PROGRAM [ARG ...] [Timeout DURATION]` | Run an in-session host action using exact argv. |
| `Stop` | Required final command; stop the owned named session cleanly. |

`Type` and `Key` accept the same named-key vocabulary as `termctrl send`. `Click` and `Drag` reuse
the normal SGR mouse implementation, so the application must enable mouse tracking. Coordinates are
application-cell coordinates, not screen pixels.

With `Pointer on`, click and drag also produce structured press, move, and release events in an
opt-in version 2 recording. Render them with `save --recording ... --pointer` or `video --pointer`.
Without that directive, tape recordings remain version 1 and render exactly as before.

`Setup`, `Action`, and `Cleanup` have a finite 30-second default timeout. A trailing
`Timeout DURATION` overrides it. Each action starts in its own Unix process group; timeout terminates
the group, and both stdout and stderr are drained with bounded diagnostic retention. Successful
actions must exit zero. Cleanup runs even after setup, launch, or playback failure when no live owned
session remains, and all cleanup entries are attempted in reverse declaration order. Cleanup errors
follow rather than replace the primary error. Write cleanup actions to tolerate partially completed
setup and repeated use.

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
Click 12 8
Wait "Your turn"
Type "middle" Pace 35ms
Key enter
Action "/usr/bin/touch" "fixtures/opponent-ready" Timeout 2s
Wait "You won"
Mark result
Sleep 750ms
Stop
```

Host actions intentionally support external fixture mutation without shell parsing. They can still
run arbitrary executables, so treat tapes as executable project code. If shell behavior is genuinely the
application under demonstration, make that choice explicit in argv, for example `Launch /bin/sh -c
"..."`; `play` itself never joins arguments or evaluates them through a shell.
