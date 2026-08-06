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

## 優先する設計原則

1. 取得済み model による推論中は、明示的操作なしに network を使用しない。
2. audio callback で allocation、lock、I/O、log、推論を行わない。
3. OS 固有機能は compile-time の OS 名だけでなく、実行時 capability として扱う。
4. durable state を event stream だけに依存させない。
5. 録音開始、共有範囲、保存先、model download は個別の同意対象にする。
6. CLI、GPUI、MCP は同じ application service を利用する薄い adapter にする。
7. model、runtime、app binary、update metadata の信頼性と license を別々に追跡する。

## 現在の repository

現在は Milestone 2 までの基盤として `koe-core`、`koe-audio`、`koe-recording`、
`koe-app`、`koe-cli` が存在する。domain state machine、bounded callback handoff、
segmented WAV と crash recovery、単一所有 coordinator、および capability/doctor
CLI を実装済みである。CPAL microphone capture adapter に加え、Windows/macOS の
output loopback と Linux の PipeWire sink/PulseAudio monitor を実行時に検出する。
system audio を選択した session は isolated stems と 16 kHz mono mix を保存し、
drift correction と gap marker を manifest に記録する。利用不能な source は
availability、permission、probe effect を分離して機械可読に報告する。manifest v2 は
callback block ごとの session timeline と capture epoch を整数 microseconds で保存する。
crash recovery は境界検証済みの専用 artifact を生成し、元の WAV を変更しない。
`unsafe_code`、panic、unwrap、unused などを deny する strict lint は維持する。
Nix の対象は `x86_64-linux`、`aarch64-linux`、`aarch64-darwin` であり、
Windows と Intel macOS は CI で補完する。

## 調査の範囲

本仕様は `ren workflow deep-research` により、計画 1、並列調査 6、独立検証 6、
統合 1 の計 14 agent slot を実行して作成した。CPAL、Foundry Local、各 OS、
MCP、Rust/Cargo、NVIDIA model card の一次資料を優先した。

文書にある「確認済み」は一次資料で直接確認できた事実、「方針」は koe の設計判断、
「要検証」は実機または upstream への確認が必要な事項を表す。
