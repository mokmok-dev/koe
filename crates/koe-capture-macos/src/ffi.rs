//! `CoreAudio` bindings and the two public entry points.

#![allow(unsafe_code)]

use koe_capture_core::{DeviceInfo, Error, RawStream, StreamSpec};

use std::ffi::CString;
use std::ffi::{CStr, c_char, c_void};
use std::mem::size_of;
use std::ptr::{addr_of, from_ref, null, null_mut};
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Instant;

use crate::enumerate::{
    Defaults, DeviceRecord, SYSTEM_PREFIX, Target, check_spec, describe_device, resolve,
    total_channels,
};
use crate::pipeline::{self, CALLBACK_FRAMES, Poll, QUEUE_FRAMES, RETRY, SAMPLE_BYTES, Sink};

const SYSTEM_OBJECT: u32 = 1;
const ELEMENT_MAIN: u32 = 0;
const PARAMETER_ERROR: i32 = -50;
const TEXT_ENCODING_UTF8: u32 = 0x0800_0100;
const NATIVE_ENDIAN_FLAG: u32 = if cfg!(target_endian = "big") { 2 } else { 0 };
/// Four-character code of a `CoreAudio` selector or scope.
const fn code(text: [u8; 4]) -> u32 {
    u32::from_be_bytes(text)
}
const SCOPE_GLOBAL: u32 = code(*b"glob");
const SCOPE_INPUT: u32 = code(*b"inpt");
const SCOPE_OUTPUT: u32 = code(*b"outp");
const DEVICE_LIST: u32 = code(*b"dev#");
const DEFAULT_INPUT: u32 = code(*b"dIn ");
const DEFAULT_OUTPUT: u32 = code(*b"dOut");
const STREAM_CONFIGURATION: u32 = code(*b"slay");
const OBJECT_NAME: u32 = code(*b"lnam");
const DEVICE_UID: u32 = code(*b"uid ");
const NOMINAL_SAMPLE_RATE: u32 = code(*b"nsrt");
const UNIT_TYPE_OUTPUT: u32 = code(*b"auou");
const UNIT_SUBTYPE_HAL_OUTPUT: u32 = code(*b"ahal");
const UNIT_MANUFACTURER_APPLE: u32 = code(*b"appl");
const FORMAT_LINEAR_PCM: u32 = code(*b"lpcm");
const FORMAT_FLAG_FLOAT: u32 = 1;
const FORMAT_FLAG_PACKED: u32 = 8;
const UNIT_SCOPE_GLOBAL: u32 = 0;
const UNIT_SCOPE_INPUT: u32 = 1;
const UNIT_SCOPE_OUTPUT: u32 = 2;
const INPUT_ELEMENT: u32 = 1;
const OUTPUT_ELEMENT: u32 = 0;
const UNIT_STREAM_FORMAT: u32 = 8;
const UNIT_MAX_FRAMES_PER_SLICE: u32 = 14;
const UNIT_CURRENT_DEVICE: u32 = 2_000;
const UNIT_ENABLE_IO: u32 = 2_003;
const UNIT_INPUT_CALLBACK: u32 = 2_005;
const AGGREGATE_UID_KEY: &str = "uid";
const AGGREGATE_NAME_KEY: &str = "name";
const AGGREGATE_TAP_LIST_KEY: &str = "taps";
const AGGREGATE_PRIVATE_KEY: &str = "private";
const SUB_TAP_UID_KEY: &str = "uid";
const SUB_TAP_DRIFT_KEY: &str = "drift";

