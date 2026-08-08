//! Model lifecycle state machine from `spec/02-model-runtime.md`.

use serde::{Deserialize, Serialize};

use crate::types::ModelError;

/// States defined by the model runtime specification.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    Absent,
    Resolving,
    Downloading,
    Verifying,
    Installed,
    Loading,
    Ready,
    InUse,
    Unloading,
    Removing,
    Failed,
}

impl ModelState {
    #[must_use]
    #[allow(clippy::unnested_or_patterns)]
    pub const fn allows(
        self,
        next: Self,
    ) -> bool {
        use ModelState::{
            Absent, Downloading, Failed, InUse, Installed, Loading, Ready, Removing, Resolving,
            Unloading, Verifying,
        };
        matches!(
            (self, next),
            (Absent, Resolving | Failed)
                | (Resolving, Downloading | Failed)
                | (Downloading, Verifying | Failed)
                | (Verifying, Installed | Failed)
                | (Installed, Loading | Removing | Failed)
                | (Loading, Ready | Unloading | Failed)
                | (Ready, InUse | Unloading | Failed)
                | (InUse, InUse | Unloading | Failed)
                | (Unloading, Installed | Ready | Failed)
                | (Removing, Absent | Failed)
                | (Failed, Absent)
        )
    }
}

/// Checked model lifecycle. Repeating a terminal-esque state is idempotent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelLifecycle {
    state: ModelState,
}

impl ModelLifecycle {
    /// Starts absent.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: ModelState::Absent,
        }
    }

    /// Restores a lifecycle for an already validated persisted installation.
    pub(crate) const fn persisted_installed() -> Self {
        Self {
            state: ModelState::Installed,
        }
    }

    pub(crate) const fn persisted_ready() -> Self {
        Self {
            state: ModelState::Ready,
        }
    }

    /// Latest model state.
    #[must_use]
    pub const fn state(&self) -> ModelState {
        self.state
    }

    /// Applies a checked transition.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::InvalidTransition`] when the pair is not in the
    /// specification.
    pub const fn transition(
        &mut self,
        next: ModelState,
    ) -> Result<(), ModelError> {
        if !self.state.allows(next) {
            return Err(ModelError::InvalidTransition);
        }
        self.state = next;
        Ok(())
    }
}

impl Default for ModelLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{ModelLifecycle, ModelState};

    #[test]
    fn install_remove_round_trip() {
        let mut lifecycle = ModelLifecycle::new();
        for state in [
            ModelState::Resolving,
            ModelState::Downloading,
            ModelState::Verifying,
            ModelState::Installed,
            ModelState::Removing,
            ModelState::Absent,
        ] {
            assert!(lifecycle.transition(state).is_ok());
        }
        assert_eq!(lifecycle.state(), ModelState::Absent);
    }

    #[test]
    fn load_unload_round_trip() {
        let mut lifecycle = ModelLifecycle {
            state: ModelState::Installed,
        };
        for state in [
            ModelState::Loading,
            ModelState::Ready,
            ModelState::InUse,
            ModelState::Unloading,
            ModelState::Installed,
        ] {
            assert!(lifecycle.transition(state).is_ok());
        }
    }

    #[test]
    fn failed_unload_can_return_to_ready_for_retry() {
        let mut lifecycle = ModelLifecycle {
            state: ModelState::Ready,
        };
        lifecycle
            .transition(ModelState::Unloading)
            .expect("unloading");
        lifecycle.transition(ModelState::Ready).expect("ready");
        assert_eq!(lifecycle.state(), ModelState::Ready);
    }

    #[test]
    fn final_session_release_unloads_to_installed() {
        let mut lifecycle = ModelLifecycle {
            state: ModelState::Ready,
        };
        lifecycle.transition(ModelState::InUse).expect("in use");
        lifecycle
            .transition(ModelState::Unloading)
            .expect("unloading");
        lifecycle
            .transition(ModelState::Installed)
            .expect("installed");
        assert_eq!(lifecycle.state(), ModelState::Installed);
    }

    #[test]
    fn any_state_allows_failed_and_failed_allows_absent() {
        for state in [
            ModelState::Absent,
            ModelState::Resolving,
            ModelState::Downloading,
            ModelState::Verifying,
            ModelState::Installed,
            ModelState::Loading,
            ModelState::Ready,
            ModelState::InUse,
            ModelState::Unloading,
            ModelState::Removing,
        ] {
            let mut lifecycle = ModelLifecycle { state };
            assert!(lifecycle.transition(ModelState::Failed).is_ok());
        }
        let mut lifecycle = ModelLifecycle {
            state: ModelState::Failed,
        };
        assert!(lifecycle.transition(ModelState::Absent).is_ok());
    }

    #[test]
    fn unsupported_transition_is_rejected() {
        let mut lifecycle = ModelLifecycle::new();
        assert!(lifecycle.transition(ModelState::Installed).is_err());
        assert_eq!(lifecycle.state(), ModelState::Absent);
    }
}
