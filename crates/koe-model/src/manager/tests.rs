//! Milestone 3 acceptance tests driven by the fixture adapter.

use std::{
    sync::{Arc, atomic::AtomicUsize},
    time::Instant,
};

use koe_core::NetworkPolicy;
use tempfile::TempDir;
use tokio::sync::mpsc;

use crate::{
    AsrSessionSettings, DigestAllowlist, FixtureFoundryAdapter, InstalledModelId, KoeModelManager,
    ModelError, ModelManager, ModelScope, ModelSelector,
    fixture::CountingAdapter,
    fixture_transcribe,
    types::{InstallOptions, LoadedModelId, ModelId},
};

const SELECTOR: &str = "fixture-nemotron-asr-0.6b";

fn install_options(policy: NetworkPolicy) -> InstallOptions {
    InstallOptions {
        policy,
        cancel: tokio_util::sync::CancellationToken::new(),
        progress: None,
        expected_descriptor: None,
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
async fn loaded_model_with_shared_alias_but_distinct_id_does_not_block_install() {
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
    let mut other = installed.descriptor.clone();
    other.id = ModelId::new("distinct-runtime-id".to_owned());
    manager.state.write().await.loaded.insert(
        LoadedModelId::new(),
        super::LoadedRecord {
            installed: InstalledModelId::new(),
            descriptor: other,
            references: Arc::new(AtomicUsize::new(0)),
        },
    );

    let repeated = manager
        .install(
            &SELECTOR.parse::<ModelSelector>().expect("selector"),
            &install_options(NetworkPolicy::ModelInstallOnly),
        )
        .await
        .expect("shared alias is not identity");
    assert_eq!(repeated.id, installed.id);
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
        .expect("finished session releases exactly one reference");
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

    let error = manager
        .install(
            &SELECTOR.parse::<ModelSelector>().expect("selector"),
            &install_options(NetworkPolicy::Denied),
        )
        .await
        .expect_err("denied install must fail");
    assert_eq!(error.code(), "KOE-MODEL-NETWORK-DENIED");

    let missing = manager
        .resolve(&SELECTOR.parse::<ModelSelector>().expect("selector"))
        .await
        .expect_err("offline resolve must fail");
    assert_eq!(missing, ModelError::OfflineArtifactMissing);

    let catalog = manager
        .list(ModelScope::Catalog)
        .await
        .expect_err("catalog offline must fail");
    assert_eq!(catalog.code(), "KOE-MODEL-NETWORK-DENIED");

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
async fn closed_progress_channel_does_not_fail_install() {
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
    drop(receiver);
    manager
        .install(
            &SELECTOR.parse::<ModelSelector>().expect("selector"),
            &InstallOptions {
                policy: NetworkPolicy::ModelInstallOnly,
                cancel: tokio_util::sync::CancellationToken::new(),
                progress: Some(progress),
                expected_descriptor: None,
                force_redownload: false,
            },
        )
        .await
        .expect("progress observers cannot change install success");
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
                expected_descriptor: None,
                force_redownload: false,
            },
        )
        .await
        .expect("install");
    let mut phases = Vec::new();
    while let Some(phase) = receiver.recv().await {
        phases.push(phase);
    }
    assert_eq!(phases.first(), Some(&crate::ModelProgress::Resolving));
    assert_eq!(phases.last(), Some(&crate::ModelProgress::Done));
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
                expected_descriptor: None,
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
