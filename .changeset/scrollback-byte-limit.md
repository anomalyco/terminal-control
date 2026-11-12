---
"@kitlangton/terminal-control": patch
---

Retain about 15,000 rows of 120-column readable `logs` scrollback instead of a few hundred. Ghostty measures the scrollback limit in bytes, so the previous limit of 10,000 kept only Ghostty's minimum page allocation.
