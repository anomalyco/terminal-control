# Tape Scripting

`termctrl play FILE.tape` runs a deterministic terminal demo from readable source. A tape is distinct
from a `.termctrl` recording: the tape describes intended actions, while a recording stores the timed
terminal events that actually occurred. Video export remains a separate command after playback.

## Execution Contract

`play` reads at most 1 MiB of UTF-8, parses and validates every non-comment line, checks the working
directory, and only then launches the named session. It does not partially run a syntactically invalid
tape. A launch or step failure is reported as `file:line`, and the session created by that invocation
is stopped. A `Wait` timeout also includes the last visible screen, truncated for diagnostics.

Relative `Cwd` and `Record` paths resolve from the directory containing the tape. The default working
directory is that directory, and `Action` runs in the configured working directory. `Record` creates
a private `.termctrl` timeline through the normal session recorder. Run `termctrl video RECORDING
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
| `Host none\|opentui` | Terminal host profile; defaults to `none`. |
| `Color auto\|always\|never` | Color environment policy; defaults to `auto`. |
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
| `Action PROGRAM [ARG ...]` | Run a host process using exact argv, tape cwd, and tape environment. |
| `Stop` | Required final command; stop the owned named session cleanly. |

`Type` and `Key` accept the same named-key vocabulary as `termctrl send`. `Click` and `Drag` reuse
the normal SGR mouse implementation, so the application must enable mouse tracking. Coordinates are
application-cell coordinates, not screen pixels.

## Example

```text
# demos/ttt.tape
Session ttt-demo
Viewport 80 24
Cell 9 18
Cwd ".."
Env TTT_SEED "fixed-demo"
Record "captures/ttt.termctrl"
Color always
Launch "cargo" "run" "--release" "--bin" "ttt"

Wait "Choose a square" Timeout 10s
Click 12 8
Wait "Your turn"
Type "middle" Pace 35ms
Key enter
Action "/usr/bin/touch" "fixtures/opponent-ready"
Wait "You won"
Mark result
Sleep 750ms
Stop
```

`Action` intentionally supports external fixture mutation without shell parsing. It can still run an
arbitrary executable, so treat tapes as executable project code. If shell behavior is genuinely the
application under demonstration, make that choice explicit in argv, for example `Launch /bin/sh -c
"..."`; `play` itself never joins arguments or evaluates them through a shell.
