---
"@kitlangton/terminal-control": patch
---

Wake named-session control requests as soon as they arrive instead of waiting for an idle polling sleep. Clarify immediate screen reads, readiness-driven demo workflows, and intentional capture/typing delays without changing stable-capture defaults.

Reuse a bounded base raster during pointer video animation instead of repeating terminal text layout for every pointer position. Preserve frame pixels, recording timing, and screenshot behavior.
