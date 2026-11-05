//! `WASAPI` backend.
//!
//! Enumeration walks `IMMDeviceEnumerator` over the active capture and render
//! endpoints. Capture opens a shared-mode `IAudioClient` in event-driven mode,
//! so `read` blocks on a kernel event instead of polling.

// The Win32 structures and vtable slots below exist for their layout, and the
// operating system owns how much of them this backend names.
#![allow(dead_code)]

use std::ffi::c_void;
use std::marker::PhantomData;
use std::ptr::{from_ref, null, null_mut};

use crate::Source;
use crate::config::StreamSpec;
use crate::device::{DeviceId, DeviceInfo};
use crate::error::Error;
use crate::stream::RawStream;

const COINIT_MULTITHREADED: u32 = 0;
const CLSCTX_ALL: u32 = 0x17;
const STGM_READ: u32 = 0;
const DEVICE_STATE_ACTIVE: u32 = 0x1;
const FLOW_RENDER: u32 = 0;
const FLOW_CAPTURE: u32 = 1;
const ROLE_CONSOLE: u32 = 0;

const AUDCLNT_SHAREMODE_SHARED: u32 = 0;
const AUDCLNT_STREAMFLAGS_LOOPBACK: u32 = 0x0002_0000;
const AUDCLNT_STREAMFLAGS_EVENTCALLBACK: u32 = 0x0004_0000;
const AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;
const AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM: u32 = 0x8000_0000;
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;

const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const BITS_PER_SAMPLE: u16 = 32;
const EXTENSIBLE_EXTRA_SIZE: u16 = 22;
const VT_LPWSTR: u16 = 31;

/// Buffer duration the client asks for, in 100 ns units.
const BUFFER_DURATION: i64 = 10_000_000;
const WAIT_MILLISECONDS: u32 = 2_000;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;

#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

