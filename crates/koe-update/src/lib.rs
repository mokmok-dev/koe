//! Signed update metadata, side-by-side install and rollback for koe.
//!
//! Milestone 7 (`spec/08-roadmap.md`) ships production distribution: this
//! crate implements the TUF-style update contract — versioned, expiring,
//! hash-bound, signed metadata — plus an offline store that publishes new
//! app versions side by side, keeps the previous version for rollback and
//! rejects expired, replayed, foreign-platform or tampered inputs.
//!
//! The store itself never touches the network. A caller fetches a release
//! out of band (with explicit consent), hands this crate the signed metadata
//! and the downloaded artifact, and the crate verifies every bound before
//! publishing. See `spec/02-model-runtime.md` and `spec/06-security-and-privacy.md`.

mod signing;
mod store;
mod types;

pub use signing::{
    hex_decode, hex_encode, parse_signed_update, public_key_hex, sign_update, verify_signature,
};
pub use store::{
    QuarantineNote, UpdateStore, built_in_target_triple, file_digest, validate_metadata,
};
pub use types::{
    MAX_METADATA_BYTES, MAX_SIGNATURES, MAX_TARGET_SIZE, MAX_TARGETS, METADATA_SCHEMA,
    SignatureEntry, SignedUpdate, TARGETS_ROLE, UpdateError, UpdateMetadata, UpdateState,
    UpdateStatus, UpdateTarget,
};