#[repr(C)]
struct Address {
    selector: u32,
    scope: u32,
    element: u32,
}
#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}
/// Header of the variable-length `AudioBufferList`; trailing buffers are
/// reached through `buffers`.
#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}
#[repr(C)]
struct AudioComponentDescription {
    kind: u32,
    sub_type: u32,
    manufacturer: u32,
    flags: u32,
    flags_mask: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

type RenderProc =
    extern "C" fn(*mut c_void, *mut u32, *const c_void, u32, u32, *mut AudioBufferList) -> i32;
#[repr(C)]
struct RenderCallback {
    input_proc: Option<RenderProc>,
    reference: *mut c_void,
}
/// Callback tables of the `CFType` collections. Only their addresses are used.
#[repr(C)]
struct Callbacks([usize; 6]);

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object: u32,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
    ) -> i32;
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn AudioHardwareCreateAggregateDevice(description: *const c_void, device: *mut u32) -> i32;
    fn AudioHardwareDestroyAggregateDevice(device: u32) -> i32;
}

// The process-tap entry points only exist from macOS 14.2 on, so they are
// resolved by name; a hard link would make the whole binary fail to launch on
// 14.0 and 14.1.
unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}
const RTLD_DEFAULT: *mut c_void = (-2isize).cast_unsigned() as *mut c_void;
type CreateTap = unsafe extern "C" fn(*mut c_void, *mut u32) -> i32;
type DestroyTap = unsafe extern "C" fn(u32) -> i32;
#[derive(Clone, Copy)]
struct TapApi {
    create: CreateTap,
    destroy: DestroyTap,
}
fn tap_api() -> Option<TapApi> {
    let create = unsafe { dlsym(RTLD_DEFAULT, c"AudioHardwareCreateProcessTap".as_ptr()) };
    let destroy = unsafe { dlsym(RTLD_DEFAULT, c"AudioHardwareDestroyProcessTap".as_ptr()) };
    if create.is_null() || destroy.is_null() {
        return None;
    }
    Some(TapApi {
        create: unsafe { std::mem::transmute::<*mut c_void, CreateTap>(create) },
        destroy: unsafe { std::mem::transmute::<*mut c_void, DestroyTap>(destroy) },
    })
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(
        component: *mut c_void,
        d: *const AudioComponentDescription,
    ) -> *mut c_void;
    fn AudioComponentInstanceNew(component: *mut c_void, instance: *mut *mut c_void) -> i32;
    fn AudioComponentInstanceDispose(instance: *mut c_void) -> i32;
    fn AudioUnitSetProperty(
        unit: *mut c_void,
        p: u32,
        s: u32,
        e: u32,
        d: *const c_void,
        n: u32,
    ) -> i32;
    fn AudioUnitInitialize(unit: *mut c_void) -> i32;
    fn AudioUnitUninitialize(unit: *mut c_void) -> i32;
    fn AudioUnitRender(
        unit: *mut c_void,
        f: *mut u32,
        t: *const c_void,
        b: u32,
        n: u32,
        d: *mut AudioBufferList,
    ) -> i32;
    fn AudioOutputUnitStart(unit: *mut c_void) -> i32;
    fn AudioOutputUnitStop(unit: *mut c_void) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(a: *const c_void, t: *const c_char, e: u32) -> *const c_void;
    fn CFStringGetLength(text: *const c_void) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(t: *const c_void, b: *mut c_char, n: isize, e: u32) -> u8;
    fn CFDictionaryCreateMutable(
        a: *const c_void,
        n: isize,
        k: *const Callbacks,
        v: *const Callbacks,
    ) -> *const c_void;
    fn CFDictionarySetValue(dictionary: *const c_void, key: *const c_void, value: *const c_void);
    fn CFArrayCreate(
        a: *const c_void,
        v: *const *const c_void,
        n: isize,
        c: *const Callbacks,
    ) -> *const c_void;
    fn CFRelease(value: *const c_void);
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: Callbacks;
    static kCFTypeDictionaryValueCallBacks: Callbacks;
    static kCFTypeArrayCallBacks: Callbacks;
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}
const fn backend(operation: &'static str, code: i32) -> Error {
    Error::Backend { operation, code }
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
const fn main(selector: u32, scope: u32) -> Address {
    Address {
        selector,
        scope,
        element: ELEMENT_MAIN,
    }
}
/// Reads a property into word-aligned storage, returning the size it reports.
fn property(object: u32, address: &Address) -> Result<(u32, Vec<u64>), Error> {
    let mut size = 0;
    let status =
        unsafe { AudioObjectGetPropertyDataSize(object, address, 0, null(), &raw mut size) };
    if status != 0 {
        return Err(backend("AudioObjectGetPropertyDataSize", status));
    }
    // One spare word keeps `value` off a dangling pointer for an empty property.
    let mut storage = vec![0u64; (size as usize).div_ceil(size_of::<u64>()).max(1)];
    let mut written = size;
    let status = unsafe {
        let data = storage.as_mut_ptr().cast();
        AudioObjectGetPropertyData(object, address, 0, null(), &raw mut written, data)
    };
    if status == 0 {
        Ok((size, storage))
    } else {
        Err(backend("AudioObjectGetPropertyData", status))
    }
}
fn value<T: Copy>(object: u32, address: &Address) -> Result<T, Error> {
    let (_, storage) = property(object, address)?;
    Ok(unsafe { storage.as_ptr().cast::<T>().read() })
}

struct Cf(*const c_void);
impl Cf {
    const fn new(value: *const c_void, operation: &'static str) -> Result<Self, Error> {
        if value.is_null() {
            Err(backend(operation, PARAMETER_ERROR))
        } else {
            Ok(Self(value))
        }
    }
    const fn raw(&self) -> *const c_void {
        self.0
    }
}
impl Drop for Cf {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

fn cf_read(text: *const c_void) -> Option<String> {
    if text.is_null() {
        return None;
    }
    let length = unsafe { CFStringGetLength(text) };
    let maximum = unsafe { CFStringGetMaximumSizeForEncoding(length, TEXT_ENCODING_UTF8) };
    if maximum <= 0 {
        return None;
    }
    let mut buffer = vec![0u8; usize::try_from(maximum).ok()?.checked_add(1)?];
    let size = isize::try_from(buffer.len()).ok()?;
    let filled = unsafe {
        let buffer = buffer.as_mut_ptr().cast();
        CFStringGetCString(text, buffer, size, TEXT_ENCODING_UTF8)
    };
    if filled == 0 {
        return None;
    }
    let text = unsafe { CStr::from_ptr(buffer.as_ptr().cast()) };
    Some(text.to_string_lossy().into_owned())
}
fn cf_make(text: &str) -> Result<Cf, Error> {
    let text =
        CString::new(text).map_err(|_| backend("CFStringCreateWithCString", PARAMETER_ERROR))?;
    Cf::new(
        unsafe { CFStringCreateWithCString(null(), text.as_ptr(), TEXT_ENCODING_UTF8) },
        "CFStringCreateWithCString",
    )
}
fn text_property(object: u32, selector: u32) -> Option<String> {
    let pointer: *const c_void = value(object, &main(selector, SCOPE_GLOBAL)).ok()?;
    cf_read(Cf::new(pointer, "AudioObjectGetPropertyData").ok()?.raw())
}
fn channel_count(object: u32, scope: u32) -> u32 {
    // A device with no streams in this direction reports an error for the scope
    // instead of an empty list, which is a normal answer here.
    let Ok((_, storage)) = property(object, &main(STREAM_CONFIGURATION, scope)) else {
        return 0;
    };
    let list = storage.as_ptr().cast::<AudioBufferList>();
    let count = usize::try_from(unsafe { (*list).number_buffers }).unwrap_or(0);
    total_channels((0..count).map(|index| {
        let buffer = unsafe { addr_of!((*list).buffers).cast::<AudioBuffer>().add(index) };
        unsafe { (*buffer).number_channels }
    }))
}
fn objects() -> Result<Vec<u32>, Error> {
    let (size, storage) = property(SYSTEM_OBJECT, &main(DEVICE_LIST, SCOPE_GLOBAL))?;
    let count = size as usize / size_of::<u32>();
    let mut objects = Vec::with_capacity(count);
    for index in 0..count {
        objects.push(unsafe { storage.as_ptr().cast::<u32>().add(index).read() });
    }
    Ok(objects)
}
fn record(object: u32) -> DeviceRecord {
    let input_channels = channel_count(object, SCOPE_INPUT);
    let output_channels = channel_count(object, SCOPE_OUTPUT);
    DeviceRecord {
        object,
        uid: text_property(object, DEVICE_UID),
        name: text_property(object, OBJECT_NAME),
        input_channels,
        output_channels,
        sample_rate: value(object, &main(NOMINAL_SAMPLE_RATE, SCOPE_GLOBAL)).ok(),
    }
}
fn catalog() -> Result<Vec<DeviceRecord>, Error> {
    Ok(objects()?.into_iter().map(record).collect())
}

fn selector(name: &CStr) -> *mut c_void {
    unsafe { sel_registerName(name.as_ptr()) }
}
fn objc_class(name: &CStr) -> *mut c_void {
    unsafe { objc_getClass(name.as_ptr()) }
}
type MessageSend = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
/// Sends one message. `objc_msgSend` is variadic, so this is the only prototype
/// in use; every shape passes an argument slot, which the callee ignores when
/// the selector takes none.
fn send(receiver: *mut c_void, selector: *mut c_void, argument: *mut c_void) -> *mut c_void {
    if receiver.is_null() {
        return null_mut();
    }
    let send = unsafe { std::mem::transmute::<unsafe extern "C" fn(), MessageSend>(objc_msgSend) };
    unsafe { send(receiver, selector, argument) }
}

struct ProcessTap {
    tap: u32,
    destroy: DestroyTap,
}
impl Drop for ProcessTap {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.tap) };
    }
}
struct AggregateDevice(u32);
impl Drop for AggregateDevice {
    fn drop(&mut self) {
        unsafe { AudioHardwareDestroyAggregateDevice(self.0) };
    }
}
fn create_tap(name: &str, output_uid: &str) -> Result<(ProcessTap, String), Error> {
    let api = tap_api().ok_or(Error::Unavailable {
        feature: "system audio capture",
    })?;
    if objc_class(c"CATapDescription").is_null() {
        return Err(Error::Unavailable {
            feature: "system audio capture",
        });
    }
    let pool = unsafe { objc_autoreleasePoolPush() };
    let created = tap_object(name, output_uid, api);
    unsafe { objc_autoreleasePoolPop(pool) };
    created
}
fn tap_object(name: &str, output_uid: &str, api: TapApi) -> Result<(ProcessTap, String), Error> {
    let description = send(
        send(
            objc_class(c"CATapDescription"),
            selector(c"alloc"),
            null_mut(),
        ),
        selector(c"initStereoGlobalTapButExcludeProcesses:"),
        send(objc_class(c"NSArray"), selector(c"array"), null_mut()),
    );
    if description.is_null() {
        return Err(backend("CATapDescription", PARAMETER_ERROR));
    }
    send(description, selector(c"autorelease"), null_mut());
    // The initializer assigns the tap its UUID; reading it avoids needing the
    // read-write status of the property, which differs across SDK revisions.
    let uuid = send(description, selector(c"UUID"), null_mut());
    // `NSString` is toll-free bridged to `CFString`.
    let uuid_text = cf_read(send(uuid, selector(c"UUIDString"), null_mut()).cast_const())
        .ok_or_else(|| backend("NSUUID", PARAMETER_ERROR))?;
    if let Ok(text) = CString::new(name) {
        let make = selector(c"stringWithUTF8String:");
        let string = send(
            objc_class(c"NSString"),
            make,
            text.as_ptr().cast_mut().cast(),
        );
        send(description, selector(c"setName:"), string);
    }
    // The tap must follow the chosen output device and stay private to us.
    let device = cf_make(output_uid)?;
    send(
        description,
        selector(c"setDeviceUID:"),
        device.raw().cast_mut(),
    );
    send(description, selector(c"setMuteBehavior:"), null_mut());
    send(
        description,
        selector(c"setPrivate:"),
        std::ptr::without_provenance_mut(1),
    );
    let mut tap = 0;
    let status = unsafe { (api.create)(description, &raw mut tap) };
    if status == 0 {
        Ok((
            ProcessTap {
                tap,
                destroy: api.destroy,
            },
            uuid_text,
        ))
    } else {
        Err(backend("AudioHardwareCreateProcessTap", status))
    }
}

