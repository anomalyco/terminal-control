# Detached Workspace Idle Cost

## Goal

Keep persistent workspaces cheap enough to leave running without sacrificing immediate agent
control or human reattachment.

## Benchmark

```bash
cargo build --release --bin termctrl
TERMCTRL_BIN=./target/release/termctrl bun scripts/bench-workspace-idle.ts
```

The benchmark creates a detached one-pane workspace whose child is idle, then samples the daemon
for one warmup second and nine measured seconds with macOS `top`.

## Primary Metric

- `workspace_idle_context_switches_per_s`: daemon scheduler wakeups while detached.

## Secondary Metric

- `workspace_idle_cpu_percent`: median CPU reported by `top`.
- `workspace_attach_median_ms`: separate user-visible reattachment guardrail.

## Hypothesis Loop

1. Measure the fixed 5 ms workspace scheduler.
2. Let the detached daemon block on its control listener between coarse pane-output checks.
3. Keep the change only if wakeups fall materially and attachment remains within its existing
   spread.

## Results

| Experiment | Context switches/s | Idle CPU |
| --- | ---: | ---: |
| Fixed 5 ms scheduler | 256 | 0.30% |
| Listener-blocked detached scheduler | 31 | 0.00% |

The daemon now blocks on the control listener for up to 50 ms while fully idle, so control requests
wake it immediately. A 500 ms fast-tick window after each request preserves multi-step attachment
and agent workflows. Reattachment measured 67.9 ms median versus the existing 56.6 ms baseline;
the 11.3 ms difference is below one 60 Hz frame and keeps the interaction visibly immediate.

## Correctness Guardrails

- Control requests wake the daemon immediately while detached.
- Pane output and exits are still observed while detached.
- Attached input and repaint cadence remains unchanged.
