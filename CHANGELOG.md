# Changelog

## Unreleased

### Changed

- `ModelError::NetworkDenied.code()` now returns `KOE-MODEL-NETWORK-DENIED`
  instead of `KOE-MODEL-OFFLINE-MISSING`. Consumers matching stable error codes
  should handle the new value as a consent/policy failure; the old value remains
  reserved for `OfflineArtifactMissing`.
- `ModelError` and `AdapterError` are now `#[non_exhaustive]`; downstream
  matches must include a wildcard arm. This pre-1.0 change permits future error
  additions without repeatedly breaking exhaustive matches.
- `FoundryAdapter::install` must return a complete, non-empty artifact inventory
  even for cache hits. Prefer
  `InstalledArtifact::try_from_cache_paths(root, model_id, paths).await` for a
  preexisting cache hit, or `try_from_created_cache_paths(...)` for a newly
  downloaded entry that cancellation may safely clean up; use `try_new`/
  `try_new_created` only for prevalidated path claims. `into_parts()` now
  returns `InstalledArtifactParts`, whose named `created_by_install` field
  preserves cleanup provenance. The manager hashes each file once.
- `CoordinatorTask::shutdown` no longer accepts a `RecorderCoordinator`; replace
  `task.shutdown(&coordinator)` with `task.shutdown()`.
- `FileDigest` fields are private and validated. Replace struct literals with
  `FileDigest::try_new(sha256, size)?` and use the `sha256()`/`size()` accessors.
- `TranscriptError` is now `#[non_exhaustive]` and includes `RecordTooLarge`,
  `InvalidSegment`, and `StoreLocked`; downstream matches must include a
  wildcard arm. Append validates schema version 1, ordered timestamps, and
  nonempty/control-free source and model identities. `final_segment(...)` and
  `segment.revise(...)` now return `Result`; migrate `let s = final_segment(...)`
  to `let s = final_segment(...)?` (and likewise `let r = s.revise(...)?`). Use
  `TranscriptSegment::validate` for a structured reason. `TranscriptStore::open`
  now owns an exclusive lifetime lock and reports
  `KOE-TRANSCRIPT-STORE-LOCKED` for concurrent owners.
- `TranscriptError::CorruptLog.code()` now returns
  `KOE-TRANSCRIPT-CORRUPT-LOG` instead of the unrelated
  `KOE-STORE-FINALIZE-FAILED`; consumers matching stable codes should add the
  new corruption-specific value.
- `OpenSource::{sample_rate, channels}` are now
  `preferred_sample_rate`/`preferred_channels`, with serialized field names
  preserved. Missing `negotiation` deserializes as `Nearest` to preserve the
  previous backend-selection behavior; opt into `Exact` explicitly. Rust
  callers can migrate literals to
  `OpenSource::exact(...)` or `OpenSource::nearest(...)`.
- Installed-model listing is now fail-closed on corrupt manifests. Use
  `ModelStore::inspect_installed_manifests` to display healthy entries and all
  repairable corrupt IDs, then call the manager repair API as appropriate.
- `ReplacementFailure` is `#[non_exhaustive]`; include a wildcard match arm.
  `ModelError::ReplacementInvalidated` now includes both the invalidated
  `InstalledModelId` and a classified cause.
- Installation enforces one persisted registration per runtime model ID. Legacy
  stores with multiple registrations return
  `ModelError::DuplicateRegistrations`; remove redundant registrations
  explicitly before updating.
- `ModelManager::inspect_installed_models` is async and returns healthy models
  alongside every repairable corrupt installation ID.
- Direct `ModelStore::publish_manifest` now validates caller-provided
  inventories and returns `ModelError::InvalidManifest(ManifestValidationError)`
  for malformed input.
- `ModelStore::open` now takes a lifetime-scoped exclusive interprocess lock;
  another independent owner receives `ModelError::StoreLocked`. Clones share
  the same lock.
- Model removal now commits registration deletion even when adapter cache
  cleanup fails and reports `RemovalIncomplete`; callers should treat this as
  partial success because retrying the removed installation id returns
  `NotFound`.
- `ModelManifest` is now `#[non_exhaustive]`; downstream code should obtain it
  from manager/store results or deserialize persisted manifests rather than use
  struct literals. Mocks and tests can use
  `ModelManifest::external(descriptor, files, verification)`, whose timestamp is
  zero, SDK version is `external`, and cache directory is `None`. Older
  manifests deserialize `cache_directory` as `None`.
