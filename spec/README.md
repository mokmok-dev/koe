# koe 実装仕様

更新日: 2026-08-04

koe は、マイク入力とシステム音声を録音し、取得済みのローカル ASR
モデルで完全オフライン文字起こしを行うクロスプラットフォーム Rust
アプリケーションである。利用形態として CLI、GPUI デスクトップアプリ、
MCP stdio サーバーを提供する。

## 結論

実装は可能である。ただし「クロスプラットフォーム」は全 OS で同一の音声
API を使うことを意味しない。CPAL の共通 API と OS 固有 backend を
`AudioSource` の内側へ隠し、実行時 capability detection で差を表現する。

Foundry Local は Rust SDK と Nemotron の live transcription sample を提供し、
モデルの検索、download、load、session、unload、cache removal を実行できる。
初回取得後の推論はローカル cache からオフラインで実行できる。一方、download
artifact の暗号学的検証方法は公開資料だけでは確定できないため、preview 段階では
信頼境界として明示し、独自 manifest と digest allowlist を重ねる。

## 仕様一覧

| 文書 | 内容 |
| --- | --- |
| [01-audio-capture.md](01-audio-capture.md) | マイク、システム音声、OS backend、同期 |
| [02-model-runtime.md](02-model-runtime.md) | Foundry Local、Nemotron、model lifecycle |
| [03-core-architecture.md](03-core-architecture.md) | crate、trait、状態機械、realtime pipeline |
| [04-storage-and-transcripts.md](04-storage-and-transcripts.md) | 録音、manifest、transcript、障害復旧 |
| [05-interfaces.md](05-interfaces.md) | CLI、GPUI、MCP |
| [06-security-and-privacy.md](06-security-and-privacy.md) | 脅威モデル、権限、保存、supply chain |
| [07-testing-and-distribution.md](07-testing-and-distribution.md) | テスト matrix、CI、署名、配布 |
| [08-roadmap.md](08-roadmap.md) | 段階的実装、受入条件、未解決事項 |

## Milestone 7 の実装状況（Production distribution candidate）

実装と fixture/hosted 検証は存在するが、Production 完了判定は 5 台の実機 HIL、
signing/notarization、checklist 記録後にのみ行う。`koe-update` を追加し、TUF 風の署名付き update metadata（role/version/expiry/`platform`/hash-bound target）と回帰安全な store を実装した。

- `koe-release-sign` (bin) が release artifact directory を SHA-256/size で hash し、署名 metadata 内の単一 `install_target` を指定する。`koe update apply` は binary に埋め込んだ publisher public key（通常操作で差替不可）で検証して side-by-side に install し、`koe update launch` が署名・binding・at-rest digest を再検証して active executable を起動する。rollback も同じ検証後に previous executable を再選択する。
- expired / replay / tampered / foreign-platform / unsupported-schema はすべて安定 error code（`KOE-UPDATE-*`）で拒否し、quarantine note を残す。store は network を触らない。
- CLI は `koe update status|apply|rollback` を提供し、`apply` は fresh consent を要求する。
- `tools/release/` に SBOM（CycloneDX）、third-party notices、SHA256SUMS、署名 wrapper を追加した。
- `packaging/` に macOS Info.plist / entitlements / notarize、Windows MSIX manifest / package、Linux AppImage を追加した。Flatpak は CPAL が portal 返却 PipeWire FD/node を消費できないため非 production prototype とし、直接 device access を与えず配布しない。
- `.github/workflows/release.yaml` は各 OS の native test、build・署名・notarization・attestation・update metadata 署名を実行する。reusable `release-hil.yaml` は同じ run の immutable package を download し、必須 install/launch/uninstall と mic/system waveform gate を self-hosted runner で実行する。
- `docs/release-checklist.md`（privacy/security/release gate）と `docs/release-hil-matrix.md` を追加した。

未達: 実機での signing/notarization/staple、MSIX 署名、portal denial、HIL matrix の実測値はリリース時に `docs/release-checklist.md` / `docs/release-hil-matrix.md` で記録する。

## 優先する設計原則

1. 取得済み model による推論中は、明示的操作なしに network を使用しない。
2. audio callback で allocation、lock、I/O、log、推論を行わない。
3. OS 固有機能は compile-time の OS 名だけでなく、実行時 capability として扱う。
4. durable state を event stream だけに依存させない。
5. 録音開始、共有範囲、保存先、model download は個別の同意対象にする。
6. CLI、GPUI、MCP は同じ application service を利用する薄い adapter にする。
7. model、runtime、app binary、update metadata の信頼性と license を別々に追跡する。

