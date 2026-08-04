# セキュリティとプライバシー

## 保護対象

- raw microphone/system audio
- transcript と speaker/context metadata
- session title、file path、device name
- model/runtime artifact と license record
- application/update signing key
- optional API credential

## Trust boundary

```text
OS audio subsystem / permission broker
  -> CPAL and OS adapters
  -> koe process
       -> Foundry native runtime
       -> local session/model stores
       -> CLI/GPUI/MCP caller
network
  -> model/update endpoints（明示操作時のみ）
```

audio source、Foundry/runtime、MCP client、download artifact、filesystem path はすべて
別 trust boundary とする。

## Privacy defaults

- inference は local-only
- session 中の network は `Denied`
- telemetry、cloud upload、自動 transcript sharing は実装しない
- log に audio、transcript、prompt、secret、完全 path を含めない
- crash report と diagnostics は生成前後に allowlist filter を通す
- retention は明示表示し、削除保証の限界を説明する

## Consent と録音表示

録音開始前に次を表示・確認する。

- microphone/system audio の各 source
- system-wide か選択 process/window か
- 保存先と retention
- ASR model と license
- MCP caller へ返す範囲

OS permission と application consent は別 record とする。過去の OS grant を今回の
recording consent とみなさない。recording 中は CLI、window、tray/menu の少なくとも
利用中 surface に persistent indicator を出す。

macOS の microphone purpose string と system audio declaration を設定する。
Windows package は microphone capability と denial を処理する。
[Apple microphone permission](https://developer.apple.com/documentation/BundleResources/Information-Property-List/NSMicrophoneUsageDescription),
[Windows capabilities](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/app-capability-declarations#device-capabilities)

Linux sandbox では XDG portal によりユーザーが選択した PipeWire node だけを使う。
[XDG ScreenCast portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)

## Filesystem

- app-owned data root を起動時に決定し、operation 中に変更しない。
- path component は UUID または application-generated ID に限定する。
- user path は canonical root と component 単位で検証する。
- symlink、hard-link、mount point、case folding を OS 別 test する。
- new file は `create_new(true)`、owner-only permission、予測困難な temp 名を使う。
- temp から final への publish は同一 filesystem 内 atomic rename とする。
- archive/export 展開を実装する場合は path traversal と decompression bomb を防ぐ。

`create_new(true)` は既存 file と dangling symlink を原子的に拒否する。
[Rust OpenOptions](https://doc.rust-lang.org/std/fs/struct.OpenOptions.html#method.create_new)

## Secret

初期版の offline recording に secret は不要である。将来 secret が必要な場合:

- macOS: Keychain
- Windows: Credential Locker
- Linux: Secret Service が利用可能なときだけ永続化
- unavailable 時: memory-only または機能無効

[Apple Keychain](https://developer.apple.com/documentation/security/keychain-services),
[Windows Credential Locker](https://learn.microsoft.com/en-us/windows/apps/develop/security/credential-locker)

## MCP

- 初期版は stdio のみ
- least privilege user、制限された filesystem view
- model install 時以外は network deny
- tool ごとに path/session ownership を再認可
- recording、data exposure、delete 前に host consent
- stdout は protocol 専用、log は stderr
- request size、concurrency、duration、output size に limit
- arbitrary command、arbitrary path、arbitrary URL tool を提供しない

[MCP security principles](https://modelcontextprotocol.io/specification/2025-03-26/index#security-and-trust-safety),
[MCP local server security](https://modelcontextprotocol.io/docs/2026-07-28/tutorials/security/security_best_practices#local-mcp-server-compromise)

HTTP transport を追加する場合は loopback bind だけで安全とはみなさず、origin validation、
authentication、authorization、CSRF/DNS rebinding 対策、TLS または制限 IPC を別仕様にする。

## Supply chain

対象を分離して管理する。

| 対象 | 必須情報 |
| --- | --- |
| Rust crate | version、source、license、advisory、lockfile |
| Foundry SDK/Core/CLI | version、配布元、license、digest |
| model/provider | model ID、version、variant、license、files/digest |
| app binary | commit、builder、signature、SBOM、provenance |
| update metadata | role、version、expiry、target digest |

`cargo vendor` の checksum は悪意ある改変に対する真正性保証ではない。
[Cargo source replacement](https://doc.rust-lang.org/cargo/reference/source-replacement.html#directory-sources)

- `cargo vet` または同等 review policy
- `cargo deny` による advisory/license/source check
- locked dependency と reproducible build の継続評価
- CycloneDX または SPDX SBOM
- GitHub artifact attestation
- TUF-style signed/versioned/expiring/hash-bound update metadata

[TUF specification](https://theupdateframework.github.io/specification/latest/),
[GitHub artifact attestations](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations)

## Deletion

通常の file deletion は physical immediate erase を保証しない。
[Rust remove_file](https://doc.rust-lang.org/std/fs/fn.remove_file.html)

製品が保証するのは app index と通常 filesystem namespace からの削除までとする。
SSD wear leveling、snapshot、backup、open handle からの復元不能性は保証しない。
filesystem encryption、backup exclusion、retention policy を推奨し、UI で保証範囲を明示する。

## Security gates

- threat model review
- permission denial/revocation test
- symlink/path race test
- network-denied offline test
- model digest mutation test
- MCP consent/authorization test
- log/diagnostic data-leak snapshot test
- dependency/model/license inventory
- signed release と provenance verification