impl Guid {
    const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

const CLSID_MM_DEVICE_ENUMERATOR: Guid = Guid::new(
    0xBCDE_0395,
    0xE52F,
    0x467C,
    [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
);
const IID_IMM_DEVICE_ENUMERATOR: Guid = Guid::new(
    0xA956_64D2,
    0x9614,
    0x4F35,
    [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
);
const IID_IAUDIO_CLIENT: Guid = Guid::new(
    0x1CB9_AD4C,
    0xDBFA,
    0x4C32,
    [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2],
);
const IID_IAUDIO_CAPTURE_CLIENT: Guid = Guid::new(
    0xC8AD_BD64,
    0xE71E,
    0x48A0,
    [0xA4, 0xDE, 0x18, 0x5C, 0x39, 0x5C, 0xD3, 0x17],
);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: Guid = Guid::new(
    0x0000_0003,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

/// `PKEY_Device_FriendlyName`.
const PKEY_DEVICE_FRIENDLY_NAME: PropertyKey = PropertyKey {
    format: Guid::new(
        0xA45C_254E,
        0xDF1C,
        0x4EFD,
        [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0],
    ),
    id: 14,
};

#[repr(C)]
struct PropertyKey {
    format: Guid,
    id: u32,
}

/// A `PROPVARIANT` large enough for the widest member the platform stores.
#[repr(C)]
struct PropVariant {
    value_type: u16,
    reserved: [u16; 3],
    data: [*mut c_void; 2],
}

#[repr(C, packed(1))]
struct WaveFormatEx {
    format_tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    bits_per_sample: u16,
    extra_size: u16,
}

#[repr(C, packed(1))]
struct WaveFormatExtensible {
    format: WaveFormatEx,
    valid_bits_per_sample: u16,
    channel_mask: u32,
    sub_format: Guid,
}

impl WaveFormatExtensible {
    fn new(spec: StreamSpec) -> Self {
        let channels = spec.channels();
        let channel_mask = match channels {
            1 => 0x4,
            2 => 0x3,
            _ => 0,
        };
        Self {
            format: WaveFormatEx {
                format_tag: WAVE_FORMAT_EXTENSIBLE,
                channels,
                samples_per_sec: spec.sample_rate(),
                avg_bytes_per_sec: spec.sample_rate() * u32::from(channels) * 4,
                block_align: channels * 4,
                bits_per_sample: BITS_PER_SAMPLE,
                extra_size: EXTENSIBLE_EXTRA_SIZE,
            },
            valid_bits_per_sample: BITS_PER_SAMPLE,
            channel_mask,
            sub_format: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        }
    }
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, mode: u32) -> i32;
    fn CoUninitialize();
    fn CoCreateInstance(
        class: *const Guid,
        outer: *mut c_void,
        context: u32,
        interface: *const Guid,
        out: *mut *mut c_void,
    ) -> i32;
    fn CoTaskMemFree(pointer: *mut c_void);
    fn PropVariantClear(variant: *mut PropVariant) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(
        attributes: *mut c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}

// Every slot exists so the offsets after it match the COM vtable; the ones this
// backend never calls still have to be declared.

#[repr(C)]
struct UnknownVtable {
    query_interface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct EnumeratorVtable {
    unknown: UnknownVtable,
    enum_audio_endpoints: unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> i32,
    get_default_audio_endpoint:
        unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) -> i32,
    get_device: unsafe extern "system" fn(*mut c_void, *const u16, *mut *mut c_void) -> i32,
    register_endpoint_notification_callback:
        unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32,
    unregister_endpoint_notification_callback:
        unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32,
}

#[repr(C)]
struct CollectionVtable {
    unknown: UnknownVtable,
    get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
    item: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32,
}

#[repr(C)]
struct DeviceVtable {
    unknown: UnknownVtable,
    activate: unsafe extern "system" fn(
        *mut c_void,
        *const Guid,
        u32,
        *mut c_void,
        *mut *mut c_void,
    ) -> i32,
    open_property_store: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32,
    get_id: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> i32,
    get_state: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
}

#[repr(C)]
struct PropertyStoreVtable {
    unknown: UnknownVtable,
    get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
    get_at: unsafe extern "system" fn(*mut c_void, u32, *mut PropertyKey) -> i32,
    get_value: unsafe extern "system" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> i32,
    set_value:
        unsafe extern "system" fn(*mut c_void, *const PropertyKey, *const PropVariant) -> i32,
    commit: unsafe extern "system" fn(*mut c_void) -> i32,
}

#[repr(C)]
struct AudioClientVtable {
    unknown: UnknownVtable,
    initialize: unsafe extern "system" fn(
        *mut c_void,
        u32,
        u32,
        i64,
        i64,
        *const WaveFormatEx,
        *const Guid,
    ) -> i32,
    get_buffer_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
    get_stream_latency: unsafe extern "system" fn(*mut c_void, *mut i64) -> i32,
    get_current_padding: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
    is_format_supported: unsafe extern "system" fn(
        *mut c_void,
        u32,
        *const WaveFormatEx,
        *mut *mut WaveFormatEx,
    ) -> i32,
    get_mix_format: unsafe extern "system" fn(*mut c_void, *mut *mut WaveFormatEx) -> i32,
    get_device_period: unsafe extern "system" fn(*mut c_void, *mut i64, *mut i64) -> i32,
    start: unsafe extern "system" fn(*mut c_void) -> i32,
    stop: unsafe extern "system" fn(*mut c_void) -> i32,
    reset: unsafe extern "system" fn(*mut c_void) -> i32,
    set_event_handle: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32,
    get_service: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
}

#[repr(C)]
struct CaptureClientVtable {
    unknown: UnknownVtable,
    get_buffer: unsafe extern "system" fn(
        *mut c_void,
        *mut *mut u8,
        *mut u32,
        *mut u32,
        *mut u64,
        *mut u64,
    ) -> i32,
    release_buffer: unsafe extern "system" fn(*mut c_void, u32) -> i32,
    get_next_packet_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
}

const fn failed(operation: &'static str, code: i32) -> Error {
    Error::Backend { operation, code }
}

const fn invariant(operation: &'static str) -> Error {
    Error::BackendInvariant { operation }
}

const fn check(operation: &'static str, status: i32) -> Result<(), Error> {
    if status < 0 {
        Err(failed(operation, status))
    } else {
        Ok(())
    }
}

/// Reads the vtable the object points at. The memory belongs to the module.
const unsafe fn vtable_of<T>(object: *mut c_void) -> &'static T {
    unsafe { &*object.cast::<*const T>().read() }
}

unsafe fn wide_string(pointer: *const u16) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    let mut length = 0;
    while unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    let units = unsafe { std::slice::from_raw_parts(pointer, length) };
    Some(String::from_utf16_lossy(units))
}

/// Holds the apartment alive for as long as it exists.
struct Com;

impl Com {
    fn initialize() -> Result<Self, Error> {
        let status = unsafe { CoInitializeEx(null_mut(), COINIT_MULTITHREADED) };
        check("CoInitializeEx", status)?;
        Ok(Self)
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// Frees memory the platform allocated with `CoTaskMemAlloc`.
struct CoTaskMem<T>(*mut T);

impl<T> CoTaskMem<T> {
    const fn as_ptr(&self) -> *mut T {
        self.0
    }
}

impl<T> Drop for CoTaskMem<T> {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(self.0.cast()) };
    }
}

struct EventHandle(*mut c_void);

impl EventHandle {
    fn create() -> Result<Self, Error> {
        let handle = unsafe { CreateEventW(null_mut(), 0, 0, null()) };
        if handle.is_null() {
            return Err(failed("CreateEventW", 0));
        }
        Ok(Self(handle))
    }

    const fn as_handle(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Clears a variant on every path, which also frees a string the callee stored.
struct Variant(PropVariant);

impl Variant {
    const fn empty() -> Self {
        // `PropVariantInit` is a zero fill, which is `VT_EMPTY`.
        Self(unsafe { std::mem::zeroed() })
    }

    const fn value(&self) -> &PropVariant {
        &self.0
    }
}

impl Drop for Variant {
    fn drop(&mut self) {
        unsafe { PropVariantClear(&raw mut self.0) };
    }
}

/// A COM interface this module owns and releases exactly once.
struct Owned<T>(*mut c_void, PhantomData<T>);

impl<T> Owned<T> {
    const fn from_raw(object: *mut c_void) -> Self {
        Self(object, PhantomData)
    }

    const fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    const unsafe fn vtable(&self) -> &'static T {
        unsafe { vtable_of::<T>(self.0) }
    }
}

impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        let vtable = unsafe { vtable_of::<UnknownVtable>(self.0) };
        unsafe { (vtable.release)(self.0) };
    }
}

fn from_com<T>(
    operation: &'static str,
    status: i32,
    object: *mut c_void,
) -> Result<Owned<T>, Error> {
    check(operation, status)?;
    if object.is_null() {
        return Err(invariant(operation));
    }
    Ok(Owned::from_raw(object))
}

impl Owned<EnumeratorVtable> {
    fn create() -> Result<Self, Error> {
        let mut object: *mut c_void = null_mut();
        let status = unsafe {
            CoCreateInstance(
                from_ref(&CLSID_MM_DEVICE_ENUMERATOR),
                null_mut(),
                CLSCTX_ALL,
                from_ref(&IID_IMM_DEVICE_ENUMERATOR),
                &raw mut object,
            )
        };
        from_com("CoCreateInstance", status, object)
    }

