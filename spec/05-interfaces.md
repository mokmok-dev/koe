# CLI、MCP

## 共通方針

各 interface は `koe-app` の同一 command、snapshot、event API を利用する。adapter 自身が
CPAL stream、Foundry model handle、WAV writer を所有しない。

長時間 operation は `OperationId` を返し、進捗取得、cancel、完了確認を共通化する。
error は安定 code、短い user message、任意の診断 ID を返す。

## CLI

想定 command:

```text
koe capabilities
koe devices list [--source mic|system]
koe permissions status
koe models list [--installed|--loaded]
koe models install <selector>
koe models remove <id>
koe record --mic <id> [--system <id>] --model <id> --output <dir>
koe sessions list
koe sessions show <id>
koe sessions export <id>
koe sessions delete <id>
koe doctor
```

規則:

- human-readable output と `--output-format json|jsonl` を分ける。
- progress bar は TTY のみ。machine mode は stderr へ structured progress を出す。
- audio/transcript data を stdout へ流す command は明示 option がある場合だけ。
- `record` の Ctrl-C は cooperative stop と finalize を行う。2 回目は cancel を要求する。
- password や token を command line argument で受け取らない。

CLI は最初の vertical slice と reference behavior にする。

## MCP stdio

初期版は remote HTTP を提供せず stdio transport のみとする。MCP stdio は client が
server subprocess を起動し、stdin/stdout で newline-delimited JSON-RPC を交換する。
stdout に protocol 外の output を書かず、diagnostic は stderr へ送る。
[MCP stdio transport](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#stdio)

想定 tool:

- `koe_capabilities`
- `koe_list_devices`
- `koe_list_models`
- `koe_install_model`
- `koe_start_recording`
- `koe_stop_recording`
- `koe_get_operation`
- `koe_get_session`
- `koe_get_transcript`
- `koe_export_session`
- `koe_delete_session`

想定 resource:

- `koe://capabilities`
- `koe://operations/{id}`
- `koe://sessions/{id}`
- `koe://sessions/{id}/transcript`

録音開始、model download、transcript/audio の返却、delete は sensitive operation
として host 側の明示同意を要求する。tool description だけを同意とみなさない。
[MCP security principles](https://modelcontextprotocol.io/specification/2025-03-26/index#security-and-trust-safety)

## Progress と cancellation

MCP progress token は active request 間で一意にし、値は単調増加、完了後は停止する。
[MCP progress](https://modelcontextprotocol.io/specification/2025-06-18/basic/utilities/progress#progress-flow)

cancel は cooperative とする。

- cancel receipt を idempotent に記録
- callback と writer の停止
- model append の停止
- partial artifact の checkpoint/finalize
- `Cancelled` または race で既に完了した `Completed` を返す

request timeout と operation lifetime を分離する。MCP request が timeout しても
明示 policy なしに録音を orphan にせず、start request 時に `detach=false` を既定にする。
[MCP cancellation](https://modelcontextprotocol.io/specification/2025-06-18/basic/utilities/cancellation#behavior-requirements),
[MCP lifecycle timeout](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle#timeouts)

## Process topology

初期版:

- CLI: 1 process
- MCP: 1 stdio server process
- 同時に複数 frontend が同じ session store を書く構成は非対応

Foundry native runtime の crash、GPU conflict、redistribution 条件を prototype で評価後、
必要なら `koe-daemon` と private authenticated IPC を導入する。最初から daemon を
前提にして複雑化しない。