fn dictionary() -> Result<Cf, Error> {
    let keys = addr_of!(kCFTypeDictionaryKeyCallBacks);
    let values = addr_of!(kCFTypeDictionaryValueCallBacks);
    Cf::new(
        unsafe { CFDictionaryCreateMutable(null(), 0, keys, values) },
        "CFDictionaryCreateMutable",
    )
}
fn put(dictionary: &Cf, key: &str, value: *const c_void) -> Result<(), Error> {
    let key = cf_make(key)?;
    unsafe { CFDictionarySetValue(dictionary.raw(), key.raw(), value) };
    Ok(())
}
fn create_aggregate(tap_uid: &str, name: &str) -> Result<AggregateDevice, Error> {
    let entry = dictionary()?;
    let tap_name = cf_make(tap_uid)?;
    put(&entry, SUB_TAP_UID_KEY, tap_name.raw())?;
    put(&entry, SUB_TAP_DRIFT_KEY, unsafe { kCFBooleanTrue })?;
    let callbacks = addr_of!(kCFTypeArrayCallBacks);
    let taps = [entry.raw()];
    let list = unsafe { CFArrayCreate(null(), taps.as_ptr(), 1, callbacks) };
    let list = Cf::new(list, "CFArrayCreate")?;
    let device = dictionary()?;
    let uid = cf_make(&format!("koe-tap-{tap_uid}"))?;
    let label = cf_make(name)?;
    put(&device, AGGREGATE_UID_KEY, uid.raw())?;
    put(&device, AGGREGATE_NAME_KEY, label.raw())?;
    // `kAudioAggregateDeviceTapAutoStartKey` is deliberately not set: it makes
    // the aggregate wait for the first tapped audio, so an idle system tap
    // delivers no frames and recording loses system audio. Without it the
    // aggregate starts with the audio unit and the tap streams silence until a
    // process plays something.
    put(&device, AGGREGATE_TAP_LIST_KEY, list.raw())?;
    put(&device, AGGREGATE_PRIVATE_KEY, unsafe { kCFBooleanTrue })?;
    let mut aggregate = 0;
    let status = unsafe { AudioHardwareCreateAggregateDevice(device.raw(), &raw mut aggregate) };
    if status == 0 {
        Ok(AggregateDevice(aggregate))
    } else {
        Err(backend("AudioHardwareCreateAggregateDevice", status))
    }
}