    unsafe fn endpoints(&self, flow: u32) -> Result<Owned<CollectionVtable>, Error> {
        let mut object: *mut c_void = null_mut();
        let vtable = unsafe { self.vtable() };
        let status = unsafe {
            (vtable.enum_audio_endpoints)(self.as_ptr(), flow, DEVICE_STATE_ACTIVE, &raw mut object)
        };
        from_com("IMMDeviceEnumerator::EnumAudioEndpoints", status, object)
    }

    unsafe fn default_device_id(&self, flow: u32) -> Option<String> {
        let mut object: *mut c_void = null_mut();
        let vtable = unsafe { self.vtable() };
        let status = unsafe {
            (vtable.get_default_audio_endpoint)(self.as_ptr(), flow, ROLE_CONSOLE, &raw mut object)
        };
        let device = from_com::<DeviceVtable>(
            "IMMDeviceEnumerator::GetDefaultAudioEndpoint",
            status,
            object,
        )
        .ok()?;
        unsafe { device.id() }.ok()
    }

    unsafe fn device(&self, id: *const u16) -> Result<Owned<DeviceVtable>, Error> {
        let mut object: *mut c_void = null_mut();
        let vtable = unsafe { self.vtable() };
        let status = unsafe { (vtable.get_device)(self.as_ptr(), id, &raw mut object) };
        from_com("IMMDeviceEnumerator::GetDevice", status, object)
    }
}

impl Owned<CollectionVtable> {
    unsafe fn count(&self) -> Result<u32, Error> {
        let mut count = 0;
        let vtable = unsafe { self.vtable() };
        let status = unsafe { (vtable.get_count)(self.as_ptr(), &raw mut count) };
        check("IMMDeviceCollection::GetCount", status)?;
        Ok(count)
    }

