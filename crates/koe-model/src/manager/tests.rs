//! Milestone 3 acceptance tests driven by the fixture adapter.

use std::time::Instant;

use koe_core::NetworkPolicy;
use tempfile::TempDir;
use tokio::sync::mpsc;

use crate::{
    AsrSessionSettings, DigestAllowlist, FixtureFoundryAdapter, FoundryAdapter, InstalledArtifact,
    InstalledFile, KoeModelManager, ModelError, ModelId, ModelManager, ModelScope, ModelSelector,
    fixture::CountingAdapter, fixture_transcribe, types::InstallOptions,
};

const SELECTOR: &str = "fixture-nemotron-asr-0.6b";

fn inventory(
    artifact: &InstalledArtifact,
    descriptor: &crate::ModelDescriptor,
) -> Result<Vec<crate::ModelFile>, ModelError> {
    super::inventory_from_artifact(
        artifact,
        descriptor,
        &tokio_util::sync::CancellationToken::new(),
    )
}

fn install_options(policy: NetworkPolicy) -> InstallOptions {
    InstallOptions {
        policy,
        cancel: tokio_util::sync::CancellationToken::new(),
        progress: None,
        accepted_descriptor: None,
        force_redownload: false,
    }
}

#[allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
fn sample_audio(seconds: usize) -> Vec<i16> {
    (0..16_000 * seconds)
        .map(|i| {
            let phase = (i as f64 * 2.0 * std::f64::consts::PI * 440.0 / 16_000.0).sin();
            (phase * 12_000.0) as i16
        })
        .collect()
}

