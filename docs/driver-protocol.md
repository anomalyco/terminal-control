# External Driver Protocol

External agent tooling can keep multiple embedded sessions alive through a versioned JSON Lines protocol over stdin/stdout:

```bash
termctrl driver
```

The driver writes a `hello` message with protocol and Terminal Control versions, then accepts typed operations: `launch`, `status`, `send`, `waitForText`, `waitForIdle`, `waitForExit`, `capture`, `logs`, `recording`, `resize`, `stop`, and `shutdown`. It is intended for clients such as the TypeScript test client, while the shell-facing flat commands remain convenient for individual workflows.

Each request has a numeric `id`, a `method`, optional `params`, and a `sessionId` for every method
except `launch` and `shutdown`. Each line receives exactly one response or error with the same `id`:

```json
{"type":"response","id":1,"result":{"sessionId":"app"}}
{"type":"error","id":2,"error":{"code":"REQUEST_FAILED","message":"session app has exited"}}
```

Malformed JSON or request shapes use `INVALID_REQUEST`; input read failures use `READ_ERROR` with a
null `id`; valid operations that fail use `REQUEST_FAILED`. Adding backward-compatible optional
fields does not change the protocol version. Breaking request, response, or lifecycle semantics must
increment `protocolVersion`; clients reject unsupported versions rather than guessing compatibility.

## Example Requests

```json
{"type":"hello","protocolVersion":1,"terminalControlVersion":"<installed-version>"}
{"id":1,"method":"launch","sessionId":"app","params":{"command":["my-terminal-app"],"cols":100,"rows":30,"inheritEnv":false,"env":{"TERM":"xterm-256color"}}}
{"id":2,"method":"waitForText","sessionId":"app","params":{"text":"Ready","timeoutMs":5000}}
{"id":3,"method":"send","sessionId":"app","params":{"input":[{"type":"text","value":"help"},{"type":"key","value":"enter"}]}}
{"id":4,"method":"capture","sessionId":"app","params":{"settleMs":250,"deadlineMs":5000}}
```

## Capture Semantics

A driver `capture` response contains a structured visible frame, derived `text`, and a completion `reason`: `idle`, `deadline`, `exited`, or `outputclosed`. A test client should normally require `idle` or `exited` instead of accepting a deadline fallback as a stable snapshot.

Raw ANSI is omitted by default; request `includeAnsi: true` for retained transcript bytes or `includeSvg: true` for rendered visual evidence.

## Input Semantics

Driver input is intentionally exact: text, raw bytes, known key values, and single-letter control input are supported without claiming unimplemented key chords.
