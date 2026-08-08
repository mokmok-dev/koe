# 実装ロードマップ

## Milestone 0: Spike と dependency pin

成果物:

- CPAL で各 OS の microphone と system audio capability probe
- Foundry Rust sample を pinned commit/version で build
- Nemotron catalog availability と variant/provider inventory
- license と redistribution の記録

受入条件:

- 各 OS で supported/unsupported/permission-required を machine-readable に出せる
- model install -> load -> short PCM -> transcript -> unload -> offline repeat が成功する
- Foundry download integrity の確認可能範囲を文書化する

未達なら interface は維持し、unsupported backend/model を capability として扱う。

## Milestone 1: Microphone recording CLI

crate:

- `koe-core`
- `koe-audio`
- `koe-recording`
- `koe-app`
- `koe-cli`

機能:

- device list
- microphone capture
- bounded callback handoff
- segmented WAV
- checkpoint/finalize/recovery
- session manifest
- cooperative Ctrl-C

受入条件:

- 1 時間録音で crash せず、欠落 metric が取得できる
- device loss、overflow、writer failure が安定 error code になる
- forced crash 後に最後の checkpoint まで recovery できる

## Milestone 2: System audio と同期

機能:

- Windows WASAPI loopback
- macOS 14.6+ process tap
- Linux PipeWire sink、PulseAudio monitor fallback
- isolated stems と mixed track
- drift estimation/resampling、gap marker
- permission UX

受入条件:

- OS HIL で既知 test signal の capture を検証
- mic/system の長時間 drift が測定される
- unsupported/DRM/portal denial を failure と誤認しない

## Milestone 3: Foundry Local と offline ASR

crate:

- `koe-model`
- `koe-transcript`

機能:

- list/resolve/install/cancel/load/unload/remove
- model manifest、digest inventory、license display
- PCM16 mono 16 kHz adapter
- Nemotron streaming session
- transcript JSONL/final materialization
- strict offline network policy

受入条件:

- online install 後、outbound block 環境で E2E transcription が成功
- cache missing 時に network access せず明示 error
- active model removal と version switch を拒否
- chunk size ごとの latency/WER/RTF baseline を保存

## Milestone 4: CLI reference product

機能:

- complete command surface
- JSON/JSONL contract
- session list/show/export/delete
- `doctor`
- config migration と retention

受入条件:

- scripted E2E が 3 OS で再現可能
- stdout/stderr contract が snapshot test 済み
- audio/transcript が default log に現れない

## Milestone 6: MCP stdio

機能:

- capability/device/model/session tools
- operation/progress/cancellation
- resource-based transcript access
- per-tool consent と path/session authorization
- stdio sandbox guidance

受入条件:

- stdout に MCP 外 byte を出さない
- untrusted path、oversized request、orphan recording を防ぐ
- recording/data exposure/delete の consent test を通る
- cancellation race が idempotent な終端状態になる

## Milestone 7: Production distribution

機能:

- macOS signing/notarization/stapling
- Windows signing/timestamp/package
- Linux package/portal integration
- SBOM、notices、attestation
- signed update metadata と rollback
- release HIL matrix

受入条件:

- clean machine install/uninstall
- offline recording/transcription
- update、rollback、expired metadata、tampered artifact test
- privacy/security/release checklist 完了

## 優先リスク

1. Foundry Local が preview で API、catalog、alias が変化する。
2. model/provider artifact の publisher verification が公開 API で確定しない。
3. cross-stream clock origin と device drift は application 補正が必要。
4. macOS system audio は 14.6+ で、旧 OS は同一機能を提供できない。
5. Linux desktop/sandbox ごとの PipeWire portal behavior が異なる。
6. Nemotron の日本語品質、CPU-only RTF、memory は未測定。
7. 長時間 WAV、RF64、segment rotation の最適値は未確定。

## 決定前の open question

- 最低 OS version と support tier
- multilingual Nemotron を初期 default にするか
- transcript の対象言語と WER/CER corpus
- default chunk size
- isolated stem を default 保存するか
- application-level encryption を初期 scope に含めるか
- Linux package format
- Foundry runtime を同一 process に置くか child process に分離するか

open question は Milestone 0/1 の測定結果を ADR として記録して閉じる。
