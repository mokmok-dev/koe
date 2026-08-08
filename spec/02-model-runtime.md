# Model runtime と lifecycle

## 目的

Foundry Local の API 変化、catalog、hardware variant を `koe-model` 内へ隔離し、
application からは model の取得と ASR session の lifecycle だけを見せる。

## 状態機械

```text
Absent
  -> Resolving
  -> Downloading
  -> Verifying
  -> Installed
  -> Loading
  -> Ready
  -> InUse
  -> Unloading
  -> Installed
  -> Removing
  -> Absent
```

各操作は cancellation token と progress event を受け取る。中断された download は
`Installed` に遷移させず、staging directory を cleanup または quarantine する。
既存 cache の force-redownload は usable artifact を先に削除してはならない。公式 SDK
1.2.3 には atomic force/replace option がないため、Foundry adapter は既存 cache を保持して
`KOE-MODEL-FORCE-REDOWNLOAD-UNSUPPORTED` を返す。
`remove` は active session、loaded model、参照中 recording job がある間は拒否する。

## 抽象 API

```rust
trait ModelManager {
    async fn list(&self, scope: ModelScope) -> Result<Vec<ModelDescriptor>, ModelError>;
    async fn resolve(&self, selector: ModelSelector) -> Result<ModelDescriptor, ModelError>;
    async fn install(&self, model: ModelId, options: InstallOptions)
        -> Result<InstalledModel, ModelError>;
    async fn load(&self, model: InstalledModelId) -> Result<LoadedModel, ModelError>;
    async fn unload(&self, model: LoadedModelId) -> Result<(), ModelError>;
    async fn remove(&self, model: InstalledModelId) -> Result<(), ModelError>;
}

trait StreamingAsrSession {
    async fn append(&mut self, chunk: Pcm16Mono16k) -> Result<(), AsrError>;
    async fn finish(self) -> Result<FinalTranscript, AsrError>;
}
```

Rust では async trait の具体化、ownership、Foundry handle の `Send`/`Sync` 制約を
確認して signature を確定する。Foundry object を public type に含めない。

## Foundry Local adapter

公式 Rust sample は英語向け
`nemotron-speech-streaming-en-0.6b` と多言語向け
`nemotron-3.5-asr-streaming-0.6b` を示し、model 検索、cache 確認、download、
load、PCM append、result stream、stop、unload まで実行する。
[Foundry Local Rust live sample](https://github.com/microsoft/Foundry-Local/blob/b4ca39fcb4cc90aaea6f6e89e6665f9577e69855/samples/rust/live-audio-transcription/src/main.rs#L14-L316)

adapter は次の SDK 操作を対応付ける。

| koe | Foundry Local |
| --- | --- |
| `resolve` | catalog `get_model` / `get_model_variant` |
| `list(Installed)` | `get_cached_models` |
| `list(Loaded)` | `get_loaded_models` |
| `install` | download builder + progress + cancel |
| `load` / `unload` | model load / unload |
| `remove` | `remove_from_cache` |
| `latest` | `get_latest_version` |

[Foundry catalog](https://github.com/microsoft/Foundry-Local/blob/b4ca39fcb4cc90aaea6f6e89e6665f9577e69855/sdk/rust/src/catalog.rs#L181-L413),
[Foundry model API](https://github.com/microsoft/Foundry-Local/blob/b4ca39fcb4cc90aaea6f6e89e6665f9577e69855/sdk/rust/src/detail/model.rs#L44-L470)

## Offline contract

`NetworkPolicy` は application core の明示値にする。

- `Denied`: cache と local runtime だけを利用し、catalog refresh、download、
  update check を実行しない。
- `ModelInstallOnly`: user が指定した install/update operation の間だけ許可する。
- `Allowed`: 将来用。録音 session の既定にはしない。

`Denied` で必要 artifact がない場合は `OfflineArtifactMissing` を返す。暗黙に network
へ fallback しない。offline E2E test は DNS failure ではなく OS sandbox/firewall
で outbound を遮断して実施する。

Foundry Local は初回取得後の model が cache から entirely offline で動くとしている。
[Foundry catalog architecture](https://learn.microsoft.com/en-us/azure/foundry-local/concepts/foundry-local-architecture#foundry-catalog)

## Model manifest

Foundry catalog metadata と koe 独自情報を immutable manifest に保存する。

```json
{
  "schema_version": 1,
  "model_id": "catalog stable id",
  "alias": "nemotron-3.5-asr-streaming-0.6b",
  "version": "catalog version",
  "variant": "resolved hardware variant",
  "provider": "resolved execution provider",
  "license_id": "model-specific",
  "source": "catalog URI",
  "files": [{"path": "relative/path", "sha256": "...", "size": 0}],
  "installed_at": "RFC3339",
  "foundry_version": "pinned adapter version",
  "verification": "verified|runtime-only|quarantined"
}
```

Foundry SDK、CLI、model は license が異なるため、同一 license として扱わない。
[Foundry license](https://github.com/microsoft/Foundry-Local#license)

公開資料だけでは Foundry Core が download 時に行う signature/checksum 検証を確定
できない。初期版では次を実施する。

1. HTTPS と Foundry runtime の標準 download を使用する。
2. install 完了後に全 file の SHA-256 と size を manifest に記録する。
3. app が管理する allowlist がある release は期待 digest と照合する。
4. digest 不一致、unknown file、path escape を quarantine する。
5. allowlist のない artifact は UI に `runtime-only verification` と表示する。

これは publisher authenticity の完全な代替ではない。upstream が署名情報を公開した
時点で signature verification を追加する。

## ASR session

canonical input は `Pcm16Mono16k`。live API の既定も 16 kHz、mono、16-bit である。
送信 queue は Foundry SDK 内部だけに依存せず koe 側でも bounded にする。
[Foundry live session](https://github.com/microsoft/Foundry-Local/blob/b4ca39fcb4cc90aaea6f6e89e6665f9577e69855/sdk/rust/src/openai/live_audio_session.rs#L1-L708)

`is_final` は model-neutral event に保持するが、現行 Nemotron は常に final とされる。
SDK response に ID がない場合も final まで同一 fallback segment ID を保持し、timestamp が
省略された revision は同じ segment の直前 bounds を再利用する。`chunk_ms` は SDK 内部の
rechunk option ではなく caller-side の目標 duration と 1 append の上限であり、caller が
16 kHz PCM をその長さに分割する（最後の短い chunk は許可）。session settings は
`chunk_ms=1..=60000`、`push_queue_capacity=1..=4096` とし、違反や上限超過 append は typed
input/settings error にする。partial の置換を前提とした UI にしない。chunk は
80/160/560/1120 ms を設定候補とし、
既定値は benchmark 後に決める。公開 model card では chunk 増加に伴う WER 改善が
報告されている。
[Nemotron performance](https://huggingface.co/nvidia/nemotron-speech-streaming-en-0.6b#performance)

## Version と update

- app release は検証済み Foundry SDK version と最低 runtime version を pin する。
- alias だけで再現性を保証せず、session manifest へ model ID/version/variant を記録する。
- `check-update` と `install-update` を分離する。
- 新 version は side-by-side install し、load smoke test 成功後に default を切り替える。
- rollback 用に直前 version を保持し、active session 中に切り替えない。

## Fallback

Nemotron live が catalog、OS、hardware で利用不能なら capability として報告する。
whole-file transcription は Whisper native audio API を別 capability として提供できるが、
live Nemotron と同一 session API の挙動を仮定しない。
[Foundry native audio transcription](https://learn.microsoft.com/en-us/azure/foundry-local/reference/reference-sdk-current#native-audio-transcription-api-3)