#[tokio::test]
async fn online_install_then_offline_transcription_succeeds() {
    let root = TempDir::new().expect("temp");
    let cache = TempDir::new().expect("cache");
    let manager = KoeModelManager::new(
        root.path(),
        DigestAllowlist::empty(),
        Box::new(FixtureFoundryAdapter::new(cache.path())),
        NetworkPolicy::Denied,
    )
    .expect("manager");

    // Online install with explicit consent.
    let installed = manager
        .install(
            &SELECTOR.parse::<ModelSelector>().expect("selector"),
            &install_options(NetworkPolicy::ModelInstallOnly),
        )
        .await
        .expect("install");
    assert_eq!(
        installed.manifest.verification,
        crate::Verification::RuntimeOnly
    );
    assert!(!installed.manifest.files.is_empty());

    // Offline load + transcription, policy frozen to Denied.
    let loaded = manager.load(&installed.id).await.expect("offline load");
    let mut session = manager
        .create_asr_session(&installed.id, &AsrSessionSettings::default())
        .await
        .expect("session");
    let audio = sample_audio(1);
    session
        .append(crate::Pcm16Mono16k {
            samples: audio.clone(),
            session_start_us: 0,
        })
        .await
        .expect("append");
    let final_transcript = Box::new(session).finish().await.expect("finish");
    let text = final_transcript
        .events
        .iter()
        .map(|event| event.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(text, fixture_transcribe(&audio));
    manager
        .unload(&loaded.id)
        .await
        .expect("finished session releases exactly one model reference");
}

#[tokio::test]
async fn denied_policy_never_touches_the_adapter() {
    let root = TempDir::new().expect("temp");
    let cache = TempDir::new().expect("cache");
    let counting = CountingAdapter::new(FixtureFoundryAdapter::new(cache.path()));
    let manager = KoeModelManager::new(
        root.path(),
        DigestAllowlist::empty(),
        Box::new(counting),
        NetworkPolicy::Denied,
    )
    .expect("manager");

    for policy in [NetworkPolicy::Denied, NetworkPolicy::Allowed] {
        let error = manager
            .install(
                &SELECTOR.parse::<ModelSelector>().expect("selector"),
                &install_options(policy),
            )
            .await
            .expect_err("non-install policy must fail");
        assert_eq!(error, ModelError::NetworkDenied);
        assert_eq!(error.code(), "KOE-MODEL-OFFLINE-MISSING");
    }

    let missing = manager
        .resolve(&SELECTOR.parse::<ModelSelector>().expect("selector"))
        .await
        .expect_err("offline resolve must fail");
    assert_eq!(missing, ModelError::OfflineArtifactMissing);

    let catalog = manager
        .list(ModelScope::Catalog)
        .await
        .expect_err("catalog offline must fail");
    assert_eq!(catalog.code(), "KOE-MODEL-OFFLINE-MISSING");

    let internal = manager
        .list(ModelScope::Loaded)
        .await
        .expect("loaded is local");
    assert!(internal.is_empty());
    assert_eq!(
        manager.adapter_outbound_attempts().await.expect("attempts"),
        0,
        "denied policy must never touch the adapter"
    );
}

#[tokio::test]
async fn install_resolution_requires_the_narrow_policy_and_honors_precancellation() {
    let root = TempDir::new().expect("temp");
    let cache = TempDir::new().expect("cache");
    let manager = KoeModelManager::new(
        root.path(),
        DigestAllowlist::empty(),
        Box::new(CountingAdapter::new(FixtureFoundryAdapter::new(
            cache.path(),
        ))),
        NetworkPolicy::Denied,
    )
    .expect("manager");
    let selector = SELECTOR.parse::<ModelSelector>().expect("selector");

    for policy in [NetworkPolicy::Denied, NetworkPolicy::Allowed] {
        assert_eq!(
            manager
                .resolve_for_install(
                    &selector,
                    policy,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await,
            Err(ModelError::NetworkDenied)
        );
    }
    let cancelled = tokio_util::sync::CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        manager
            .resolve_for_install(&selector, NetworkPolicy::ModelInstallOnly, &cancelled)
            .await,
        Err(ModelError::Cancelled)
    );
    assert_eq!(manager.adapter_outbound_attempts().await.expect("calls"), 0);

    let descriptor = manager
        .resolve_for_install(
            &selector,
            NetworkPolicy::ModelInstallOnly,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("narrow resolve");
    assert_eq!(descriptor.alias.0, SELECTOR);
    assert_eq!(manager.adapter_outbound_attempts().await.expect("calls"), 1);
}

#[tokio::test]
async fn active_model_removal_and_version_switch_are_refused() {
    let root = TempDir::new().expect("temp");
    let cache = TempDir::new().expect("cache");
    let manager = KoeModelManager::new(
        root.path(),
        DigestAllowlist::empty(),
        Box::new(FixtureFoundryAdapter::new(cache.path())),
        NetworkPolicy::Denied,
    )
    .expect("manager");
    let installed = manager
        .install(
            &SELECTOR.parse::<ModelSelector>().expect("selector"),
            &install_options(NetworkPolicy::ModelInstallOnly),
        )
        .await
        .expect("install");

    // Removal while loaded is refused.
    manager.load(&installed.id).await.expect("load");
    let removal = manager
        .remove(&installed.id)
        .await
        .expect_err("loaded removal must fail");
    assert_eq!(removal, ModelError::Busy);

    // Version switch (reinstall with the same alias) while loaded is refused.
    let version_switch = manager
        .install(
            &SELECTOR.parse::<ModelSelector>().expect("selector"),
            &install_options(NetworkPolicy::ModelInstallOnly),
        )
        .await
        .expect_err("version switch must fail");
    assert_eq!(version_switch, ModelError::Busy);

    // After unload, removal succeeds.
    let loaded = manager.load(&installed.id).await.expect("idempotent load");
    manager.unload(&loaded.id).await.expect("unload");
    manager.remove(&installed.id).await.expect("remove");
    assert_eq!(
        manager
            .list(ModelScope::Installed)
            .await
            .expect("installed list"),
        Vec::new()
    );
}

#[tokio::test]
async fn remove_is_refused_while_an_asr_session_is_active() {
    let root = TempDir::new().expect("temp");
    let cache = TempDir::new().expect("cache");
    let manager = KoeModelManager::new(
        root.path(),
        DigestAllowlist::empty(),
        Box::new(FixtureFoundryAdapter::new(cache.path())),
        NetworkPolicy::Denied,
    )
    .expect("manager");
    let installed = manager
        .install(
            &SELECTOR.parse::<ModelSelector>().expect("selector"),
            &install_options(NetworkPolicy::ModelInstallOnly),
        )
        .await
        .expect("install");
    manager.load(&installed.id).await.expect("load");
    let loaded = manager
        .load(&installed.id)
        .await
        .expect("idempotent load handle");
    let session = manager
        .create_asr_session(&installed.id, &AsrSessionSettings::default())
        .await
        .expect("session");

    let removal = manager
        .remove(&installed.id)
        .await
        .expect_err("active removal must fail");
    assert_eq!(removal, ModelError::Busy);

    drop(session);
    // Wait until the guard's Drop has released the model reference.
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if manager.unload(&loaded.id).await.is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "reference was never released");
        tokio::task::yield_now().await;
    }
    manager
        .remove(&installed.id)
        .await
        .expect("remove after release");
}

