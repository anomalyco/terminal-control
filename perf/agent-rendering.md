# Agent latency and video rendering

Goal: shorten verified input → screen-read cycles and video exports without weakening readiness,
changing input timing, or changing rendered pixels. Use a release binary, not debug-build timings.

```sh
PATH=/opt/homebrew/opt/zig@0.15/bin:$PATH cargo build --release
bun scripts/bench-performance.ts --mode all --runs 7 --out-dir /path/to/results
```

The benchmark performs one warmup and seven measured runs per case; JSON Lines report medians,
median absolute deviations, and raw samples. Use `--binary` to compare saved builds, and `--mode
agent` or `--mode video` for individual experiments. Fixtures and isolated named sessions are
cleaned up; only synthetic videos remain when `--out-dir` is specified. `ffmpeg` is required.

## What is measured

- CLI `show` on a populated 80×24 real PTY.
- CLI `send` → literal readiness `wait` → `show`, asserting the newly acknowledged input.
- Persistent TypeScript driver input → literal readiness → immediate capture.
- Two-second 80×24, 60fps, 2× video exports: plain, moving pointer, pointer with footer.
  Rendering-stage times use the existing stderr progress boundaries; total includes ffmpeg.

No arbitrary sleeps, changed default settling, or reduced frame rate are used to improve scores.
Application startup, external model/tool-call latency, and network package installation are not
part of these metrics. Results are machine/workload-specific, not performance guarantees.

## Experiments

1. **Named daemon wakeup:** replace the unconditional 10ms idle sleep with `poll` on the listener.
   Requests wake immediately; the same maximum idle interval still pumps PTY/semantic output.
  Baseline: CLI show 14.29ms, CLI interaction 37.88ms (local macOS arm64).
  The persistent-driver interaction is already 5.88ms; its worker's `recv_timeout` wakes for
  requests, unlike the daemon's unconditional sleep. Do not replace that with busy polling.
   Retained: 15-run control/candidate comparison was 13.75 → 5.40ms for show and
   37.48 → 10.90ms for interaction (61% / 71% lower). All 23 session tests and clippy passed.
   The first 7-run candidate gave 4.59ms / 10.56ms; the result exceeds observed scheduling noise.

2. **Repeated terminal rasterization:** a changed pointer currently invalidates the whole rendered
   screen, repeating SVG text layout and rasterization. Try one bounded cached base raster, painting
   the pointer over a copy. Preserve the existing full-image cache and exact output pixels.
   Baseline: 21.27s total / 20.39s rendering with pointer; 19.85s / 19.47s with pointer and footer.
   Plain export: 0.88s total / 0.37s rendering. A two-second CPU sample during active rendering
   placed most samples in SVG/text conversion. Pending candidate measurement and pixel comparison.

## Guardrails / next candidates

- Keep explicit quiet periods and input pacing. The TypeScript stable-capture defaults are a test
  contract, not an accidental sleep; readiness-driven demos can explicitly capture immediately.
- Keep Ghostty state confined to the session thread.
- Do not remove mouse capability checks or legacy daemon compatibility for one fewer round trip.
- Inspect long-transcript screen reads separately: the named protocol still carries retained ANSI
  even for text-only consumers. A selective response needs an additive compatibility design.
- PNG intermediate files and ffmpeg encoding may dominate after raster reuse. Measure before
  changing the export pipeline or trading disk traffic for raw pixel pipe bandwidth.
