//! Frontend-independent state and policy for the GPUI desktop adapter.

use koe_core::{CapabilityState, PermissionState, SessionState, SourceKind};
use serde::{Deserialize, Serialize};

use crate::{RecordingConsent, SessionSnapshot};

/// Top-level desktop destinations. Recording state deliberately does not live
/// in this enum, so navigating or minimizing cannot hide its indicator.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DesktopPage {
    #[default]
    Setup,
    Recorder,
    Models,
    Sessions,
    Settings,
}

/// Privacy-preserving desktop settings persisted by the outer adapter.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DesktopSettings {
    pub retention_days: Option<u32>,
    pub diagnostics_enabled: bool,
    pub offline_only: bool,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        Self {
            retention_days: None,
            diagnostics_enabled: false,
            offline_only: true,
        }
    }
}

/// One source row shown during setup and permission recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceStatus {
    pub kind: SourceKind,
    pub capability: CapabilityState,
    pub permission: PermissionState,
    pub backend: String,
}

impl SourceStatus {
    /// Whether the source can currently be selected without prompting.
    #[must_use]
    pub const fn ready(&self) -> bool {
        matches!(
            (self.capability, self.permission),
            (
                CapabilityState::Supported,
                PermissionState::Granted
                    | PermissionState::Unobservable
                    | PermissionState::NotApplicable
            ) | (
                CapabilityState::PermissionRequired,
                PermissionState::NotDetermined | PermissionState::Unobservable
            )
        )
    }

    /// Actionable, stable copy for denial and revocation UX.
    #[must_use]
    pub fn guidance(&self) -> &'static str {
        match self.permission {
            PermissionState::Denied | PermissionState::Restricted => {
                "Permission denied. Open system settings, allow access, then refresh."
            },
            PermissionState::Revoked => {
                "Permission was revoked. Recording is stopped; restore access in system settings."
            },
            PermissionState::NotDetermined => {
                "Permission is required. koe will ask only after you continue."
            },
            _ if self.capability == CapabilityState::Unsupported => {
                "This source is unavailable on the current system."
            },
            _ => "Ready",
        }
    }
}

/// State shared by the setup, recorder, model, library and settings views.
#[derive(Clone, Debug)]
pub struct DesktopViewModel {
    page: DesktopPage,
    snapshot: Option<SessionSnapshot>,
    sources: Vec<SourceStatus>,
    settings: DesktopSettings,
    consent: RecordingConsent,
    selected_microphone: Option<String>,
    selected_system_audio: Option<String>,
    selected_model: Option<String>,
    status_message: String,
}

impl Default for DesktopViewModel {
    fn default() -> Self {
        Self {
            page: DesktopPage::Setup,
            snapshot: None,
            sources: Vec::new(),
            settings: DesktopSettings::default(),
            consent: RecordingConsent::default(),
            selected_microphone: None,
            selected_system_audio: None,
            selected_model: None,
            status_message: "Review sources, storage, retention and model before recording."
                .to_owned(),
        }
    }
}

impl DesktopViewModel {
    #[must_use]
    pub const fn page(&self) -> DesktopPage {
        self.page
    }

    pub const fn navigate(
        &mut self,
        page: DesktopPage,
    ) {
        self.page = page;
    }

    #[must_use]
    pub fn sources(&self) -> &[SourceStatus] {
        &self.sources
    }

    pub fn replace_sources(
        &mut self,
        sources: Vec<SourceStatus>,
    ) {
        self.sources = sources;
    }

    #[must_use]
    pub const fn settings(&self) -> DesktopSettings {
        self.settings
    }

    pub const fn set_settings(
        &mut self,
        settings: DesktopSettings,
    ) {
        self.settings = settings;
    }

    pub fn select_microphone(
        &mut self,
        id: impl Into<String>,
    ) {
        self.selected_microphone = Some(id.into());
    }

    pub fn select_system_audio(
        &mut self,
        id: Option<String>,
    ) {
        self.selected_system_audio = id;
    }

    pub fn select_model(
        &mut self,
        id: Option<String>,
    ) {
        self.selected_model = id;
    }

    #[must_use]
    pub fn selected_microphone(&self) -> Option<&str> {
        self.selected_microphone.as_deref()
    }

    #[must_use]
    pub fn selected_system_audio(&self) -> Option<&str> {
        self.selected_system_audio.as_deref()
    }

    #[must_use]
    pub fn selected_model(&self) -> Option<&str> {
        self.selected_model.as_deref()
    }

