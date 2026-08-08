# テスト、CI、配布

## Test layers

### Unit

- state transition と idempotent stop/cancel
- sample conversion、downmix、gain、clipping
- timestamp mapping、drift estimator
- queue overflow と discontinuity
- model selector、manifest、license policy
- transcript revision と timeline
- path validation、retention、redaction
- config migration

audio test は deterministic fixture と property test を使い、wall clock や実 device に
依存させない。

### Component

- fake CPAL source -> normalization -> writer
- fixture PCM -> fake ASR -> transcript
- fake catalog -> install/cancel/update/remove
- crash point injection -> recovery
- bounded slow consumer -> backpressure
- CLI JSON/JSONL contract
- MCP protocol conformance と stdout cleanliness

### OS integration

| Case | Windows | macOS | Linux |
| --- | --- | --- | --- |
| microphone | WASAPI | CoreAudio | PipeWire/Pulse/ALSA |
| system audio | WASAPI loopback | process tap | PipeWire sink/Pulse monitor |
| permission deny/revoke | package/settings | TCC | portal/policy |
| device hot-plug | 必須 | 必須 | 必須 |
| sleep/resume | 必須 | 必須 | 必須 |
| default switch | 必須 | 必須 | 必須 |
| clock drift | mic + loopback | mic + tap | source + sink |

### Hardware-in-the-loop

GitHub-hosted runner は compile/unit/fixture/package matrix に使い、real microphone、
loopback、permission、hot-plug、glitch、GPU/NPU は専用 machine で検証する。
[GitHub runner matrix](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/choose-the-runner-for-a-job)

HIL machine は専用 test audio を物理/virtual route へ再生し、capture 結果を既知 waveform
と比較する。user の実 audio や meeting を test artifact にしない。

## Quality metrics

実装前に測定方法を固定し、Milestone 1 の baseline 後に threshold を決める。

- callback deadline miss / hour
- dropped frames / hour
- clock drift ppm と correction discontinuity
- peak/RMS error、channel mapping、clipping count
- first result latency、final result latency
- real-time factor
- WER/CER（言語別、正規化規則を固定）
- resident memory、model load time、disk footprint
- CPU/GPU utilization、thermal throttling
- recovery time、finalization time

Nemotron model card は chunk 80/160/560/1120 ms の latency/accuracy tradeoff を示す。
製品 threshold は model card の値を流用せず、対象 hardware/corpus で決める。
[Nemotron performance](https://huggingface.co/nvidia/nemotron-speech-streaming-en-0.6b#performance)

## CI matrix

Hosted CI:

- Ubuntu x86_64、Linux arm64
- Windows x86_64、可能なら arm64 compile
- macOS Intel、Apple Silicon
- stable Rust、最低対応 Rust は決定後追加
- fmt、clippy、test、doc、dependency/license audit
- feature matrix: minimal、PipeWire、PulseAudio、MCP
- package smoke test と artifact attestation

Nix は既存の Linux x86_64/arm64、Apple Silicon を維持する。Windows と Intel macOS は
GitHub Actions の native runner を authoritative build とする。

PR では model download を行わず fake/fixture を使う。nightly/release candidate で
許可された test cache と HIL を使い、license acceptance を自動 bypass しない。

## Release artifact

- CLI archive/binary
- MCP binary は CLI と同一 binary の subcommand または別最小 binary
- checksum file
- SBOM
- third-party notices と model license registry
- provenance attestation
- signed update metadata

model は初期版 app に同梱せず、user が license を確認して install する。
offline environment への transfer bundle は将来、署名 manifest と digest verification
を含む別 feature とする。

## Platform signing

macOS:

- Developer ID
- hardened runtime
- secure timestamp
- notarization
- ticket stapling

[Apple notarization](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)

Windows:

- MSIX または署名済み installer
- trusted code-signing certificate
- timestamp
- capability declaration

[MSIX signing](https://learn.microsoft.com/en-us/windows/msix/package/signing-package-overview)

Linux:

- distro package または AppImage/Flatpak の選定 spike
- PipeWire/Pulse/ALSA dependency の明示
- Flatpak 時は portal と permission manifest
- package signature と repository metadata

## Release gates

1. clean checkout から reproducible build
2. unit/component/hosted matrix green
3. OS HIL matrix green
4. offline firewall test green
5. model install/load/unload/remove matrix green
6. permission/consent/privacy review
7. SBOM/advisory/license review
8. signing/notarization/package verification
9. update/rollback simulation
10. recovery artifact の manual inspection
