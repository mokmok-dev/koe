//! `CoreAudio` backend.
//!
//! Enumeration reads `AudioObject` properties off the system object. A
//! microphone is captured by a HAL output `AudioUnit` run in input mode; system
//! audio taps the output mix with a `CATapDescription` and runs the same unit
//! against a private aggregate device.

use crate::Source::{Microphone, SystemAudio};
use crate::config::StreamSpec;
use crate::device::{DeviceId, DeviceInfo};
use crate::error::{Error, Result};
use crate::queue::FrameQueue;
use crate::stream::RawStream;

use core::ffi::{CStr, c_char, c_void};
use core::mem::size_of;
use core::ptr::{addr_of, from_ref, null, null_mut};
use core::sync::atomic::{AtomicPtr, Ordering};
use std::ffi::CString;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

type Ptr = *const c_void;
type MutPtr = *mut c_void;
type Str = *const c_void;
type Data = *mut AudioBufferList;

const SYSTEM_OBJECT: u32 = 1;
const ELEMENT_MAIN: u32 = 0;
const PARAMETER_ERROR: i32 = -50;
const QUEUE_FRAMES: usize = 8_192;
const CALLBACK_FRAMES: usize = 4_096;
const SAMPLE_BYTES: u32 = 4;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY: Duration = Duration::from_millis(1);
const TEXT_ENCODING_UTF8: u32 = 0x0800_0100;
const SYSTEM_PREFIX: &str = "system:";
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
const UNIT_CURRENT_DEVICE: u32 = 2_000;
const UNIT_ENABLE_IO: u32 = 2_003;
const UNIT_INPUT_CALLBACK: u32 = 2_005;
const AGGREGATE_UID_KEY: &str = "uid";
const AGGREGATE_NAME_KEY: &str = "name";
const AGGREGATE_TAP_LIST_KEY: &str = "taps";
const AGGREGATE_TAP_START_KEY: &str = "tapautostart";
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
        qualifier: Ptr,
        size: *mut u32,
    ) -> i32;
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const Address,
        qualifier_size: u32,
        qualifier: Ptr,
        size: *mut u32,
        data: MutPtr,
    ) -> i32;
    fn AudioHardwareCreateProcessTap(description: MutPtr, tap: *mut u32) -> i32;
    fn AudioHardwareDestroyProcessTap(tap: u32) -> i32;
    fn AudioHardwareCreateAggregateDevice(description: Ptr, device: *mut u32) -> i32;
    fn AudioHardwareDestroyAggregateDevice(device: u32) -> i32;
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(component: MutPtr, d: *const AudioComponentDescription) -> MutPtr;
    fn AudioComponentInstanceNew(component: MutPtr, instance: *mut MutPtr) -> i32;
    fn AudioComponentInstanceDispose(instance: MutPtr) -> i32;
    fn AudioUnitSetProperty(unit: MutPtr, p: u32, s: u32, e: u32, d: Ptr, n: u32) -> i32;
    fn AudioUnitInitialize(unit: MutPtr) -> i32;
    fn AudioUnitUninitialize(unit: MutPtr) -> i32;
    fn AudioUnitRender(unit: MutPtr, f: *mut u32, t: Ptr, b: u32, n: u32, d: Data) -> i32;
    fn AudioOutputUnitStart(unit: MutPtr) -> i32;
    fn AudioOutputUnitStop(unit: MutPtr) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(a: Ptr, t: *const c_char, e: u32) -> Str;
    fn CFStringGetLength(text: Str) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(t: Str, b: *mut c_char, n: isize, e: u32) -> u8;
    fn CFDictionaryCreateMutable(a: Ptr, n: isize, k: *const Callbacks, v: *const Callbacks)
    -> Str;
    fn CFDictionarySetValue(dictionary: Ptr, key: Ptr, value: Ptr);
    fn CFArrayCreate(a: Ptr, v: *const Str, n: isize, c: *const Callbacks) -> Str;
    fn CFRelease(value: Ptr);
    static kCFBooleanTrue: Ptr;
    static kCFTypeDictionaryKeyCallBacks: Callbacks;
    static kCFTypeDictionaryValueCallBacks: Callbacks;
    static kCFTypeArrayCallBacks: Callbacks;
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> MutPtr;
    fn sel_registerName(name: *const c_char) -> MutPtr;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> MutPtr;
    fn objc_autoreleasePoolPop(pool: MutPtr);
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
fn property(object: u32, address: &Address) -> Result<(u32, Vec<u64>)> {
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
fn value<T: Copy>(object: u32, address: &Address) -> Result<T> {
    let (_, storage) = property(object, address)?;
    Ok(unsafe { storage.as_ptr().cast::<T>().read() })
}

struct Cf(*const c_void);
impl Cf {
    const fn new(value: *const c_void, operation: &'static str) -> Result<Self> {
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
fn cf_make(text: &str) -> Result<Cf> {
    let text =
        CString::new(text).map_err(|_| backend("CFStringCreateWithCString", PARAMETER_ERROR))?;
    Cf::new(
        unsafe { CFStringCreateWithCString(null(), text.as_ptr(), TEXT_ENCODING_UTF8) },
        "CFStringCreateWithCString",
    )
}
fn text_property(object: u32, selector: u32) -> Option<String> {
    let pointer: Ptr = value(object, &main(selector, SCOPE_GLOBAL)).ok()?;
    cf_read(Cf::new(pointer, "AudioObjectGetPropertyData").ok()?.raw())
}
fn channel_count(object: u32, scope: u32) -> u32 {
    // A device with no streams in this direction reports an error for the scope
    // instead of an empty list, which is a normal answer here.
    let Ok((_, storage)) = property(object, &main(STREAM_CONFIGURATION, scope)) else {
        return 0;
    };
    let list = storage.as_ptr().cast::<AudioBufferList>();
    let count = unsafe { (*list).number_buffers };
    let mut channels = 0u32;
    for index in 0..usize::try_from(count).unwrap_or(0) {
        let buffer = unsafe { addr_of!((*list).buffers).cast::<AudioBuffer>().add(index) };
        channels = channels.saturating_add(unsafe { (*buffer).number_channels });
    }
    channels
}
fn objects() -> Result<Vec<u32>> {
    let (size, storage) = property(SYSTEM_OBJECT, &main(DEVICE_LIST, SCOPE_GLOBAL))?;
    let count = size as usize / size_of::<u32>();
    let mut objects = Vec::with_capacity(count);
    for index in 0..count {
        objects.push(unsafe { storage.as_ptr().cast::<u32>().add(index).read() });
    }
    Ok(objects)
}
fn object_for(uid: &str, device: &DeviceInfo) -> Result<u32> {
    for object in objects()? {
        if text_property(object, DEVICE_UID).as_deref() == Some(uid) {
            return Ok(object);
        }
    }
    Err(Error::DeviceNotFound(device.id().clone()))
}
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
// A float-to-int cast saturates, so a broken rate lands on 0 or `u32::MAX`.
fn describe(object: u32, default_input: u32, default_output: u32) -> Vec<DeviceInfo> {
    let mut devices = Vec::new();
    let inputs = channel_count(object, SCOPE_INPUT);
    let outputs = channel_count(object, SCOPE_OUTPUT);
    let Some(uid) = text_property(object, DEVICE_UID) else {
        return devices;
    };
    let name = text_property(object, OBJECT_NAME)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| uid.clone());
    let rate = value(object, &main(NOMINAL_SAMPLE_RATE, SCOPE_GLOBAL)).unwrap_or(0.0) as u32;
    if inputs > 0
        && let Ok(id) = DeviceId::new(uid.as_str())
    {
        let default = object == default_input;
        let count = u16::try_from(inputs).unwrap_or(u16::MAX);
        let entry = DeviceInfo::new(id, name.clone(), Microphone, default, rate, count);
        devices.push(entry);
    }
    if outputs > 0
        && let Ok(id) = DeviceId::new(format!("{SYSTEM_PREFIX}{uid}"))
    {
        let default = object == default_output;
        let count = u16::try_from(outputs).unwrap_or(u16::MAX);
        let entry = DeviceInfo::new(id, name, SystemAudio, default, rate, count);
        devices.push(entry);
    }
    devices
}

/// Lists the capture devices `CoreAudio` reports.
pub fn devices() -> Result<Vec<DeviceInfo>> {
    let default_input = value(SYSTEM_OBJECT, &main(DEFAULT_INPUT, SCOPE_GLOBAL)).unwrap_or(0);
    let default_output = value(SYSTEM_OBJECT, &main(DEFAULT_OUTPUT, SCOPE_GLOBAL)).unwrap_or(0);
    let mut devices = Vec::new();
    for object in objects()? {
        devices.extend(describe(object, default_input, default_output));
    }
    Ok(devices)
}
fn selector(name: &CStr) -> MutPtr {
    unsafe { sel_registerName(name.as_ptr()) }
}
fn objc_class(name: &CStr) -> MutPtr {
    unsafe { objc_getClass(name.as_ptr()) }
}
type MessageSend = unsafe extern "C" fn(MutPtr, MutPtr, MutPtr) -> MutPtr;
/// Sends one message. `objc_msgSend` is variadic, so this is the only prototype
/// in use; every shape passes an argument slot, which the callee ignores when
/// the selector takes none.
fn send(receiver: MutPtr, selector: MutPtr, argument: MutPtr) -> MutPtr {
    if receiver.is_null() {
        return null_mut();
    }
    let send = unsafe { core::mem::transmute::<unsafe extern "C" fn(), MessageSend>(objc_msgSend) };
    unsafe { send(receiver, selector, argument) }
}

struct ProcessTap(u32);
impl Drop for ProcessTap {
    fn drop(&mut self) {
        unsafe { AudioHardwareDestroyProcessTap(self.0) };
    }
}
struct AggregateDevice(u32);
impl Drop for AggregateDevice {
    fn drop(&mut self) {
        unsafe { AudioHardwareDestroyAggregateDevice(self.0) };
    }
}
fn create_tap(name: &str) -> Result<(ProcessTap, String)> {
    let pool = unsafe { objc_autoreleasePoolPush() };
    let created = tap_object(name);
    unsafe { objc_autoreleasePoolPop(pool) };
    created
}
fn tap_object(name: &str) -> Result<(ProcessTap, String)> {
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
    let mut tap = 0;
    let status = unsafe { AudioHardwareCreateProcessTap(description, &raw mut tap) };
    if status == 0 {
        Ok((ProcessTap(tap), uuid_text))
    } else {
        Err(backend("AudioHardwareCreateProcessTap", status))
    }
}

fn dictionary() -> Result<Cf> {
    let keys = addr_of!(kCFTypeDictionaryKeyCallBacks);
    let values = addr_of!(kCFTypeDictionaryValueCallBacks);
    Cf::new(
        unsafe { CFDictionaryCreateMutable(null(), 0, keys, values) },
        "CFDictionaryCreateMutable",
    )
}
fn put(dictionary: &Cf, key: &str, value: Ptr) -> Result<()> {
    let key = cf_make(key)?;
    unsafe { CFDictionarySetValue(dictionary.raw(), key.raw(), value) };
    Ok(())
}
fn create_aggregate(tap_uid: &str, name: &str) -> Result<AggregateDevice> {
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
    put(&device, AGGREGATE_TAP_LIST_KEY, list.raw())?;
    put(&device, AGGREGATE_TAP_START_KEY, unsafe { kCFBooleanTrue })?;
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
    queue: Mutex<FrameQueue>,
    pending: Mutex<Vec<f32>>,
    scratch: Mutex<Vec<f32>>,
    /// Written before the callback is installed and read on the audio thread.
    unit: AtomicPtr<c_void>,
    channels: usize,
    capacity: usize,
}
impl Shared {
    fn new(channels: usize, unit: *mut c_void) -> Self {
        Self {
            queue: Mutex::new(FrameQueue::new(channels, QUEUE_FRAMES)),
            pending: Mutex::new(Vec::new()),
            scratch: Mutex::new(vec![0.0; channels * CALLBACK_FRAMES]),
            unit: AtomicPtr::new(unit),
            channels,
            capacity: channels * QUEUE_FRAMES,
        }
    }
    fn take(&self, out: &mut [f32]) -> usize {
        let mut queue = lock(&self.queue);
        {
            let mut pending = lock(&self.pending);
            if !pending.is_empty() {
                let accepted = queue.push(pending.as_slice()) * self.channels;
                pending.drain(..accepted);
            }
        }
        queue.pop_into(out)
    }
}

extern "C" fn render(ctx: MutPtr, flags: *mut u32, stamp: Ptr, b: u32, n: u32, _data: Data) -> i32 {
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
    if status != 0 {
        return status;
    }
    let delivered = list.buffers[0].data_byte_size as usize / size_of::<f32>();
    let delivered = delivered - delivered % channels;
    if delivered == 0 {
        return 0;
    }
    let received = &scratch[..delivered];
    let mut queue = lock(&shared.queue);
    let mut pending = lock(&shared.pending);
    let mut carry = core::mem::take(&mut *pending);
    if !carry.is_empty() {
        let accepted = queue.push(&carry) * channels;
        carry.drain(..accepted);
    }
    let accepted = queue.push(received) * channels;
    carry.extend_from_slice(&received[accepted..]);
    carry.truncate(shared.capacity);
    drop(scratch);
    *pending = carry;
    drop(queue);
    drop(pending);
    0
}

struct Unit(*mut c_void);
impl Unit {
    fn new_hal() -> Result<Self> {
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
    fn set<T>(&self, property: u32, scope: u32, element: u32, data: &T) -> Result<()> {
        let size = u32::try_from(size_of::<T>())
            .map_err(|_| backend("AudioUnitSetProperty", PARAMETER_ERROR))?;
        let data = from_ref(data).cast();
        let status = unsafe { AudioUnitSetProperty(self.0, property, scope, element, data, size) };
        if status == 0 {
            Ok(())
        } else {
            Err(backend("AudioUnitSetProperty", status))
        }
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
    fn read(&mut self, out: &mut [f32]) -> Result<usize> {
        let channels = self.shared.channels;
        let wanted = out.len() / channels;
        let deadline = Instant::now() + READ_TIMEOUT;
        let mut produced = 0;
        while produced < wanted {
            produced += self.shared.take(&mut out[produced * channels..]);
            if produced == wanted {
                break;
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout {
                    operation: "AudioUnitRender",
                });
            }
            thread::sleep(RETRY);
        }
        Ok(produced)
    }
}

/// Opens a capture stream on `device`.
pub fn open(device: &DeviceInfo, spec: StreamSpec) -> Result<Box<dyn RawStream>> {
    let channels = usize::from(spec.channels());
    let width = u32::try_from(channels).unwrap_or(1);
    let (object, aggregate, tap) = match device.source() {
        Microphone => (object_for(device.id().as_str(), device)?, None, None),
        SystemAudio => {
            let uid = device
                .id()
                .as_str()
                .strip_prefix(SYSTEM_PREFIX)
                .unwrap_or_default();
            let output = object_for(uid, device)?;
            if channel_count(output, SCOPE_OUTPUT) == 0 {
                return Err(Error::DeviceNotFound(device.id().clone()));
            }
            let (tap, tap_uid) = create_tap(device.name())?;
            let aggregate = create_aggregate(&tap_uid, device.name())?;
            let object = aggregate.0;
            (object, Some(aggregate), Some(tap))
        }
    };
    let unit = Unit::new_hal()?;
    let shared = Arc::new(Shared::new(channels, unit.0));
    unit.set(UNIT_ENABLE_IO, UNIT_SCOPE_INPUT, INPUT_ELEMENT, &1u32)?;
    unit.set(UNIT_ENABLE_IO, UNIT_SCOPE_OUTPUT, OUTPUT_ELEMENT, &0u32)?;
    unit.set(
        UNIT_CURRENT_DEVICE,
        UNIT_SCOPE_GLOBAL,
        ELEMENT_MAIN,
        &object,
    )?;
    let format = AudioStreamBasicDescription {
        sample_rate: f64::from(spec.sample_rate()),
        format_id: FORMAT_LINEAR_PCM,
        format_flags: FORMAT_FLAG_FLOAT | FORMAT_FLAG_PACKED | NATIVE_ENDIAN_FLAG,
        bytes_per_packet: SAMPLE_BYTES * width,
        frames_per_packet: 1,
        bytes_per_frame: SAMPLE_BYTES * width,
        channels_per_frame: width,
        bits_per_channel: 8 * SAMPLE_BYTES,
        reserved: 0,
    };
    // The client side of the input element is its output scope: the device
    // feeds the unit, which hands converted samples to the callback.
    unit.set(
        UNIT_STREAM_FORMAT,
        UNIT_SCOPE_OUTPUT,
        INPUT_ELEMENT,
        &format,
    )?;
    let callback = RenderCallback {
        input_proc: Some(render),
        reference: Arc::as_ptr(&shared).cast_mut().cast(),
    };
    unit.set(
        UNIT_INPUT_CALLBACK,
        UNIT_SCOPE_GLOBAL,
        ELEMENT_MAIN,
        &callback,
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
