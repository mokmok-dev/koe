# 音声キャプチャ

## 目的

マイク、システム音声、将来の file input を同じ session pipeline に接続しつつ、
device、権限、clock、sample format の OS 差を application core から隔離する。

## 共通 API

`koe-audio` は概念的に次の境界を持つ。

```rust
trait AudioBackend {
    fn capabilities(&self) -> Result<AudioCapabilities, AudioError>;
    fn enumerate(&self, kind: SourceKind) -> Result<Vec<AudioDevice>, AudioError>;
    fn open(&self, request: OpenSource) -> Result<Box<dyn AudioStream>, AudioError>;
}

trait AudioStream {
    fn start(&mut self, sink: FrameSink) -> Result<(), AudioError>;
    fn stop(&mut self) -> Result<(), AudioError>;
}
```

実際の trait は callback safety と object safety を検証して決める。外部へ渡す frame は
少なくとも次を含む。

- session 内で単調増加する sequence
- source ID と source kind
- native sample rate、channel count、sample format
- CPAL の capture/callback timestamp
- discontinuity、overflow、device-lost marker
- PCM payload

OS 固有 device ID は opaque string として保存し、永続設定では表示名と backend
を併記する。ID が次回起動時に無効なら自動的に別 device へ切り替えず、ユーザーへ
再選択を求める。

## OS 別 backend

| OS | マイク | システム音声 | 方針 |
| --- | --- | --- | --- |
| Windows | CPAL/WASAPI capture endpoint | CPAL/WASAPI render endpoint loopback | shared mode を必須とし、DRM 無音を正常状態として扱う |
| macOS 14.6+ | CPAL/CoreAudio | CPAL process tap + private aggregate device | microphone と system audio の用途宣言・権限を分離する |
| Linux/PipeWire | CPAL native PipeWire source | sink capture | 第一選択。sandbox では XDG portal の許可 node に限定する |
| Linux/PulseAudio | CPAL/PulseAudio source | sink monitor source | compatibility fallback |
| Linux/ALSA | CPAL/ALSA | portable な system mix はない | microphone のみ。`snd-aloop` は opt-in advanced 設定 |

Windows loopback は system mix を shared mode で取得し、保護コンテンツが除外される
場合がある。[Microsoft WASAPI loopback](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)

CPAL は Windows render device の input stream に `LOOPBACK` flag を付ける。
[CPAL WASAPI implementation](https://raw.githubusercontent.com/RustAudio/cpal/master/src/host/wasapi/device.rs#L763-L829)

macOS loopback は CoreAudio process tap と private aggregate device を利用する。
CPAL の loopback 最低対応は macOS 14.6 である。
[CPAL macOS implementation](https://raw.githubusercontent.com/RustAudio/cpal/master/src/host/coreaudio/macos/loopback.rs#L63-L128),
[CPAL platform support](https://github.com/RustAudio/cpal#supported-platforms)

PipeWire backend は sink capture と stream grouping を実装している。
[CPAL PipeWire implementation](https://raw.githubusercontent.com/RustAudio/cpal/master/src/host/pipewire/device.rs#L141-L170)

## Realtime 規則

CPAL callback は高優先度 thread で呼ばれるため、callback 内では次を禁止する。

- blocking mutex、async runtime への待機
- heap allocation と buffer resize
- model inference、resampling、mixing
- filesystem I/O、network I/O
- format を伴う logging

事前確保した source ごとの bounded SPSC ring buffer へ frame をコピーし、
満杯時は source ごとの `OverflowPolicy` に従う。既定は oldest/newest の暗黙破棄
ではなく、現在の frame を拒否して `DroppedFrames` marker と metric を生成する。
ASR は欠落を跨いで文章を連結しない。

`ringbuf::try_push` は overflow を明示できる。
[ringbuf](https://docs.rs/ringbuf/latest/ringbuf/)

## 正規化、同期、mix

ASR feed は little-endian signed PCM、16 kHz、mono、16-bit を canonical format
とする。録音用 isolated stem は可能な限り native format を保持する。

1. capture timestamp を session monotonic clock へ anchor する。
2. source ごとに channel mapping と gain を適用する。
3. `rubato` の事前確保 buffer で 16 kHz へ変換する。
4. microphone と system audio の drift を連続推定する。
5. bounded jitter buffer 後に共通 timeline へ配置する。
6. ASR 用 mono mix と、保存用 isolated stem を別々に生成する。

CPAL は異なる stream 間で clock origin を共有すると保証しない。cross-stream の
timestamp subtraction を同期根拠にしてはならない。
[StreamInstant](https://docs.rs/cpal/latest/cpal/struct.StreamInstant.html)

`rubato::process_into_buffer` は事前確保した realtime 処理に利用でき、async
resampler は clock drift に応じて ratio を変更できる。
[rubato realtime considerations](https://docs.rs/rubato/latest/rubato/#real-time-considerations)

## Error と recovery

`AudioError` は少なくとも次を区別する。

- `Unsupported`
- `PermissionRequired` / `PermissionDenied`
- `DeviceNotFound` / `DeviceLost`
- `UnsupportedFormat`
- `StreamBuildFailed` / `StreamRuntimeFailed`
- `BufferOverflow`
- `ClockDiscontinuity`

device loss 時は session を `Degraded` に遷移させ、残る source と writer を即座に
停止しない。自動 reopen は同じ stable device ID に対し bounded retry とし、
別 device への切替は明示操作にする。gap は manifest と transcript に記録する。

## 要検証

- macOS 14.6 以降の minor release ごとの権限挙動
- PipeWire portal で system-audio-only を選択できる desktop matrix
- PulseAudio monitor source の stable identifier
- source ごとの clock drift と acceptable correction range
- Bluetooth、USB hot-plug、sample-rate change
- DRM、無音、省電力復帰、default device change
