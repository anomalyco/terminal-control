# @kitlangton/terminal-control-opentui

## 1.2.1

## 1.2.0

## 1.1.1

### Patch Changes

- 96c90e6: Detach background session daemons from their launcher's process group so named sessions started or restarted with the CLI survive launcher-group hangups.
- 4de9fec: Answer cursor-position queries with the actual query-time cursor position when using the OpenTUI host profile, including split and repeated queries, without injecting unsolicited startup cursor reports.

## 1.1.0

## 1.0.0

## 0.6.0

### Minor Changes

- 5a0a277: Create a private application semantic socket for commands launched with the OpenTUI host profile and add `show --format semantic` for optional application-provided UI snapshots.