#[tokio::test]
#[allow(clippy::float_cmp)]
async fn chunk_benchmark_baseline_is_persisted_per_chunk_size() {
    let root = TempDir::new().expect("temp");
    let cache = TempDir::new().expect("cache");
    let manager = KoeModelManager::new(
        root.path(),
        DigestAllowlist::empty(),
        Box::new(FixtureFoundryAdapter::new(cache.path())),
        NetworkPolicy::Denied,
    )
    .expect("manager");
    let installed = manager
        .install(
            &SELECTOR.parse::<ModelSelector>().expect("selector"),
            &install_options(NetworkPolicy::ModelInstallOnly),
        )
        .await
        .expect("install");
    manager.load(&installed.id).await.expect("load");

    let audio = sample_audio(2);
    let reference = fixture_transcribe(&audio);
    for chunk_ms in [80, 160, 560, 1120] {
        let settings = AsrSessionSettings {
            chunk_ms,
            ..AsrSessionSettings::default()
        };
        let baseline = manager
            .run_benchmark(&installed.id, &settings, &audio, &reference)
            .await
            .expect("baseline");
        assert_eq!(baseline.chunk_ms, chunk_ms);
        assert_eq!(baseline.wer_pct, 0.0);
        assert!(!baseline.rtf.is_nan());
    }
    let report = manager.benchmarks(&installed.id).expect("report");
    assert_eq!(report.baselines.len(), 4);
    let mut chunk_sizes = report
        .baselines
        .iter()
        .map(|baseline| baseline.chunk_ms)
        .collect::<Vec<_>>();
    chunk_sizes.sort_unstable();
    assert_eq!(chunk_sizes, vec![80, 160, 560, 1120]);
}

#[tokio::test]
async fn install_rejects_catalog_metadata_that_changed_after_acceptance() {
    let root = TempDir::new().expect("temp");
    let cache = TempDir::new().expect("cache");
    let manager = KoeModelManager::new(
        root.path(),
        DigestAllowlist::empty(),
        Box::new(CountingAdapter::new(FixtureFoundryAdapter::new(
            cache.path(),
        ))),
        NetworkPolicy::Denied,
    )
    .expect("manager");
    let mut accepted = FixtureFoundryAdapter::fixture_descriptor();
    accepted.version = crate::ModelVersion::new("changed-after-consent".to_owned());
    accepted.source = "fixture://changed-after-consent".to_owned();
    let error = manager
        .install(
            &SELECTOR.parse::<ModelSelector>().expect("selector"),
            &InstallOptions {
                accepted_descriptor: Some(accepted),
                ..install_options(NetworkPolicy::ModelInstallOnly)
            },
        )
        .await
        .expect_err("changed descriptor");
    assert_eq!(error, ModelError::LicenseNotAccepted);
    assert_eq!(
        manager.adapter_outbound_attempts().await.expect("calls"),
        1,
        "the descriptor mismatch must be rejected before adapter install"
    );
    assert!(manager.installed_models().expect("installed").is_empty());
}

#[tokio::test]
async fn install_is_idempotent_without_forced_redownload() {
    let root = TempDir::new().expect("temp");
    let cache = TempDir::new().expect("cache");
    let adapter = FixtureFoundryAdapter::new(cache.path());
    let manager = KoeModelManager::new(
        root.path(),
        DigestAllowlist::empty(),
        Box::new(adapter),
        NetworkPolicy::Denied,
    )
    .expect("manager");
    let selector = SELECTOR.parse::<ModelSelector>().expect("selector");
    let first = manager
        .install(&selector, &install_options(NetworkPolicy::ModelInstallOnly))
        .await
        .expect("first install");
    let second = manager
        .install(&selector, &install_options(NetworkPolicy::ModelInstallOnly))
        .await
        .expect("second install");
    assert_eq!(first.id, second.id);
}