    unsafe fn item(&self, index: u32) -> Result<Owned<DeviceVtable>, Error> {
        let mut object: *mut c_void = null_mut();
        let vtable = unsafe { self.vtable() };
        let status = unsafe { (vtable.item)(self.as_ptr(), index, &raw mut object) };
        from_com("IMMDeviceCollection::Item", status, object)
    }
}

impl Owned<DeviceVtable> {
    unsafe fn id(&self) -> Result<String, Error> {
        let mut pointer: *mut u16 = null_mut();
        let vtable = unsafe { self.vtable() };
        let status = unsafe { (vtable.get_id)(self.as_ptr(), &raw mut pointer) };
        check("IMMDevice::GetId", status)?;
        if pointer.is_null() {
            return Err(invariant("IMMDevice::GetId"));
        }
        let id = CoTaskMem(pointer);
        unsafe { wide_string(id.as_ptr()) }.ok_or_else(|| invariant("IMMDevice::GetId"))
    }

    unsafe fn friendly_name(&self) -> Option<String> {
        let mut object: *mut c_void = null_mut();
        let vtable = unsafe { self.vtable() };
        let status =
            unsafe { (vtable.open_property_store)(self.as_ptr(), STGM_READ, &raw mut object) };
        let store =
            from_com::<PropertyStoreVtable>("IMMDevice::OpenPropertyStore", status, object).ok()?;
        let mut variant = Variant::empty();
        let vtable = unsafe { store.vtable() };
        let status = unsafe {
            (vtable.get_value)(
                store.as_ptr(),
                from_ref(&PKEY_DEVICE_FRIENDLY_NAME),
                &raw mut variant.0,
            )
        };
        if status < 0 || variant.value().value_type != VT_LPWSTR {
            return None;
        }
        unsafe { wide_string(variant.value().data[0].cast::<u16>()) }
    }

    unsafe fn audio_client(&self) -> Result<Owned<AudioClientVtable>, Error> {
        let mut object: *mut c_void = null_mut();
        let vtable = unsafe { self.vtable() };
        let status = unsafe {
            (vtable.activate)(
                self.as_ptr(),
                from_ref(&IID_IAUDIO_CLIENT),
                CLSCTX_ALL,
                null_mut(),
                &raw mut object,
            )
        };
        from_com("IMMDevice::Activate", status, object)
    }

    unsafe fn mix_format(&self) -> Result<(u32, u16), Error> {
        let client = unsafe { self.audio_client() }?;
        let mut pointer: *mut WaveFormatEx = null_mut();
        let vtable = unsafe { client.vtable() };
        let status = unsafe { (vtable.get_mix_format)(client.as_ptr(), &raw mut pointer) };
        check("IAudioClient::GetMixFormat", status)?;
        if pointer.is_null() {
            return Err(invariant("IAudioClient::GetMixFormat"));
        }
        let format = CoTaskMem(pointer);
        let format = unsafe { format.as_ptr().read() };
        if format.channels == 0 {
            return Err(invariant("IAudioClient::GetMixFormat"));
        }
        Ok((format.samples_per_sec, format.channels))
    }
}

impl Owned<AudioClientVtable> {
    unsafe fn initialize(&self, format: &WaveFormatExtensible, flags: u32) -> Result<(), Error> {
        let vtable = unsafe { self.vtable() };
        let status = unsafe {
            (vtable.initialize)(
                self.as_ptr(),
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                BUFFER_DURATION,
                0,
                from_ref(format).cast(),
                null(),
            )
        };
        check("IAudioClient::Initialize", status)
    }

