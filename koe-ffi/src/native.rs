//! Swift-side native provider registration and delegation.

use std::sync::{Arc, OnceLock};

use crate::types::{AppInfo, Permission, PermissionStatus};

/// macOS framework bridge implemented by `koe-native` on the Swift side.
#[uniffi::export(callback_interface)]
pub trait NativeProvider: Send + Sync {
    fn check_permission(
        &self,
        permission: Permission,
    ) -> PermissionStatus;
    fn request_permission(
        &self,
        permission: Permission,
    ) -> PermissionStatus;
    fn enumerate_apps(&self) -> Vec<AppInfo>;
}

static NATIVE_PROVIDER: OnceLock<Arc<dyn NativeProvider>> = OnceLock::new();

/// Registers the Swift implementation of macOS framework calls.
///
/// Must be called once before any other FFI entry point that touches native
/// APIs. Safe to call multiple times; only the first registration is kept.
#[uniffi::export]
pub fn register_native_provider(provider: Box<dyn NativeProvider>) {
    let _ = NATIVE_PROVIDER.set(Arc::from(provider));
}

pub fn provider() -> Option<&'static Arc<dyn NativeProvider>> {
    NATIVE_PROVIDER.get()
}