    /// Records fresh, per-start application consent. It is cleared whenever a
    /// start is consumed, even when the OS subsequently refuses permission.
    pub const fn confirm_recording(&mut self) {
        self.consent = RecordingConsent {
            microphone: true,
            system_audio: self.selected_system_audio.is_some(),
            storage: true,
        };
    }

    pub fn take_recording_consent(&mut self) -> RecordingConsent {
        std::mem::take(&mut self.consent)
    }

    pub fn apply_snapshot(
        &mut self,
        snapshot: SessionSnapshot,
    ) {
        self.status_message = format!("Session state: {:?}", snapshot.state).to_lowercase();
        self.snapshot = Some(snapshot);
    }

    pub fn report_error(
        &mut self,
        code: &str,
        message: &str,
    ) {
        self.status_message = format!("{code}: {message}");
    }

    #[must_use]
    pub fn status_message(&self) -> &str {
        &self.status_message
    }

    #[must_use]
    pub const fn snapshot(&self) -> Option<&SessionSnapshot> {
        self.snapshot.as_ref()
    }

    /// Persistent recording indication used by the window and platform tray.
    #[must_use]
    pub fn recording_indicator(&self) -> Option<&'static str> {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| match snapshot.state {
                SessionState::Starting | SessionState::Recording => Some("RECORDING"),
                SessionState::Degraded => Some("RECORDING - SOURCE DEGRADED"),
                SessionState::Stopping | SessionState::Finalizing => Some("FINALIZING"),
                _ => None,
            })
    }

    #[must_use]
    pub fn can_start(&self) -> bool {
        let microphone_ready = self.selected_microphone.is_some()
            && self
                .sources
                .iter()
                .find(|source| source.kind == SourceKind::Microphone)
                .is_some_and(SourceStatus::ready);
        let system_ready = self.selected_system_audio.is_none()
            || self
                .sources
                .iter()
                .find(|source| source.kind == SourceKind::System)
                .is_some_and(SourceStatus::ready);
        let idle = self.snapshot.as_ref().is_none_or(|snapshot| {
            snapshot.state.is_terminal() || snapshot.state == SessionState::Idle
        });
        microphone_ready && system_ready && idle
    }
}

#[cfg(test)]
mod tests {
    use koe_core::{
        CapabilityState, OperationId, PermissionState, SessionId, SessionState, SourceKind,
    };

    use super::{DesktopPage, DesktopViewModel, SourceStatus};
    use crate::SessionSnapshot;

    fn ready_microphone() -> SourceStatus {
        SourceStatus {
            kind: SourceKind::Microphone,
            capability: CapabilityState::Supported,
            permission: PermissionState::Granted,
            backend: "fixture".to_owned(),
        }
    }

    fn snapshot(state: SessionState) -> SessionSnapshot {
        SessionSnapshot {
            operation_id: OperationId::new(),
            session_id: Some(SessionId::new()),
            state,
        }
    }

    #[test]
    fn recording_indicator_survives_navigation() {
        let mut model = DesktopViewModel::default();
        model.apply_snapshot(snapshot(SessionState::Recording));
        model.navigate(DesktopPage::Settings);
        assert_eq!(model.recording_indicator(), Some("RECORDING"));
    }

    #[test]
    fn consent_is_fresh_for_every_start() {
        let mut model = DesktopViewModel::default();
        model.select_system_audio(Some("system".to_owned()));
        model.confirm_recording();
        let consent = model.take_recording_consent();
        assert!(consent.microphone && consent.system_audio && consent.storage);
        assert_eq!(
            model.take_recording_consent(),
            crate::RecordingConsent::default()
        );
    }

    #[test]
    fn denied_and_revoked_permissions_block_start_with_guidance() {
        let mut model = DesktopViewModel::default();
        model.select_microphone("mic");
        let mut source = ready_microphone();
        source.permission = PermissionState::Revoked;
        assert!(source.guidance().contains("revoked"));
        model.replace_sources(vec![source]);
        assert!(!model.can_start());
    }

    #[test]
    fn conformance_state_uses_shared_session_snapshot() {
        let mut model = DesktopViewModel::default();
        model.select_microphone("mic");
        model.replace_sources(vec![ready_microphone()]);
        assert!(model.can_start());
        model.apply_snapshot(snapshot(SessionState::Recording));
        assert!(!model.can_start());
        model.apply_snapshot(snapshot(SessionState::Completed));
        assert!(model.can_start());
    }
}
