//! `PulseAudio` backend.
//!
//! Enumeration uses `libpulse`'s asynchronous context API over a private
//! main loop. Capture uses `libpulse-simple`, which blocks in `read` and keeps
//! the rest of the crate free of callback plumbing.

use std::ffi::CString;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::thread;
use std::time::{Duration, Instant};

use crate::Source;
use crate::config::StreamSpec;
use crate::device::{DeviceId, DeviceInfo};
use crate::error::Error;
use crate::stream::RawStream;

#[cfg(target_endian = "little")]
const PA_SAMPLE_FLOAT32_NATIVE: c_int = 5;
#[cfg(target_endian = "big")]
const PA_SAMPLE_FLOAT32_NATIVE: c_int = 6;

const PA_STREAM_RECORD: c_int = 2;
const PA_CONTEXT_READY: c_int = 4;
const PA_CONTEXT_FAILED: c_int = 5;
const PA_CONTEXT_TERMINATED: c_int = 6;
const PA_OPERATION_DONE: c_int = 1;
const PA_OPERATION_CANCELLED: c_int = 2;
const PA_INVALID_INDEX: u32 = u32::MAX;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

#[repr(C)]
struct PaSampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct PaChannelMap {
    channels: u8,
    map: [c_int; 32],
}

#[repr(C)]
struct PaCVolume {
    channels: u8,
    values: [u32; 32],
}

/// Prefix of the C `pa_source_info` layout. Only these fields are read.
#[repr(C)]
struct PaSourceInfo {
    name: *const c_char,
    index: u32,
    description: *const c_char,
    sample_spec: PaSampleSpec,
    channel_map: PaChannelMap,
    owner_module: u32,
    volume: PaCVolume,
    mute: c_int,
    monitor_of_sink: u32,
    monitor_of_sink_name: *const c_char,
}

/// Prefix of the C `pa_server_info` layout. Only these fields are read.
#[repr(C)]
struct PaServerInfo {
    user_name: *const c_char,
    host_name: *const c_char,
    server_version: *const c_char,
    server_name: *const c_char,
    sample_spec: PaSampleSpec,
    default_sink_name: *const c_char,
    default_source_name: *const c_char,
}

type SourceInfoCallback = extern "C" fn(*mut c_void, *const PaSourceInfo, c_int, *mut c_void);
type ServerInfoCallback = extern "C" fn(*mut c_void, *const PaServerInfo, *mut c_void);

#[link(name = "pulse")]
unsafe extern "C" {
    fn pa_mainloop_new() -> *mut c_void;
    fn pa_mainloop_free(mainloop: *mut c_void);
    fn pa_mainloop_get_api(mainloop: *mut c_void) -> *mut c_void;
    fn pa_mainloop_iterate(mainloop: *mut c_void, block: c_int, retval: *mut c_int) -> c_int;

    fn pa_context_new(api: *mut c_void, name: *const c_char) -> *mut c_void;
    fn pa_context_connect(
        context: *mut c_void,
        server: *const c_char,
        flags: c_int,
        spawn_api: *const c_void,
    ) -> c_int;
    fn pa_context_disconnect(context: *mut c_void);
    fn pa_context_unref(context: *mut c_void);
    fn pa_context_get_state(context: *mut c_void) -> c_int;
    fn pa_context_errno(context: *mut c_void) -> c_int;
    fn pa_context_get_source_info_list(
        context: *mut c_void,
        callback: Option<SourceInfoCallback>,
        userdata: *mut c_void,
    ) -> *mut c_void;
    fn pa_context_get_server_info(
        context: *mut c_void,
        callback: Option<ServerInfoCallback>,
        userdata: *mut c_void,
    ) -> *mut c_void;

    fn pa_operation_get_state(operation: *mut c_void) -> c_int;
    fn pa_operation_cancel(operation: *mut c_void);
    fn pa_operation_unref(operation: *mut c_void);
}

#[link(name = "pulse-simple")]
unsafe extern "C" {
    fn pa_simple_new(
        server: *const c_char,
        name: *const c_char,
        direction: c_int,
        device: *const c_char,
        stream_name: *const c_char,
        sample_spec: *const PaSampleSpec,
        channel_map: *const c_void,
        buffer_attr: *const c_void,
        error: *mut c_int,
    ) -> *mut c_void;
    fn pa_simple_read(
        simple: *mut c_void,
        data: *mut c_void,
        bytes: usize,
        error: *mut c_int,
    ) -> c_int;
    fn pa_simple_free(simple: *mut c_void);
}

