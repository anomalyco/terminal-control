---
"@kitlangton/terminal-control": patch
"@kitlangton/terminal-control-opentui": patch
"@kitlangton/terminal-control-darwin-arm64": patch
"@kitlangton/terminal-control-darwin-x64": patch
"@kitlangton/terminal-control-linux-arm64-gnu": patch
"@kitlangton/terminal-control-linux-x64-gnu": patch
---

Detach background session daemons from their launcher's process group so named sessions started or restarted with the CLI survive launcher-group hangups.