#[tokio::test]
async fn install_progress_reaches_done_and_cancel_stops_install() {
    let root = TempDir::new().expect("temp");
    let cache = TempDir::new().expect("cache");
    let manager = KoeModelManager::new(
        root.path(),
        DigestAllowlist::empty(),
        Box::new(FixtureFoundryAdapter::new(cache.path())),
        NetworkPolicy::Denied,
    )
    .expect("manager");
    let (progress, mut receiver) = mpsc::channel(8);
    let installed = manager
        .install(
            &SELECTOR.parse::<ModelSelector>().expect("selector"),
            &InstallOptions {
                policy: NetworkPolicy::ModelInstallOnly,
                cancel: tokio_util::sync::CancellationToken::new(),
                progress: Some(progress),
                accepted_descriptor: None,
                force_redownload: false,
            },
        )
        .await
        .expect("install");
    let mut phases = Vec::new();
    while let Some(phase) = receiver.recv().await {
        phases.push(phase);
    }
    assert_eq!(
        phases,
        vec![
            crate::ModelProgress::Resolving,
            crate::ModelProgress::Downloading,
            crate::ModelProgress::Verifying,
            crate::ModelProgress::Installing,
            crate::ModelProgress::Done,
        ]
    );
    assert_eq!(
        installed.manifest.verification,
        crate::Verification::RuntimeOnly
    );

    // Cancelled install never publishes a manifest.
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let cancelled = manager
        .install(
            &"cancelled-alias"
                .parse::<ModelSelector>()
                .expect("selector"),
            &InstallOptions {
                policy: NetworkPolicy::ModelInstallOnly,
                cancel,
                progress: None,
                accepted_descriptor: None,
                force_redownload: false,
            },
        )
        .await
        .expect_err("cancelled install must fail");
    assert_eq!(cancelled, ModelError::Cancelled);
    assert!(
        manager
            .list(ModelScope::Installed)
            .await
            .expect("installed list")
            .iter()
            .all(|descriptor| descriptor.alias.0 != "cancelled-alias")
    );
}

#[tokio::test]
async fn cancellation_during_an_active_install_returns_promptly_without_publication() {
    let root = TempDir::new().expect("temp");
    let cache = TempDir::new().expect("cache");
    let manager = KoeModelManager::new(
        root.path(),
        DigestAllowlist::empty(),
        Box::new(
            FixtureFoundryAdapter::new(cache.path())
                .with_install_delay(std::time::Duration::from_secs(5)),
        ),
        NetworkPolicy::Denied,
    )
    .expect("manager");
    let cancel = tokio_util::sync::CancellationToken::new();
    let trigger = cancel.clone();
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let error = manager
        .install(
            &SELECTOR.parse::<ModelSelector>().expect("selector"),
            &InstallOptions {
                cancel,
                ..install_options(NetworkPolicy::ModelInstallOnly)
            },
        )
        .await
        .expect_err("cancel active install");
    cancel_task.await.expect("cancel task");
    assert_eq!(error, ModelError::Cancelled);
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert!(manager.installed_models().expect("installed").is_empty());
}

#[tokio::test]
async fn uncancellable_install_is_awaited_and_cleans_only_its_new_artifact() {
    let root = TempDir::new().expect("temp");
    let cache = TempDir::new().expect("cache");
    let delay = std::time::Duration::from_millis(120);
    let manager = KoeModelManager::new(
        root.path(),
        DigestAllowlist::empty(),
        Box::new(
            FixtureFoundryAdapter::new(cache.path())
                .with_install_delay(delay)
                .ignoring_install_cancellation(),
        ),
        NetworkPolicy::Denied,
    )
    .expect("manager");
    let cancel = tokio_util::sync::CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        trigger.cancel();
    });

    let started = Instant::now();
    let error = manager
        .install(
            &SELECTOR.parse::<ModelSelector>().expect("selector"),
            &InstallOptions {
                cancel,
                ..install_options(NetworkPolicy::ModelInstallOnly)
            },
        )
        .await
        .expect_err("cancelled uncancellable install");
    assert_eq!(error, ModelError::Cancelled);
    assert!(started.elapsed() >= delay);
    assert!(manager.installed_models().expect("installed").is_empty());
    assert!(
        !cache.path().join("models").exists()
            || std::fs::read_dir(cache.path().join("models"))
                .expect("models dir")
                .next()
                .is_none(),
        "operation-owned cache artifact must be cleaned"
    );
}