const fn backend(operation: &'static str, code: c_int) -> Error {
    Error::Backend { operation, code }
}

fn text(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned(),
    )
}

struct Mainloop(*mut c_void);

impl Mainloop {
    fn new() -> Result<Self, Error> {
        let mainloop = unsafe { pa_mainloop_new() };
        if mainloop.is_null() {
            Err(Error::Backend {
                operation: "pa_mainloop_new",
                code: 0,
            })
        } else {
            Ok(Self(mainloop))
        }
    }

    fn api(&self) -> Result<*mut c_void, Error> {
        let api = unsafe { pa_mainloop_get_api(self.0) };
        if api.is_null() {
            Err(Error::Backend {
                operation: "pa_mainloop_get_api",
                code: 0,
            })
        } else {
            Ok(api)
        }
    }

    fn iterate(&self) -> Result<(), Error> {
        let status = unsafe { pa_mainloop_iterate(self.0, 0, null_mut()) };
        if status < 0 {
            Err(backend("pa_mainloop_iterate", status))
        } else {
            Ok(())
        }
    }
}

impl Drop for Mainloop {
    fn drop(&mut self) {
        unsafe { pa_mainloop_free(self.0) };
    }
}

struct Context(*mut c_void);

impl Context {
    fn new(api: *mut c_void) -> Result<Self, Error> {
        let name = c"koe-capture";
        let context = unsafe { pa_context_new(api, name.as_ptr()) };
        if context.is_null() {
            Err(Error::Backend {
                operation: "pa_context_new",
                code: 0,
            })
        } else {
            Ok(Self(context))
        }
    }

    fn connect(&self) -> Result<(), Error> {
        let status = unsafe { pa_context_connect(self.0, null(), 0, null()) };
        if status < 0 {
            Err(backend("pa_context_connect", self.errno()))
        } else {
            Ok(())
        }
    }

    fn errno(&self) -> c_int {
        unsafe { pa_context_errno(self.0) }
    }

    fn state(&self) -> c_int {
        unsafe { pa_context_get_state(self.0) }
    }

