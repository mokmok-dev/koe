# Core architecture

## Crate 構成

```text
crates/
  koe-core/          domain type、state machine、command、event、error
  koe-audio/         CPAL、OS capture、normalization、sync、mix
  koe-model/         Foundry adapter、catalog、cache、ASR session
  koe-recording/     writer、manifest、recovery
  koe-transcript/    segment、timeline、export
  koe-app/           use case、coordinator、policy
apps/
  koe-cli/
  koe-mcp/
```

`koe-core` は CPAL、Foundry、MCP、filesystem implementation に依存しない。
`koe-app` が port trait を所有し、外側の crate が adapter を実装する。

Cargo workspace は library と個別 binary の共有に適する。
[Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html),
[Cargo targets](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#library)

## Coordinator

録音 session と排他的資源は `RecorderCoordinator` task が単一所有する。

```text
UI adapter
  -> bounded command mpsc
  -> RecorderCoordinator
       -> AudioBackend
       -> ModelManager
       -> RecordingStore
       -> TranscriptStore
  <- oneshot command result
  <- broadcast ephemeral events
  <- watch latest snapshot
```

Tokio の mpsc + oneshot、broadcast、watch はこの用途に対応する。
[Tokio sync](https://docs.rs/tokio/latest/tokio/sync/index.html#mpsc-channel)

event の遅延 consumer は durable state を欠落させ得る。UI は event を描画最適化に使い、
再接続時または lag 検出時に `SessionSnapshot` と transcript store を再取得する。

## Session state

```text
Idle
  -> Preparing
  -> PermissionRequired
  -> Starting
  -> Recording
  -> Degraded
  -> Stopping
  -> Finalizing
  -> Completed

任意の非終端状態 -> Failed
Preparing/Starting/Recording/Degraded -> Cancelling -> Cancelled
```

終端遷移は冪等にする。`stop`、`cancel`、runtime error が競合しても writer finalize は
一度だけ実行する。`Cancelled` でも manifest と利用可能な部分録音を残し、policy に
従って削除できる。

## Command

- `ListCapabilities`
- `ListDevices`
- `RequestPermission`
- `ListModels`
- `InstallModel` / `CancelInstall` / `RemoveModel`
- `StartSession`
- `StopSession` / `CancelSession`
- `GetSession`
- `GetTranscript`
- `ExportSession`
- `DeleteSession`
- `Doctor`

各 command は `OperationId`、caller、deadline、cancellation token、audit context を持つ。
CLI と MCP は同じ command を使い、adapter 固有引数を core に追加しない。

## Event

- lifecycle: `StateChanged`
- audio: `SourceStarted`, `Overflow`, `DeviceLost`, `ClockDiscontinuity`
- model: `DownloadProgress`, `Loaded`, `RuntimeFallback`
- transcript: `SegmentProposed`, `SegmentFinalized`
- persistence: `Checkpointed`, `Finalized`
- policy: `ConsentRequired`, `PermissionChanged`

event payload に raw PCM や全文 transcript を既定で含めない。transcript event は
segment ID と必要最小限の text を audience policy に応じて配信する。

## Pipeline と backpressure

```text
CPAL callback
  -> source SPSC ring
  -> normalize/resample
  -> timeline aligner
  -> isolated stem writer queue
  -> mono ASR mixer
  -> VAD/framer
  -> Foundry session queue
  -> transcript assembler
  -> transcript store
```

各 queue は bounded で、容量と overflow policy を config と manifest に残す。
優先順位は次のとおり。

1. callback を block しない。
2. 録音の timestamp と欠落 marker を維持する。
3. ASR feed は古い backlog を無制限に処理しない。
4. UI event は捨てても durable state を失わない。

## Error model

public error は source chain を保持しつつ、UI に安定した code を提供する。

```text
KOE-AUDIO-PERMISSION-DENIED
KOE-AUDIO-DEVICE-LOST
KOE-AUDIO-OVERFLOW
KOE-MODEL-OFFLINE-MISSING
KOE-MODEL-VERIFY-FAILED
KOE-MODEL-UNAVAILABLE
KOE-STORE-PATH-REJECTED
KOE-STORE-FINALIZE-FAILED
KOE-SESSION-CONFLICT
KOE-POLICY-CONSENT-REQUIRED
```

secret、path 全体、audio/transcript 本文は `Display` や default log に含めない。

## Concurrency invariants

- active recording session は初期版では process 全体で 1 件。
- model install と recording は同時に開始しない。
- model removal は load/session/reference が 0 のときだけ許可する。
- source callback は coordinator channel へ直接 await しない。
- writer finalize の完了前に `Completed` を公開しない。
- network policy は session 開始時に freeze し、途中変更しない。

## Dependency policy

workspace dependency を root に集約し、exact compatible range と `Cargo.lock` を commit
する。`unsafe_code = deny` は first-party crate で維持する。native SDK や dependency
内部の unsafe は dependency audit の対象とし、first-party wrapper の trust boundary
を文書化する。