#[tokio::test]
async fn cancellation_never_deletes_a_preexisting_cache_artifact() {
    let root = TempDir::new().expect("temp");
    let cache = TempDir::new().expect("cache");
    let descriptor = FixtureFoundryAdapter::fixture_descriptor();
    let mut seed = FixtureFoundryAdapter::new(cache.path());
    let seeded = seed
        .install(
            &descriptor,
            &tokio_util::sync::CancellationToken::new(),
            false,
        )
        .await
        .expect("seed cache");
    let model_file = seeded
        .files
        .iter()
        .find(|file| file.relative_path.ends_with("model.bin"))
        .expect("model file")
        .absolute_path
        .clone();
    let manager = KoeModelManager::new(
        root.path(),
        DigestAllowlist::empty(),
        Box::new(
            FixtureFoundryAdapter::new(cache.path())
                .with_install_delay(std::time::Duration::from_millis(120))
                .ignoring_install_cancellation(),
        ),
        NetworkPolicy::Denied,
    )
    .expect("manager");
    let cancel = tokio_util::sync::CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        trigger.cancel();
    });

    let error = manager
        .install(
            &SELECTOR.parse::<ModelSelector>().expect("selector"),
            &InstallOptions {
                cancel,
                ..install_options(NetworkPolicy::ModelInstallOnly)
            },
        )
        .await
        .expect_err("cancelled cache hit");
    assert_eq!(error, ModelError::Cancelled);
    assert!(
        model_file.exists(),
        "pre-existing cache content must survive"
    );
    assert!(manager.installed_models().expect("installed").is_empty());
}

#[test]
fn artifact_inventory_rejects_wrong_model_and_paths_outside_cache() {
    let cache = TempDir::new().expect("cache");
    let outside = TempDir::new().expect("outside");
    let outside_file = outside.path().join("secret.bin");
    std::fs::write(&outside_file, b"secret").expect("fixture");
    let descriptor = FixtureFoundryAdapter::fixture_descriptor();
    let artifact_root = cache.path().join("artifact");
    std::fs::create_dir(&artifact_root).expect("artifact root");
    let wrong_model = InstalledArtifact {
        cache_root: cache.path().to_path_buf(),
        artifact_root: artifact_root.clone(),
        model_id: ModelId::new("other/model".to_owned()),
        files: Vec::new(),
        created_by_install: false,
    };
    assert_eq!(
        inventory(&wrong_model, &descriptor),
        Err(ModelError::VerifyFailed)
    );

    let escaped = InstalledArtifact {
        cache_root: cache.path().to_path_buf(),
        artifact_root,
        model_id: descriptor.id.clone(),
        files: vec![InstalledFile {
            absolute_path: outside_file,
            relative_path: "secret.bin".to_owned(),
            size: 6,
            sha256: String::new(),
        }],
        created_by_install: false,
    };
    assert_eq!(
        inventory(&escaped, &descriptor),
        Err(ModelError::PathRejected)
    );
}