    fn wait_ready(&self, mainloop: &Mainloop) -> Result<(), Error> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            match self.state() {
                PA_CONTEXT_READY => return Ok(()),
                PA_CONTEXT_FAILED | PA_CONTEXT_TERMINATED => {
                    return Err(backend("pa_context_connect", self.errno()));
                }
                _ => {}
            }
            mainloop.iterate()?;
            if Instant::now() >= deadline {
                return Err(Error::Timeout {
                    operation: "pa_context_connect",
                });
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn query_server(&self, mainloop: &Mainloop, collector: &mut Collector) -> Result<(), Error> {
        let operation = unsafe {
            pa_context_get_server_info(
                self.0,
                Some(collect_server),
                std::ptr::from_mut(collector).cast(),
            )
        };
        self.wait_operation(mainloop, operation, "pa_context_get_server_info")
    }

    fn query_sources(&self, mainloop: &Mainloop, collector: &mut Collector) -> Result<(), Error> {
        let operation = unsafe {
            pa_context_get_source_info_list(
                self.0,
                Some(collect_source),
                std::ptr::from_mut(collector).cast(),
            )
        };
        self.wait_operation(mainloop, operation, "pa_context_get_source_info_list")
    }

    fn wait_operation(
        &self,
        mainloop: &Mainloop,
        operation: *mut c_void,
        name: &'static str,
    ) -> Result<(), Error> {
        if operation.is_null() {
            return Err(backend(name, self.errno()));
        }
        let deadline = Instant::now() + QUERY_TIMEOUT;
        loop {
            match unsafe { pa_operation_get_state(operation) } {
                PA_OPERATION_DONE => {
                    unsafe { pa_operation_unref(operation) };
                    return Ok(());
                }
                PA_OPERATION_CANCELLED => {
                    unsafe { pa_operation_unref(operation) };
                    return Err(backend(name, self.errno()));
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                unsafe {
                    pa_operation_cancel(operation);
                    pa_operation_unref(operation);
                }
                return Err(Error::Timeout { operation: name });
            }
            mainloop.iterate()?;
            thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            pa_context_disconnect(self.0);
            pa_context_unref(self.0);
        }
    }
}

#[derive(Default)]
struct Collector {
    devices: Vec<DeviceInfo>,
    default_sink: Option<String>,
    default_source: Option<String>,
}

extern "C" fn collect_server(
    _context: *mut c_void,
    info: *const PaServerInfo,
    userdata: *mut c_void,
) {
    if info.is_null() || userdata.is_null() {
        return;
    }
    let info = unsafe { &*info };
    let collector = unsafe { &mut *userdata.cast::<Collector>() };
    collector.default_sink = text(info.default_sink_name);
    collector.default_source = text(info.default_source_name);
}

extern "C" fn collect_source(
    _context: *mut c_void,
    info: *const PaSourceInfo,
    end_of_list: c_int,
    userdata: *mut c_void,
) {
    if end_of_list != 0 || info.is_null() || userdata.is_null() {
        return;
    }
    let info = unsafe { &*info };
    let collector = unsafe { &mut *userdata.cast::<Collector>() };
    let Some(name) = text(info.name) else {
        return;
    };
    let description = text(info.description)
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| name.clone());
    let is_monitor = info.monitor_of_sink != PA_INVALID_INDEX;
    let source = if is_monitor {
        Source::SystemAudio
    } else {
        Source::Microphone
    };
    let is_default = if is_monitor {
        text(info.monitor_of_sink_name)
            .is_some_and(|sink| collector.default_sink.as_deref() == Some(sink.as_str()))
    } else {
        collector.default_source.as_deref() == Some(name.as_str())
    };
    let Ok(id) = DeviceId::new(name) else {
        return;
    };
    collector.devices.push(DeviceInfo::new(
        id,
        description,
        source,
        is_default,
        info.sample_spec.rate,
        u16::from(info.sample_spec.channels),
    ));
}

pub fn devices() -> Result<Vec<DeviceInfo>, Error> {
    let mainloop = Mainloop::new()?;
    let api = mainloop.api()?;
    let context = Context::new(api)?;
    context.connect()?;
    context.wait_ready(&mainloop)?;

    let mut collector = Collector::default();
    context.query_server(&mainloop, &mut collector)?;
    context.query_sources(&mainloop, &mut collector)?;
    Ok(collector.devices)
}

struct PulseStream {
    simple: *mut c_void,
    channels: usize,
}

impl RawStream for PulseStream {
    fn read(&mut self, out: &mut [f32]) -> Result<usize, Error> {
        let Some(bytes) = out.len().checked_mul(size_of::<f32>()) else {
            return Err(Error::BackendInvariant { operation: "read" });
        };
        let mut error = 0;
        let status = unsafe {
            pa_simple_read(
                self.simple,
                out.as_mut_ptr().cast(),
                bytes,
                std::ptr::from_mut(&mut error),
            )
        };
        if status < 0 {
            return Err(backend("pa_simple_read", error));
        }
        Ok(out.len() / self.channels)
    }
}

impl Drop for PulseStream {
    fn drop(&mut self) {
        unsafe { pa_simple_free(self.simple) };
    }
}

pub fn open(device: &DeviceInfo, spec: StreamSpec) -> Result<Box<dyn RawStream>, Error> {
    let device_name = CString::new(device.id().as_str()).map_err(|_| Error::InvalidId)?;
    let app = c"koe-capture";
    let stream_name = c"capture";
    let channels = u8::try_from(spec.channels()).map_err(|_| Error::InvalidSpec {
        sample_rate: spec.sample_rate(),
        channels: spec.channels(),
    })?;
    let sample_spec = PaSampleSpec {
        format: PA_SAMPLE_FLOAT32_NATIVE,
        rate: spec.sample_rate(),
        channels,
    };
    let mut error = 0;
    let simple = unsafe {
        pa_simple_new(
            null(),
            app.as_ptr(),
            PA_STREAM_RECORD,
            device_name.as_ptr(),
            stream_name.as_ptr(),
            std::ptr::from_ref(&sample_spec),
            null(),
            null(),
            std::ptr::from_mut(&mut error),
        )
    };
    if simple.is_null() {
        return Err(backend("pa_simple_new", error));
    }
    Ok(Box::new(PulseStream {
        simple,
        channels: usize::from(spec.channels()),
    }))
}
