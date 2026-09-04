---
"@kitlangton/terminal-control": patch
---

Preserve color-query replies when terminal escape prefixes arrive across separate output chunks, exclude invisible text when trimming video startup, and keep the original test failure when automatic failure-artifact capture fails. Simplify shared input encoding, terminal ownership, and recording rendering without changing public interfaces.
