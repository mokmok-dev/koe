# Model license registry

koe does not bundle model weights in application releases. Model installation is
an explicit, separately consented Foundry Local operation. The resolved catalog
license identifier, model ID, version, variant, provider, source URI, and file
digests are persisted in each installed model manifest and shown before use.

| Catalog selector | Distribution in koe | License gate |
| --- | --- | --- |
| `nemotron-speech-streaming-en-0.6b` | Not bundled | The exact catalog-provided license must be displayed and accepted at install time. |
| `nemotron-3.5-asr-streaming-0.6b` | Not bundled | The exact catalog-provided license must be displayed and accepted at install time. |

A release must not claim a static model license for an alias: Foundry may resolve
an alias to a different model version or hardware variant. The immutable
per-install model manifest is the authoritative registry for acquired weights.