    unsafe fn set_event(&self, event: *mut c_void) -> Result<(), Error> {
        let vtable = unsafe { self.vtable() };
        let status = unsafe { (vtable.set_event_handle)(self.as_ptr(), event) };
        check("IAudioClient::SetEventHandle", status)
    }

    unsafe fn capture_client(&self) -> Result<Owned<CaptureClientVtable>, Error> {
        let mut object: *mut c_void = null_mut();
        let vtable = unsafe { self.vtable() };
        let status = unsafe {
            (vtable.get_service)(
                self.as_ptr(),
                from_ref(&IID_IAUDIO_CAPTURE_CLIENT),
                &raw mut object,
            )
        };
        from_com("IAudioClient::GetService", status, object)
    }

    unsafe fn start(&self) -> Result<(), Error> {
        let vtable = unsafe { self.vtable() };
        let status = unsafe { (vtable.start)(self.as_ptr()) };
        check("IAudioClient::Start", status)
    }

    unsafe fn stop(&self) {
        let vtable = unsafe { self.vtable() };
        unsafe { (vtable.stop)(self.as_ptr()) };
    }
}

unsafe fn describe(
    device: &Owned<DeviceVtable>,
    source: Source,
    default: Option<&str>,
) -> Result<Option<DeviceInfo>, Error> {
    let raw = unsafe { device.id() }?;
    let is_default = default == Some(raw.as_str());
    let name = unsafe { device.friendly_name() }.unwrap_or_else(|| raw.clone());
    let Ok((sample_rate, channels)) = (unsafe { device.mix_format() }) else {
        return Ok(None);
    };
    let Ok(id) = DeviceId::new(raw) else {
        return Ok(None);
    };
    Ok(Some(DeviceInfo::new(
        id,
        name,
        source,
        is_default,
        sample_rate,
        channels,
    )))
}

pub fn devices() -> Result<Vec<DeviceInfo>, Error> {
    let _com = Com::initialize()?;
    let enumerator = Owned::<EnumeratorVtable>::create()?;
    let mut devices = Vec::new();
    for (flow, source) in [
        (FLOW_CAPTURE, Source::Microphone),
        (FLOW_RENDER, Source::SystemAudio),
    ] {
        let default = unsafe { enumerator.default_device_id(flow) };
        let collection = unsafe { enumerator.endpoints(flow) }?;
        let count = unsafe { collection.count() }?;
        for index in 0..count {
            let Ok(device) = (unsafe { collection.item(index) }) else {
                continue;
            };
            if let Some(info) = unsafe { describe(&device, source, default.as_deref()) }? {
                devices.push(info);
            }
        }
    }
    Ok(devices)
}

struct WasapiStream {
    client: Owned<AudioClientVtable>,
    capture: Owned<CaptureClientVtable>,
    event: EventHandle,
    _com: Com,
    pending: Vec<f32>,
    front: usize,
    channels: usize,
}

impl WasapiStream {
    /// Returns false when the device produced no packet within the wait.
    ///
    /// A loopback endpoint delivers nothing while the machine is silent, so a
    /// timeout is silence rather than a failure.
    fn wait_for_packet(&self) -> Result<bool, Error> {
        loop {
            let mut frames = 0;
            let vtable = unsafe { self.capture.vtable() };
            let status =
                unsafe { (vtable.get_next_packet_size)(self.capture.as_ptr(), &raw mut frames) };
            check("IAudioCaptureClient::GetNextPacketSize", status)?;
            if frames > 0 {
                return Ok(true);
            }
            match unsafe { WaitForSingleObject(self.event.as_handle(), WAIT_MILLISECONDS) } {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => return Ok(false),
                code => return Err(failed("WaitForSingleObject", code.cast_signed())),
            }
        }
    }

