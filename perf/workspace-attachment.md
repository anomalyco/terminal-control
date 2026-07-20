# Workspace Attachment

## Goal

Keep persistent workspace creation, detachment, and reattachment visibly immediate while the daemon
continues pumping panes and serving agent controls.

## Benchmark

```bash
cargo build --release --bin termctrl
TERMCTRL_BIN=./target/release/termctrl bun scripts/bench-workspace-attach.ts
```

The benchmark performs one warmup and nine measured reattachments at `160x44`. Each measurement
starts a fresh terminal client, waits for the first complete shell frame, then disconnects while
preserving the workspace daemon.

## Primary Metric

- `workspace_attach_median_ms`: create attachment client to first visible frame.

## Secondary Metrics

- `workspace_attach_mad_ms`: median absolute deviation across measured runs.
- Detached steady-state composition: zero frames unless output or process lifecycle changes.
- Attachment output backpressure: bounded at 250 ms before detaching the stalled client.

## Hypothesis Loop

1. Measure the current callback handshake and full repaint.
2. Instrument only when the median or spread identifies a real boundary.
3. Change one boundary at a time and keep only durable wins outside the noise floor.

## Correctness Guardrails

- Pane processes and IDs survive every measured detach.
- A second human attachment is rejected.
- Reattachment applies current size and theme before its first repaint.
- Agent control remains available while detached.
