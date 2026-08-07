# koe MCP stdio server

`koe-mcp` is a local subprocess server. It does not expose HTTP and never writes
logs or application output to stdout; stdout is reserved for newline-delimited
MCP JSON-RPC messages.

Example host configuration:

```text
koe-mcp --data-root /absolute/app-owned/koe --export-root /absolute/approved/exports
```

Run it as a dedicated least-privilege user where possible. Restrict its
filesystem view to the configured roots and deny outbound network access during
recording and inference. Model installation is the only operation that may use
the network and still requires fresh `consent: true` in that tool call. Omit
`--export-root` to disable MCP exports.

Sensitive calls (recording, stopping/cancelling, transcript/session exposure,
export, and deletion) require `consent: true` on every call. A model/tool
description is not consent. Session paths are UUID-derived beneath the fixed
data root; arbitrary path and URL parameters are intentionally unsupported.

The server limits requests to 1 MiB and responses to 4 MiB. One recording or
model installation may be active at a time, and terminal operation history is bounded. EOF cooperatively cancels and finalizes
all recordings, preventing detached/orphan recording. Normal filesystem deletion
does not guarantee physical media erasure.
