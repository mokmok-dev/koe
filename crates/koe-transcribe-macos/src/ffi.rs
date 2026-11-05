//! `koe_speech_shim` bindings and their [`Shim`] implementation.
//!
//! The only unsafe code in this crate builds and consumes the shim's C ABI.

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char};
use std::ptr::{null, null_mut};
use std::slice;
use std::str;

use crate::pipeline::{CommittedPhrase, Shim, VolatileTail};

/// Opaque handle the shim hands back.
#[repr(C)]
struct KoeSttEngine {
    _opaque: [u8; 0],
}

/// Layout of the shim's `koe_stt_string`.
#[repr(C)]
struct KoeSttString {
    data: *const c_char,
    length: u32,
}

unsafe extern "C" {
    fn koe_stt_engine_create(locale: *const c_char, out: *mut *mut KoeSttEngine) -> i32;
    fn koe_stt_engine_feed(engine: *mut KoeSttEngine, pcm: *const f32, frames: u32) -> i32;
    fn koe_stt_engine_take_committed(
        engine: *mut KoeSttEngine,
        text: *mut KoeSttString,
        start_ms: *mut i64,
        end_ms: *mut i64,
    ) -> i32;
    fn koe_stt_engine_take_volatile(
        engine: *mut KoeSttEngine,
        text: *mut KoeSttString,
        revision: *mut u32,
    ) -> i32;
    fn koe_stt_engine_finish(engine: *mut KoeSttEngine) -> i32;
    fn koe_stt_engine_destroy(engine: *mut KoeSttEngine);
}

/// A live shim engine handle.
///
/// The shim guards its shared state with one `NSLock`, and each handle is used
/// from one thread at a time (`EngineSession: Send`).
pub struct EngineHandle(*mut KoeSttEngine);

// The shim engine is moved to the session thread after `create`.
unsafe impl Send for EngineHandle {}

/// The real shim.
#[derive(Clone)]
pub struct SpeechShim;

impl Shim for SpeechShim {
    type Handle = EngineHandle;

    fn create(&self, locale: Option<&CStr>) -> Result<EngineHandle, i32> {
        let locale = locale.map_or(null(), CStr::as_ptr);
        let mut handle: *mut KoeSttEngine = null_mut();
        let status = unsafe { koe_stt_engine_create(locale, &raw mut handle) };
        if status != 0 {
            return Err(status);
        }
        if handle.is_null() {
            return Err(-4);
        }
        Ok(EngineHandle(handle))
    }

    fn feed(&self, handle: &mut EngineHandle, pcm: &[f32]) -> Result<(), i32> {
        // Feeds past u32::MAX frames go out in chunks; the shim consumes each
        // buffer fully, so no audio is lost between them.
        let mut rest = pcm;
        while !rest.is_empty() {
            let frames = rest.len().min(u32::MAX as usize);
            let count = u32::try_from(frames).unwrap_or(u32::MAX);
            let status = unsafe { koe_stt_engine_feed(handle.0, rest.as_ptr(), count) };
            if status != 0 {
                return Err(status);
            }
            rest = &rest[frames..];
        }
        Ok(())
    }

    fn take_committed(&self, handle: &mut EngineHandle) -> Result<Option<CommittedPhrase>, i32> {
        let mut text = KoeSttString {
            data: null(),
            length: 0,
        };
        let mut start_ms = 0_i64;
        let mut end_ms = 0_i64;
        let status = unsafe {
            koe_stt_engine_take_committed(
                handle.0,
                &raw mut text,
                &raw mut start_ms,
                &raw mut end_ms,
            )
        };
        match status {
            0 => Ok(None),
            1 => Ok(Some(CommittedPhrase {
                text: str_from(&text)?.to_owned(),
                start_ms,
                end_ms,
            })),
            status => Err(status),
        }
    }

    fn take_volatile(&self, handle: &mut EngineHandle) -> Result<Option<VolatileTail>, i32> {
        let mut text = KoeSttString {
            data: null(),
            length: 0,
        };
        let mut revision = 0_u32;
        let status =
            unsafe { koe_stt_engine_take_volatile(handle.0, &raw mut text, &raw mut revision) };
        match status {
            0 => Ok(None),
            1 => Ok(Some(VolatileTail {
                text: str_from(&text)?.to_owned(),
                revision,
            })),
            status => Err(status),
        }
    }

    fn finish(&self, handle: &mut EngineHandle) -> Result<(), i32> {
        match unsafe { koe_stt_engine_finish(handle.0) } {
            0 => Ok(()),
            status => Err(status),
        }
    }

    fn destroy(&self, handle: EngineHandle) {
        unsafe { koe_stt_engine_destroy(handle.0) };
    }
}

/// Reads the handed-out string before the next call on the handle frees it.
///
/// A length past a null data pointer, or bytes outside UTF-8, cannot come from
/// the shim's own `strdup` of its Swift strings, so both break the contract.
fn str_from(text: &KoeSttString) -> Result<&str, i32> {
    if text.length == 0 {
        return Ok("");
    }
    if text.data.is_null() {
        return Err(-5);
    }
    let bytes = unsafe { slice::from_raw_parts(text.data.cast::<u8>(), text.length as usize) };
    str::from_utf8(bytes).map_err(|_| -5)
}
