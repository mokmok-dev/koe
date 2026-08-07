# Release checklist

Milestone 7 (`spec/08-roadmap.md`) acceptance requires the
**privacy / security / release** checklist to be complete before a release is
published. Every item is a release gate; the automated subset runs in
`.github/workflows/release.yaml`, the rest are manual and recorded in the
release PR or issue.

## How to release

1. Bump the workspace version in `Cargo.toml` (`[workspace.package] version`).
2. Open a release PR that records this checklist and reviews the generated
   `THIRD_PARTY_NOTICES.md` artifact.
3. Run the HIL matrix (`scripts/hil/*`) on the physical test machines
   (`.github/workflows/release-hil.yaml`).
4. Tag `vX.Y.Z`; `.github/workflows/release.yaml` builds, signs, notarizes,
   attests and signs update metadata. The reusable `release-hil.yaml` job
   downloads those exact artifacts. Publication also requires approval of the
   protected `production-release` environment after this checklist is attached
   to the release PR/issue.
5. Download the artifacts, verify checksums + attestation + update metadata,
   then publish the release notes.

## Privacy checklist

- [ ] `koe doctor` reports no network access during recording; outbound is
      blocked and `koe sessions show` works completely offline.
- [ ] Default logs contain no audio waveform, transcript text, full path or
      secret. Run the log/snapshot leak tests and the
      `privacy snapshot` conformance suite.
- [ ] Diagnostics are opt-in and pass the allowlist filter before generation
      or upload.
- [ ] Retention policy is displayed before recording and deletion limits are
      documented (`remove_file` does not guarantee physical erasure).
- [ ] Recording indicator is visible on the active surface (CLI, window title,
      MCP) whenever sources are live.
- [ ] macOS/Windows permission purposes and Linux PipeWire policy denial are
      shown before first capture; revoke/deny handling is verified on HIL.
      No sandbox/portal claim is made for the AppImage channel.

## Security checklist

- [ ] Threat model reviewed for this release (update supply chain, foundry
      model artifacts, MCP authorization).
- [ ] Model install requires explicit consent; `Denied` policy installs
      nothing; digest-inventory allowlist (or documented `runtime-only`
      boundary) is in every model manifest.
- [ ] Signed update metadata tested: expired, replayed, tampered and
      foreign-platform metadata all rejected (`koe update apply` unit and CLI
      tests). The first accepted update copies the executable that is already
      running into the app-owned store before activation, so rollback can
      restore the shipped version; later launches rehash that copy. This
      bootstrap protects update/download integrity, not a hostile process
      already executing as the same OS user (which can execute arbitrary code
      and is outside the publisher-signature threat model).
- [ ] `cargo deny` advisory/license audit green (or waiver recorded).
- [ ] `cargo vet` (or equivalent review policy) covers the dependency delta
      since the last release.
- [ ] CycloneDX SBOM generated for every workspace package from `Cargo.lock`;
      third-party notices (including license/NOTICE text) and the model license
      registry generated; no unconfirmed license entries remain.
- [ ] Provenance attestation emitted for every packaged artifact
      (`attest-build-provenance`) and recorded in the release notes.
- [ ] Signing keys: the update targets signing seed and macOS/Windows
      credentials are held in the CI secret store, scoped to the release
      environment; private keys never appear in logs or artifacts. Any future
      TUF root-rotation key remains offline.
- [ ] Path/symlink race and create-new semantics re-tested on the release
      branch (session, model store, update store).

## Release checklist

- [ ] Clean checkout → reproducible build (lockfile pinned, no surprise
      sources). `cargo build --release --workspace` passes on every matrix OS.
- [ ] Unit/component tests green on hosted CI (`cargo test` + `cargo clippy`
      with workspace lints on all platforms).
- [ ] OS HIL matrix green (`.github/workflows/release-hil.yaml`): mic,
      system audio, permission deny/revoke, hot-plug, sleep/resume, default
      switch, clock drift.
- [ ] Offline firewall test green: after install and model acquisition, the
      session runs with outbound blocked.
- [ ] Model install/load/unload/remove matrix green on release artifacts.
- [ ] Update simulation: retain shipped executable → install new → rollback →
      expired metadata → tampered artifact → replay. Unit/CLI tests cover the
      state machine; all scenarios also run against protected-environment
      signed fixture metadata and exact packaged release artifacts in HIL.
- [ ] Clean machine install/uninstall verified (MSIX, notarized `.app`, signed-inventory AppImage). Flatpak is not a production channel until a portal-backed audio adapter exists.
- [ ] Signing/notarization/package verification: `spctl -a -vv`, `signtool
      verify`, package signature check all pass on the HIL machines.
- [ ] SBOM/advisory/license review complete (see security checklist).
- [ ] Recovery artifact inspected manually: a forced crash mid-recording
      leaves a recoverable partial session (`koe sessions list` shows
      `RecoveredPartial`).
- [ ] Release notes include: checksums file, SBOM link, provenance statement,
      third-party notice link, update metadata public key fingerprint.

## Post-release

- [ ] Update metadata (signed) uploaded to the release endpoint and verified
      from a clean machine with only the pinned public key.
- [ ] `docs/release-hil-matrix.md` updated with the results of this release
      (dates, machines, metrics).