struct Shared {
    sink: Mutex<Sink>,
    scratch: Mutex<Vec<f32>>,
    /// Written before the callback is installed and read on the audio thread.
    unit: AtomicPtr<c_void>,
    /// Last non-zero `AudioUnitRender` status, reported to the reader.
    last_error: AtomicI32,
    channels: usize,
    /// The `AudioObject` the unit renders.
    object: u32,
    /// The rate the unit's client format carries, as [`rate_bits`].
    rate: AtomicI32,
}
impl Shared {
    fn new(
        channels: usize,
        unit: *mut c_void,
        object: u32,
        device_rate: u32,
        spec_rate: u32,
    ) -> Self {
        Self {
            sink: Mutex::new(Sink::new(channels, QUEUE_FRAMES, device_rate, spec_rate)),
            scratch: Mutex::new(vec![0.0; channels * CALLBACK_FRAMES]),
            unit: AtomicPtr::new(unit),
            last_error: AtomicI32::new(0),
            channels,
            object,
            rate: AtomicI32::new(rate_bits(device_rate)),
        }
    }
}
impl Poll for Shared {
    fn take(&self, out: &mut [f32]) -> usize {
        lock(&self.sink).take(out)
    }

    fn last_error(&self) -> i32 {
        self.last_error.load(Ordering::Acquire)
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn rest(&self) {
        thread::sleep(RETRY);
    }

    /// Matches the unit to the rate its device reports now.
    ///
    /// The HAL renders only while the unit's client format carries the device's
    /// own rate. A device that changes rate under a running stream either keeps
    /// rendering at the new rate with no error, which skews the frame clock, or
    /// answers [`pipeline::UNIT_RECONFIGURING`] and delivers nothing. Both are
    /// fixed by rebuilding the unit at the rate the device reports, so an
    /// unchanged rate costs one property read.
    ///
    /// `AudioOutputUnitStop` waits for an in-flight render, so this must run
    /// without holding the sink or the scratch buffer; the callback takes both.
    fn reconcile(&self) -> bool {
        let unit = self.unit.load(Ordering::Acquire);
        if unit.is_null() {
            return false;
        }
        let Ok(rate) = device_rate(self.object) else {
            return false;
        };
        if rate == bits_rate(self.rate.load(Ordering::Acquire)) && self.last_error() == 0 {
            return true;
        }
        unsafe {
            AudioOutputUnitStop(unit);
            if AudioUnitUninitialize(unit) != 0 {
                return false;
            }
        }
        let format = format_at(self.channels, rate);
        if configure_input(unit, self.object, &format, from_ref(self).cast_mut().cast()).is_err() {
            return false;
        }
        // Convert the frames captured at the old rate and switch the sink
        // before the unit starts, so no frame at the new rate is ever measured
        // against the old one.
        lock(&self.sink).set_device_rate(rate);
        let started = unsafe { AudioUnitInitialize(unit) == 0 && AudioOutputUnitStart(unit) == 0 };
        if started {
            self.rate.store(rate_bits(rate), Ordering::Release);
            self.last_error.store(0, Ordering::Release);
        }
        started
    }
}

/// Rate stored in the shared word.
fn rate_bits(rate: u32) -> i32 {
    i32::try_from(rate).unwrap_or(i32::MAX)
}

/// Rate a shared word holds.
fn bits_rate(bits: i32) -> u32 {
    u32::try_from(bits).unwrap_or_default()
}

extern "C" fn render_callback(
    ctx: *mut c_void,
    flags: *mut u32,
    stamp: *const c_void,
    b: u32,
    n: u32,
    _data: *mut AudioBufferList,
) -> i32 {
    let Some(shared) = (unsafe { ctx.cast::<Shared>().as_ref() }) else {
        return PARAMETER_ERROR;
    };
    let unit = shared.unit.load(Ordering::Acquire);
    if unit.is_null() {
        return PARAMETER_ERROR;
    }
    let channels = shared.channels;
    let Ok(width) = u32::try_from(channels) else {
        return PARAMETER_ERROR;
    };
    let mut scratch = lock(&shared.scratch);
    let capacity = u32::try_from(scratch.len() / channels).unwrap_or(u32::MAX);
    let wanted = n.min(capacity);
    if wanted == 0 {
        return 0;
    }
    let mut list = AudioBufferList {
        number_buffers: 1,
        buffers: [AudioBuffer {
            number_channels: width,
            data_byte_size: wanted * width * SAMPLE_BYTES,
            data: scratch.as_mut_ptr().cast(),
        }],
    };
    let status = unsafe { AudioUnitRender(unit, flags, stamp, b, wanted, &raw mut list) };
    let reported = list.buffers[0].data_byte_size;
    let mut sink = lock(&shared.sink);
    let rendered = pipeline::render(
        &mut sink,
        scratch.as_mut_slice(),
        channels,
        status,
        reported,
    );
    match rendered {
        pipeline::Render::Failed(code) => {
            shared.last_error.store(code, Ordering::Release);
        }
        pipeline::Render::Ok => {
            shared.last_error.store(0, Ordering::Release);
        }
    }
    drop(scratch);
    drop(sink);
    0
}

struct Unit(*mut c_void);
impl Unit {
    fn new_hal() -> Result<Self, Error> {
        let description = AudioComponentDescription {
            kind: UNIT_TYPE_OUTPUT,
            sub_type: UNIT_SUBTYPE_HAL_OUTPUT,
            manufacturer: UNIT_MANUFACTURER_APPLE,
            flags: 0,
            flags_mask: 0,
        };
        let component = unsafe { AudioComponentFindNext(null_mut(), &raw const description) };
        if component.is_null() {
            return Err(backend("AudioComponentFindNext", PARAMETER_ERROR));
        }
        let mut unit = null_mut();
        let status = unsafe { AudioComponentInstanceNew(component, &raw mut unit) };
        if status == 0 && !unit.is_null() {
            Ok(Self(unit))
        } else {
            Err(backend("AudioComponentInstanceNew", status))
        }
    }
}
/// Sets one unit property. Free of the owning [`Unit`], so a reader thread can
/// rebuild a unit it does not own.
fn set_property<T>(
    unit: *mut c_void,
    property: u32,
    scope: u32,
    element: u32,
    data: &T,
) -> Result<(), Error> {
    let size = u32::try_from(size_of::<T>())
        .map_err(|_| backend("AudioUnitSetProperty", PARAMETER_ERROR))?;
    let data = from_ref(data).cast();
    let status = unsafe { AudioUnitSetProperty(unit, property, scope, element, data, size) };
    if status == 0 {
        Ok(())
    } else {
        Err(backend("AudioUnitSetProperty", status))
    }
}
impl Drop for Unit {
    fn drop(&mut self) {
        unsafe {
            AudioOutputUnitStop(self.0);
            AudioUnitUninitialize(self.0);
            AudioComponentInstanceDispose(self.0);
        }
    }
}

struct AudioStream {
    // Held for their `Drop`; the unit stops before the aggregate and the tap go.
    #[allow(dead_code)]
    unit: Unit,
    #[allow(dead_code)]
    resources: Option<(AggregateDevice, ProcessTap)>,
    shared: Arc<Shared>,
}
impl RawStream for AudioStream {
    fn read(&mut self, out: &mut [f32]) -> Result<usize, Error> {
        pipeline::read_until(self.shared.as_ref(), self.shared.channels, out)
    }
}

/// The stream format the HAL accepts for `channels` interleaved `f32` frames at
/// `rate`.
fn format_at(channels: usize, rate: u32) -> AudioStreamBasicDescription {
    let width = u32::try_from(channels).unwrap_or(1);
    AudioStreamBasicDescription {
        sample_rate: f64::from(rate),
        format_id: FORMAT_LINEAR_PCM,
        format_flags: FORMAT_FLAG_FLOAT | FORMAT_FLAG_PACKED | NATIVE_ENDIAN_FLAG,
        bytes_per_packet: SAMPLE_BYTES * width,
        frames_per_packet: 1,
        bytes_per_frame: SAMPLE_BYTES * width,
        channels_per_frame: width,
        bits_per_channel: 8 * SAMPLE_BYTES,
        reserved: 0,
    }
}

/// The rate an `AudioObject` reports now.
///
/// The HAL renders only while the unit's client format carries the device's own
/// rate, so callers ask the device instead of keeping the rate enumeration
/// reported earlier.
fn device_rate(object: u32) -> Result<u32, Error> {
    let rate: f64 = value(object, &main(NOMINAL_SAMPLE_RATE, SCOPE_GLOBAL))?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rate = if rate >= 1.0 && rate <= f64::from(u32::MAX) {
        rate as u32
    } else {
        return Err(backend("AudioObjectGetPropertyData", PARAMETER_ERROR));
    };
    Ok(rate)
}

/// Wires an uninitialized HAL unit to `object` in input mode and installs the
/// render callback whose context is `reference`. Runs both when a stream opens
/// and when the reader rebuilds the unit after its device changed format.
fn configure_input(
    unit: *mut c_void,
    object: u32,
    format: &AudioStreamBasicDescription,
    reference: *mut c_void,
) -> Result<(), Error> {
    set_property(unit, UNIT_ENABLE_IO, UNIT_SCOPE_INPUT, INPUT_ELEMENT, &1u32)?;
    set_property(
        unit,
        UNIT_ENABLE_IO,
        UNIT_SCOPE_OUTPUT,
        OUTPUT_ELEMENT,
        &0u32,
    )?;
    set_property(
        unit,
        UNIT_CURRENT_DEVICE,
        UNIT_SCOPE_GLOBAL,
        ELEMENT_MAIN,
        &object,
    )?;
    // The client side of the input element is its output scope: the device
    // feeds the unit, which hands converted samples to the callback.
    set_property(
        unit,
        UNIT_STREAM_FORMAT,
        UNIT_SCOPE_OUTPUT,
        INPUT_ELEMENT,
        format,
    )?;
    let callback = RenderCallback {
        input_proc: Some(render_callback),
        reference,
    };
    set_property(
        unit,
        UNIT_INPUT_CALLBACK,
        UNIT_SCOPE_GLOBAL,
        ELEMENT_MAIN,
        &callback,
    )?;
    set_property(
        unit,
        UNIT_MAX_FRAMES_PER_SLICE,
        UNIT_SCOPE_GLOBAL,
        ELEMENT_MAIN,
        &u32::try_from(CALLBACK_FRAMES).unwrap_or(u32::MAX),
    )
}

/// Lists the microphones and output devices `CoreAudio` reports.
///
/// # Errors
///
/// Returns the backend's error when a `CoreAudio` property cannot be read.
pub fn devices() -> Result<Vec<DeviceInfo>, Error> {
    let defaults = Defaults {
        input: value(SYSTEM_OBJECT, &main(DEFAULT_INPUT, SCOPE_GLOBAL)).unwrap_or(0),
        output: value(SYSTEM_OBJECT, &main(DEFAULT_OUTPUT, SCOPE_GLOBAL)).unwrap_or(0),
    };
    let mut devices = Vec::new();
    for object in objects()? {
        devices.extend(describe_device(record(object), &defaults));
    }
    Ok(devices)
}

/// Opens a microphone or a system-audio tap.
///
/// # Errors
///
/// Returns the backend's error when the audio unit or tap cannot start.
pub fn open(device: &DeviceInfo, spec: StreamSpec) -> Result<Box<dyn RawStream>, Error> {
    check_spec(device.source(), spec)?;
    let channels = usize::from(spec.channels());
    let (object, aggregate, tap) = match resolve(device, &catalog()?)? {
        Target::Input(object) => (object, None, None),
        Target::Output => {
            let uid = device
                .id()
                .as_str()
                .strip_prefix(SYSTEM_PREFIX)
                .unwrap_or_default();
            let (tap, tap_uid) = create_tap(device.name(), uid)?;
            let aggregate = create_aggregate(&tap_uid, device.name())?;
            let object = aggregate.0;
            (object, Some(aggregate), Some(tap))
        }
    };
    let unit = Unit::new_hal()?;
    // The HAL renders only at the device's own rate, so the unit asks for that
    // and the sink converts to the rate the caller requested.
    let rate = device_rate(object)?;
    let format = format_at(channels, rate);
    let shared = Arc::new(Shared::new(
        channels,
        unit.0,
        object,
        rate,
        spec.sample_rate(),
    ));
    configure_input(
        unit.0,
        object,
        &format,
        Arc::as_ptr(&shared).cast_mut().cast(),
    )?;
    let status = unsafe { AudioUnitInitialize(unit.0) };
    if status != 0 {
        return Err(backend("AudioUnitInitialize", status));
    }
    let status = unsafe { AudioOutputUnitStart(unit.0) };
    if status != 0 {
        return Err(backend("AudioOutputUnitStart", status));
    }
    Ok(Box::new(AudioStream {
        unit,
        resources: aggregate.zip(tap),
        shared,
    }))
}