#[test]
fn artifact_inventory_hashes_actual_bytes_and_rejects_empty_or_duplicate_paths() {
    let cache = TempDir::new().expect("cache");
    let artifact_root = cache.path().join("artifact");
    std::fs::create_dir(&artifact_root).expect("artifact root");
    let path = artifact_root.join("model.bin");
    std::fs::write(&path, b"abc").expect("fixture");
    let descriptor = FixtureFoundryAdapter::fixture_descriptor();
    let reported = InstalledFile {
        absolute_path: path,
        relative_path: "artifact/model.bin".to_owned(),
        size: 999,
        sha256: "not-authoritative".to_owned(),
    };
    let artifact = InstalledArtifact {
        cache_root: cache.path().to_path_buf(),
        artifact_root: artifact_root.clone(),
        model_id: descriptor.id.clone(),
        files: vec![reported.clone()],
        created_by_install: false,
    };
    assert_eq!(
        inventory(&artifact, &descriptor).expect("inventory"),
        vec![crate::ModelFile {
            path: "artifact/model.bin".to_owned(),
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_owned(),
            size: 3,
        }]
    );

    let empty = InstalledArtifact {
        files: Vec::new(),
        ..artifact.clone()
    };
    assert_eq!(
        inventory(&empty, &descriptor),
        Err(ModelError::VerifyFailed)
    );
    let duplicate = InstalledArtifact {
        files: vec![reported.clone(), reported],
        ..artifact
    };
    assert_eq!(
        inventory(&duplicate, &descriptor),
        Err(ModelError::PathRejected)
    );

    let second_path = artifact_root.join("second.bin");
    std::fs::write(&second_path, b"def").expect("second fixture");
    let two_files = InstalledArtifact {
        cache_root: cache.path().to_path_buf(),
        artifact_root,
        model_id: descriptor.id.clone(),
        files: vec![
            InstalledFile {
                absolute_path: cache.path().join("artifact/model.bin"),
                relative_path: "artifact/model.bin".to_owned(),
                size: 0,
                sha256: String::new(),
            },
            InstalledFile {
                absolute_path: second_path,
                relative_path: "artifact/second.bin".to_owned(),
                size: 0,
                sha256: String::new(),
            },
        ],
        created_by_install: false,
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    assert!(
        super::inventory_from_artifact_with_limits(&two_files, &descriptor, &cancel, 3, 6).is_ok()
    );
    assert_eq!(
        super::inventory_from_artifact_with_limits(&two_files, &descriptor, &cancel, 2, 6),
        Err(ModelError::StoreFailed)
    );
    assert_eq!(
        super::inventory_from_artifact_with_limits(&two_files, &descriptor, &cancel, 3, 5),
        Err(ModelError::StoreFailed)
    );
    cancel.cancel();
    assert_eq!(
        super::inventory_from_artifact_with_limits(&two_files, &descriptor, &cancel, 3, 6),
        Err(ModelError::Cancelled)
    );
}

#[cfg(unix)]
#[test]
fn artifact_inventory_rejects_symlinked_files() {
    use std::os::unix::fs::symlink;

    let cache = TempDir::new().expect("cache");
    let artifact_root = cache.path().join("artifact");
    std::fs::create_dir(&artifact_root).expect("artifact root");
    let target = artifact_root.join("target.bin");
    let link = artifact_root.join("model.bin");
    std::fs::write(&target, b"model").expect("fixture");
    symlink(&target, &link).expect("symlink");
    let descriptor = FixtureFoundryAdapter::fixture_descriptor();
    let artifact = InstalledArtifact {
        cache_root: cache.path().to_path_buf(),
        artifact_root,
        model_id: descriptor.id.clone(),
        files: vec![InstalledFile {
            absolute_path: link,
            relative_path: "artifact/model.bin".to_owned(),
            size: 5,
            sha256: String::new(),
        }],
        created_by_install: false,
    };
    assert_eq!(
        inventory(&artifact, &descriptor),
        Err(ModelError::PathRejected)
    );
}

#[cfg(windows)]
#[test]
fn artifact_inventory_rejects_windows_symlinked_files_when_supported() {
    use std::os::windows::fs::symlink_file;

    let cache = TempDir::new().expect("cache");
    let artifact_root = cache.path().join("artifact");
    std::fs::create_dir(&artifact_root).expect("artifact root");
    let target = artifact_root.join("target.bin");
    let link = artifact_root.join("model.bin");
    std::fs::write(&target, b"model").expect("fixture");
    if symlink_file(&target, &link).is_err() {
        // Windows developer mode or SeCreateSymbolicLinkPrivilege is optional.
        return;
    }
    let descriptor = FixtureFoundryAdapter::fixture_descriptor();
    let artifact = InstalledArtifact {
        cache_root: cache.path().to_path_buf(),
        artifact_root,
        model_id: descriptor.id.clone(),
        files: vec![InstalledFile {
            absolute_path: link,
            relative_path: "artifact/model.bin".to_owned(),
            size: 5,
            sha256: String::new(),
        }],
        created_by_install: false,
    };
    assert_eq!(
        inventory(&artifact, &descriptor),
        Err(ModelError::PathRejected)
    );
}

#[cfg(any(unix, windows))]
#[test]
fn artifact_inventory_rejects_hard_linked_files() {
    let cache = TempDir::new().expect("cache");
    let artifact_root = cache.path().join("artifact");
    std::fs::create_dir(&artifact_root).expect("artifact root");
    let original = artifact_root.join("original.bin");
    let linked = artifact_root.join("model.bin");
    std::fs::write(&original, b"model").expect("fixture");
    std::fs::hard_link(&original, &linked).expect("hard link");
    let descriptor = FixtureFoundryAdapter::fixture_descriptor();
    let artifact = InstalledArtifact {
        cache_root: cache.path().to_path_buf(),
        artifact_root,
        model_id: descriptor.id.clone(),
        files: vec![InstalledFile {
            absolute_path: linked,
            relative_path: "artifact/model.bin".to_owned(),
            size: 5,
            sha256: String::new(),
        }],
        created_by_install: false,
    };
    assert_eq!(
        inventory(&artifact, &descriptor),
        Err(ModelError::PathRejected)
    );
}

#[tokio::test]
async fn load_reverifies_the_runtime_cache_before_opening_the_model() {
    let root = TempDir::new().expect("temp");
    let cache = TempDir::new().expect("cache");
    let manager = KoeModelManager::new(
        root.path(),
        DigestAllowlist::empty(),
        Box::new(FixtureFoundryAdapter::new(cache.path())),
        NetworkPolicy::Denied,
    )
    .expect("manager");
    let installed = manager
        .install(
            &SELECTOR.parse::<ModelSelector>().expect("selector"),
            &install_options(NetworkPolicy::ModelInstallOnly),
        )
        .await
        .expect("install");
    let model_file = installed
        .manifest
        .files
        .iter()
        .find(|file| file.path.ends_with("model.bin"))
        .expect("model file");
    let model_path = cache.path().join(&model_file.path);
    let original = std::fs::read(&model_path).expect("original bytes");
    std::fs::write(&model_path, b"replaced").expect("mutate cache");
    assert!(matches!(
        manager.load(&installed.id).await,
        Err(ModelError::VerifyFailed)
    ));

    std::fs::write(&model_path, original).expect("repair cache");
    manager
        .load(&installed.id)
        .await
        .expect("a failed load rolls back to installed for retry");
}

#[tokio::test]
async fn persisted_manifest_hydrates_as_installed_in_a_new_manager() {
    let root = TempDir::new().expect("temp");
    let cache = TempDir::new().expect("cache");
    let installed = {
        let manager = KoeModelManager::new(
            root.path(),
            DigestAllowlist::empty(),
            Box::new(FixtureFoundryAdapter::new(cache.path())),
            NetworkPolicy::Denied,
        )
        .expect("first manager");
        manager
            .install(
                &SELECTOR.parse::<ModelSelector>().expect("selector"),
                &install_options(NetworkPolicy::ModelInstallOnly),
            )
            .await
            .expect("install")
    };

    let restarted = KoeModelManager::new(
        root.path(),
        DigestAllowlist::empty(),
        Box::new(FixtureFoundryAdapter::new(cache.path())),
        NetworkPolicy::Denied,
    )
    .expect("restarted manager");
    restarted
        .load(&installed.id)
        .await
        .expect("persisted manifest must load after restart");
}

#[tokio::test]
async fn slow_or_dropped_progress_observers_do_not_change_install_success() {
    for drop_receiver in [false, true] {
        let root = TempDir::new().expect("temp");
        let cache = TempDir::new().expect("cache");
        let manager = KoeModelManager::new(
            root.path(),
            DigestAllowlist::empty(),
            Box::new(FixtureFoundryAdapter::new(cache.path())),
            NetworkPolicy::Denied,
        )
        .expect("manager");
        let (progress, receiver) = mpsc::channel(1);
        if drop_receiver {
            drop(receiver);
        }
        let installed = manager
            .install(
                &SELECTOR.parse::<ModelSelector>().expect("selector"),
                &InstallOptions {
                    policy: NetworkPolicy::ModelInstallOnly,
                    cancel: tokio_util::sync::CancellationToken::new(),
                    progress: Some(progress),
                    accepted_descriptor: None,
                    force_redownload: false,
                },
            )
            .await
            .expect("progress delivery is best-effort");
        assert_eq!(
            manager
                .installed_model(&installed.id)
                .expect("published manifest")
                .id,
            installed.id
        );
    }
}
