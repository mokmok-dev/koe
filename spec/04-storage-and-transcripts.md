# 録音、transcript、永続化

## Session directory

各 session は許可済み data root 配下の一意 directory とする。

```text
sessions/<uuid>/
  session.json
  audio/
    mic-000001.wav
    system-000001.wav
    mix-000001.wav
  transcript/
    events.jsonl
    final.json
    final.txt
  recovery/
    active.json
```

directory 名に title、device 名、model alias など外部入力を使わない。表示名は
`session.json` 内の data とし、export 時だけ sanitize した file name を提案する。

## Audio format

初期版は PCM WAV を採用する。

- isolated stem: device native rate/channel を保てる範囲で保存
- ASR mix: 16 kHz、mono、signed 16-bit PCM
- file size 上限に達する前に sequence file へ rotate
- rotate point は complete sample frame とする
- manifest に各 file の sample count、timeline range、digest を記録

Hound の `flush` は WAV header を checkpoint し、最後の flush まで読み取り可能にする。
`finalize` の error を必ず処理する。
[hound WavWriter](https://docs.rs/hound/latest/hound/struct.WavWriter.html#method.flush)

RF64 対応は確認できていないため、初期版では長時間を単一 file にせず segment する。
segment duration は 15 分を既定候補とし、実測と filesystem limit で決める。

## Crash consistency

1. `create_new(true)` で session directory 内の staging file を作る。
2. audio writer を開始する前に `active.json` を atomic replace する。
3. 一定時間または一定 sample ごとに WAV と transcript event を flush する。
4. checkpoint 後に manifest copy を temp file へ書く。
5. fsync policy に従い file と parent directory を同期する。
6. atomic rename で manifest を公開する。
7. finalize 成功後に `active.json` を除去し `Completed` を記録する。

起動時に `active.json` を走査し、WAV header と transcript JSONL の最後の完全 record
までを検証する。自動で上書きせず、`RecoveredPartial` として新 manifest を作る。

## Transcript model

```json
{
  "schema_version": 1,
  "segment_id": "uuid",
  "source": "mixed",
  "start_ms": 1200,
  "end_ms": 2840,
  "text": "transcript",
  "final": true,
  "model": {
    "id": "stable model id",
    "version": "version",
    "variant": "variant"
  },
  "audio_discontinuities": []
}
```

`events.jsonl` は append-only event log、`final.json` は materialized view とする。
interim result が導入されても同じ `segment_id` の revision として扱い、順序だけで
置換しない。現行 Nemotron が常に final を返す挙動にも対応できる。

時刻は session monotonic timeline の整数 microseconds を canonical とし、wall clock
は session 開始時 anchor としてのみ記録する。cross-stream clock の補正情報と gap を
manifest に保存する。

## Session manifest

少なくとも次を含む。

- schema version、session UUID、状態、開始/終了時刻
- app/version/platform/backend
- source device、native format、permission result
- normalization、gain、mix、resample、VAD、chunk 設定
- queue capacity、overflow count、discontinuity
- model ID/version/variant/provider/license
- audio/transcript file list、size、digest、timeline
- network policy と consent record の参照
- failure/recovery code

## Export

内部 session directory と user export を分離する。export target は次を選べる。

- audio: isolated WAV、mixed WAV
- transcript: UTF-8 text、JSON、将来の SRT/VTT
- bundle: manifest と選択 artifact

export は既存 file を既定で上書きしない。`create_new(true)` は既存 file と dangling
symlink を原子的に拒否する。
[Rust OpenOptions](https://doc.rust-lang.org/std/fs/struct.OpenOptions.html#method.create_new)

## Retention と deletion

- retention は `forever`、日数、session 単位の明示削除を持つ。
- auto deletion は active/export 中の session を除外する。
- delete 前に対象が data root 配下の既知 UUID directory であることを再検証する。
- symlink を辿らない。
- metadata index を先に「deleting」へ遷移し、個別 failure を記録する。
- `remove_file` は物理的な即時消去を保証しないと UI と仕様に明記する。

[Rust remove_file](https://doc.rust-lang.org/std/fs/fn.remove_file.html)

機密性が強く必要なら filesystem 全体暗号化と backup 除外を推奨する。将来の
application-level encryption は key rotation、thumbnail/search、crash recovery を
含む別 threat model として導入する。
