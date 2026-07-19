# Workspace Rendering

## Goal

Keep interactive shell and TUI input visibly responsive without full-screen flashing.

## Scenario

- Outer named Terminal Control session at 160x44.
- Inner native workspace split into a zsh pane and an OpenCode pane.
- Agent input targets the OpenCode pane while the outer session waits for the same text.
- One warmup followed by nine measured runs.

## Metrics

| Metric | Full repaint | Row-diff compositor |
| --- | ---: | ---: |
| Composed ANSI frame bytes | 166,103 | 15,353 |
| Incremental prompt-edit bytes | 166,103 | 494 |
| Input-to-visible median | 2,134.0 ms | 43.1 ms |
| Unchanged 160x44 frame snapshot | 592.0 us | 216.5 us |

The kept implementation combines synchronized output, changed-row rendering, style batching, and
a retained Ghostty render state that merges dirty rows into complete cached frames.
The byte counts are deterministic for the captured scenario; latency includes the CLI process used
to send input and the CLI process waiting on the outer session.
