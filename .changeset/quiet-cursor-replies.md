---
"@kitlangton/terminal-control": patch
"@kitlangton/terminal-control-opentui": patch
"@kitlangton/terminal-control-darwin-arm64": patch
"@kitlangton/terminal-control-darwin-x64": patch
"@kitlangton/terminal-control-linux-arm64-gnu": patch
"@kitlangton/terminal-control-linux-x64-gnu": patch
---

Answer cursor-position queries with the actual query-time cursor position when using the OpenTUI host profile, including split and repeated queries, without injecting unsolicited startup cursor reports.