    #[allow(clippy::cast_ptr_alignment)]
    fn store(&mut self, samples: *const u8, frames: u32, flags: u32) -> Result<(), Error> {
        let count = usize::try_from(frames)
            .ok()
            .and_then(|frames| frames.checked_mul(self.channels))
            .ok_or_else(|| invariant("IAudioCaptureClient::GetBuffer"))?;
        if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 {
            self.pending.resize(self.pending.len() + count, 0.0);
            return Ok(());
        }
        if samples.is_null() {
            return Err(invariant("IAudioCaptureClient::GetBuffer"));
        }
        // The packet is aligned for the sample type the client was opened with.
        let values = unsafe { std::slice::from_raw_parts(samples.cast::<f32>(), count) };
        self.pending.extend_from_slice(values);
        Ok(())
    }

    fn receive(&mut self) -> Result<bool, Error> {
        if !self.wait_for_packet()? {
            return Ok(false);
        }
        let mut samples: *mut u8 = null_mut();
        let mut frames = 0;
        let mut flags = 0;
        let mut device_position = 0;
        let mut qpc_position = 0;
        let vtable = unsafe { self.capture.vtable() };
        let status = unsafe {
            (vtable.get_buffer)(
                self.capture.as_ptr(),
                &raw mut samples,
                &raw mut frames,
                &raw mut flags,
                &raw mut device_position,
                &raw mut qpc_position,
            )
        };
        check("IAudioCaptureClient::GetBuffer", status)?;
        let stored = self.store(samples, frames, flags);
        let status = unsafe { (vtable.release_buffer)(self.capture.as_ptr(), frames) };
        check("IAudioCaptureClient::ReleaseBuffer", status)?;
        stored.map(|()| true)
    }

    fn drain(&mut self, out: &mut [f32]) -> usize {
        let available = self.pending.len() - self.front;
        let taken = available.min(out.len());
        out[..taken].copy_from_slice(&self.pending[self.front..self.front + taken]);
        self.front += taken;
        if self.front == self.pending.len() {
            self.pending.clear();
            self.front = 0;
        }
        taken
    }
}

impl RawStream for WasapiStream {
    fn read(&mut self, out: &mut [f32]) -> Result<usize, Error> {
        if out.is_empty() {
            return Err(Error::BufferTooSmall { frames: 0 });
        }
        if !out.len().is_multiple_of(self.channels) {
            return Err(invariant("read"));
        }
        let mut filled = 0;
        while filled < out.len() {
            filled += self.drain(&mut out[filled..]);
            if filled < out.len() && !self.receive()? {
                out[filled..].fill(0.0);
                filled = out.len();
            }
        }
        Ok(filled / self.channels)
    }
}

impl Drop for WasapiStream {
    fn drop(&mut self) {
        unsafe { self.client.stop() };
    }
}

pub fn open(device: &DeviceInfo, spec: StreamSpec) -> Result<Box<dyn RawStream>, Error> {
    let com = Com::initialize()?;
    let enumerator = Owned::<EnumeratorVtable>::create()?;
    let mut id: Vec<u16> = device.id().as_str().encode_utf16().collect();
    id.push(0);
    let endpoint = unsafe { enumerator.device(id.as_ptr()) }?;
    let client = unsafe { endpoint.audio_client() }?;

    let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    if spec.source() == Source::SystemAudio {
        flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }
    let format = WaveFormatExtensible::new(spec);
    unsafe { client.initialize(&format, flags)? };

    let event = EventHandle::create()?;
    unsafe { client.set_event(event.as_handle())? };
    let capture = unsafe { client.capture_client() }?;
    unsafe { client.start()? };

    Ok(Box::new(WasapiStream {
        client,
        capture,
        event,
        _com: com,
        pending: Vec::new(),
        front: 0,
        channels: usize::from(spec.channels()),
    }))
}