## 現在の repository

現在は Milestone 7 の production candidate まで実装した基盤として `koe-core`、`koe-audio`、`koe-recording`、
`koe-app`、`koe-model`、`koe-transcript`、`koe-update`、`koe-cli`、`koe-desktop`、`koe-mcp` が存在する。Milestone 1/2 の
domain state machine、bounded callback handoff、segmented WAV と crash recovery、
単一所有 coordinator、capability/doctor CLI、system audio と同期、manifest v2、
Milestone 3 の Foundry Local モデル管理、Milestone 4 の CLI reference product に
加えて、Milestone 5 の GPUI desktop adapter を実装した。

- `koe-model` で `FoundryAdapter`/`StreamingAsrSession` の port と
  `KoeModelManager` を実装し、list/resolve/install/load/unload/remove と model
  state machine を提供する。
- offline 契約は manager 境界で強制する。`Denied` は adapter へ一切触れず、
  cache に無い artifact は `KOE-MODEL-OFFLINE-MISSING` を返す。
- install は明示同意 (`ModelInstallOnly`) のみ許可し、digest inventory を
  manifest に記録して allowlist 照合 / quarantine を行う。active model の
  remove と version switch は `KOE-MODEL-BUSY` で拒否する。
- ライブ ASR は 16 kHz mono PCM を bounded feed bridge で async session へ送り、
  `koe-transcript` が `events.jsonl` / `final.json` / `final.txt` を materialize する。
- chunk size ごとの latency/WER/RTF baseline を `koe models benchmark` で保存する。
- Milestone 4 で `koe sessions` (list/show/export/delete) と `koe config`
  (show/set-retention/apply-retention) を追加し、データルートの設定・保存期間
  ポリシー・自動削除を実装する。
- `koe doctor` を拡張し、data root 書き込み、config 整合性、session ストア、
  audio backend、permission 状態を包括的に診断する。
- すべてのコマンドで `--output-format json|jsonl` を提供し、stdout/stderr の
  機械可読 contract をテストで保証する。default log には audio 波形や transcript
  テキストは含まれない。
- `koe-desktop` は setup、recorder、model manager、session library、privacy/
  diagnostics settings を提供する。GPUI の state は `koe-app::desktop` の frontend
  非依存 view model と shared `SessionSnapshot` に従う。
- desktop 録音は CPAL callback を bounded ring へ渡し、background capture worker
  から既存 `RecorderCoordinator` と segmented WAV store を利用する。fresh consent、
  permission denied/revoked guidance、cooperative stop/finalize を CLI と共通化する。
- すべての操作要素は Tab/Shift-Tab で移動できる。録音 indicator は page state と
  分離し、window title にも反映して最小化中の OS surface に残す。
- desktop privacy default は offline-only、diagnostics opt-in、retention forever で、
  settings は app-owned data root へ atomic rename で保存する。
- ネイティブ live-audio session は公開された foundry SDK に無いため、capability
  として報告する。E2E offline テストは fixture adapter で駆動する。
- `koe-mcp` は MCP 2025-06-18 の stdio JSON-RPC server として capability/device/model/
  session tool、operation state と cancellation、session/transcript resource を提供する。
- MCP の stdout は protocol 専用で、request/response size と operation concurrency を
  制限する。data/export root は起動時に固定し、UUID session のみ認可する。
- 録音、model install、session/transcript exposure、export、delete は call ごとの fresh
  consent を要求する。stdio EOF 時は active recording/install を cooperative cancel する。
- `unsafe_code`、panic、unwrap、unused などを deny する strict lint は維持する。
- Nix の対象は `x86_64-linux`、`aarch64-linux`、`aarch64-darwin` であり、
  Windows と Intel macOS は CI で補完する。

## 調査の範囲

本仕様は `ren workflow deep-research` により、計画 1、並列調査 6、独立検証 6、
統合 1 の計 14 agent slot を実行して作成した。CPAL、Foundry Local、各 OS、
MCP、Rust/Cargo、NVIDIA model card の一次資料を優先した。

文書にある「確認済み」は一次資料で直接確認できた事実、「方針」は koe の設計判断、
「要検証」は実機または upstream への確認が必要な事項を表す。
