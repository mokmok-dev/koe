//! `libpulse` and `libpulse-simple` bindings.

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::mem::take;
use std::ptr::{null, null_mut, with_exposed_provenance_mut};
use std::rc::Rc;
use std::thread;
use std::time::Instant;

use koe_capture_core::{DeviceInfo, Error, RawStream, StreamSpec};

use crate::enumerate::{ServerDefaults, SourceRecord};
use crate::pipeline::{
    Operation, POLL, Session, backend, bytes_for, device_cstring, enumerate, read_frames,
    wait_operation,
};

#[cfg(target_endian = "little")]
const PA_SAMPLE_FLOAT32_NATIVE: c_int = 5;
#[cfg(target_endian = "big")]
const PA_SAMPLE_FLOAT32_NATIVE: c_int = 6;

const PA_STREAM_RECORD: c_int = 2;

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

const fn pointer(operation: Operation) -> *mut c_void {
    with_exposed_provenance_mut(operation.0)
}

struct MainloopHandle(*mut c_void);

impl Drop for MainloopHandle {
    fn drop(&mut self) {
        unsafe { pa_mainloop_free(self.0) };
    }
}

/// Shared handle to the poll loop.
///
/// A `pa_context` points back into the mainloop it was created from, so every
/// context holds a reference of its own instead of relying on the owner's
/// field order to free it second.
#[derive(Clone)]
struct Mainloop(Rc<MainloopHandle>);

impl Mainloop {
    fn new() -> Result<Self, Error> {
        let mainloop = unsafe { pa_mainloop_new() };
        if mainloop.is_null() {
            Err(Error::Backend {
                operation: "pa_mainloop_new",
                code: 0,
            })
        } else {
            Ok(Self(Rc::new(MainloopHandle(mainloop))))
        }
    }

    fn api(&self) -> Result<*mut c_void, Error> {
        let api = unsafe { pa_mainloop_get_api(self.0.0) };
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
        let status = unsafe { pa_mainloop_iterate(self.0.0, 0, null_mut()) };
        if status < 0 {
            Err(backend("pa_mainloop_iterate", status))
        } else {
            Ok(())
        }
    }
}

struct Context {
    handle: *mut c_void,
    /// Holds the loop open for as long as the context exists; `handle` is only
    /// valid while that loop is alive.
    _mainloop: Mainloop,
}

impl Context {
    fn new(mainloop: &Mainloop) -> Result<Self, Error> {
        let api = mainloop.api()?;
        let name = c"koe-capture";
        let context = unsafe { pa_context_new(api, name.as_ptr()) };
        if context.is_null() {
            Err(Error::Backend {
                operation: "pa_context_new",
                code: 0,
            })
        } else {
            Ok(Self {
                handle: context,
                _mainloop: mainloop.clone(),
            })
        }
    }

    fn connect(&self) -> Result<(), Error> {
        let status = unsafe { pa_context_connect(self.handle, null(), 0, null()) };
        if status < 0 {
            Err(backend("pa_context_connect", self.errno()))
        } else {
            Ok(())
        }
    }

    fn errno(&self) -> c_int {
        unsafe { pa_context_errno(self.handle) }
    }

    fn state(&self) -> c_int {
        unsafe { pa_context_get_state(self.handle) }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            pa_context_disconnect(self.handle);
            pa_context_unref(self.handle);
        }
    }
}

#[derive(Default)]
struct Collector {
    records: Vec<SourceRecord>,
    defaults: ServerDefaults,
    failure: Option<c_int>,
}

extern "C" fn collect_server(
    context: *mut c_void,
    info: *const PaServerInfo,
    userdata: *mut c_void,
) {
    if userdata.is_null() {
        return;
    }
    let collector = unsafe { &mut *userdata.cast::<Collector>() };
    if info.is_null() {
        collector.failure = Some(unsafe { pa_context_errno(context) });
        return;
    }
    let info = unsafe { &*info };
    // PipeWire reports `@DEFAULT_SINK@`/`@DEFAULT_SOURCE@` instead of a name.
    collector.defaults.default_sink =
        text(info.default_sink_name).filter(|name| !name.starts_with('@'));
    collector.defaults.default_source =
        text(info.default_source_name).filter(|name| !name.starts_with('@'));
}

extern "C" fn collect_source(
    context: *mut c_void,
    info: *const PaSourceInfo,
    end_of_list: c_int,
    userdata: *mut c_void,
) {
    if userdata.is_null() {
        return;
    }
    let collector = unsafe { &mut *userdata.cast::<Collector>() };
    if end_of_list < 0 {
        collector.failure = Some(unsafe { pa_context_errno(context) });
        return;
    }
    if end_of_list != 0 || info.is_null() {
        return;
    }
    let info = unsafe { &*info };
    collector.records.push(SourceRecord {
        name: text(info.name),
        description: text(info.description),
        monitor_of_sink: info.monitor_of_sink,
        monitor_of_sink_name: text(info.monitor_of_sink_name),
        sample_rate: info.sample_spec.rate,
        channels: info.sample_spec.channels,
    });
}

/// Owns the loop and the context built on it. The context holds a mainloop
/// reference of its own, so the loop outlives it whatever order these fields
/// drop in.
struct PulseHal {
    mainloop: Mainloop,
    context: Context,
    collector: Collector,
}

impl Session for PulseHal {
    fn iterate(&mut self) -> Result<(), Error> {
        self.mainloop.iterate()
    }

    fn errno(&mut self) -> c_int {
        self.context.errno()
    }

    fn context_state(&mut self) -> c_int {
        self.context.state()
    }

    fn operation_state(&mut self, operation: Operation) -> c_int {
        unsafe { pa_operation_get_state(pointer(operation)) }
    }

    fn begin_server_query(&mut self) -> Result<Operation, Error> {
        let operation = unsafe {
            pa_context_get_server_info(
                self.context.handle,
                Some(collect_server),
                std::ptr::from_mut(&mut self.collector).cast(),
            )
        };
        if operation.is_null() {
            return Err(backend("pa_context_get_server_info", self.context.errno()));
        }
        Ok(Operation(operation.expose_provenance()))
    }

    fn wait_server(&mut self, operation: Operation) -> Result<ServerDefaults, Error> {
        wait_operation(self, Some(operation), "pa_context_get_server_info")?;
        if let Some(code) = self.collector.failure.take() {
            return Err(backend("pa_context_get_server_info", code));
        }
        Ok(take(&mut self.collector.defaults))
    }

    fn begin_source_query(&mut self) -> Result<Operation, Error> {
        let operation = unsafe {
            pa_context_get_source_info_list(
                self.context.handle,
                Some(collect_source),
                std::ptr::from_mut(&mut self.collector).cast(),
            )
        };
        if operation.is_null() {
            return Err(backend(
                "pa_context_get_source_info_list",
                self.context.errno(),
            ));
        }
        Ok(Operation(operation.expose_provenance()))
    }

    fn wait_sources(&mut self, operation: Operation) -> Result<Vec<SourceRecord>, Error> {
        wait_operation(self, Some(operation), "pa_context_get_source_info_list")?;
        if let Some(code) = self.collector.failure.take() {
            return Err(backend("pa_context_get_source_info_list", code));
        }
        Ok(take(&mut self.collector.records))
    }

    fn end_operation(&mut self, operation: Operation, cancelled: bool) {
        let operation = pointer(operation);
        unsafe {
            if cancelled {
                pa_operation_cancel(operation);
            }
            pa_operation_unref(operation);
        }
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn rest(&mut self) {
        thread::sleep(POLL);
    }
}

/// Lists the capture sources the `PulseAudio` server reports.
///
/// # Errors
///
/// Returns [`Error::Backend`] or [`Error::Timeout`] when the server cannot be
/// reached.
pub fn devices() -> Result<Vec<DeviceInfo>, Error> {
    let mainloop = Mainloop::new()?;
    let context = Context::new(&mainloop)?;
    context.connect()?;
    let mut hal = PulseHal {
        mainloop,
        context,
        collector: Collector::default(),
    };
    enumerate(&mut hal)
}

struct PulseStream {
    simple: *mut c_void,
    channels: usize,
}

impl RawStream for PulseStream {
    fn read(&mut self, out: &mut [f32]) -> Result<usize, Error> {
        let Some(bytes) = bytes_for(out.len()) else {
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
        read_frames(status, error, out.len(), self.channels)
    }
}

impl Drop for PulseStream {
    fn drop(&mut self) {
        unsafe { pa_simple_free(self.simple) };
    }
}

/// Opens a source for blocking capture.
///
/// # Errors
///
/// Returns [`Error::Backend`] when the stream cannot start.
pub fn open(device: &DeviceInfo, spec: StreamSpec) -> Result<Box<dyn RawStream>, Error> {
    let device_name = device_cstring(device.id())?;
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn context_holds_a_mainloop_reference_of_its_own() {
        let mainloop = Mainloop::new().unwrap();
        let before = Rc::strong_count(&mainloop.0);
        let context = Context::new(&mainloop).unwrap();
        assert_eq!(Rc::strong_count(&mainloop.0), before + 1);
        drop(context);
        assert_eq!(Rc::strong_count(&mainloop.0), before);
    }

    #[test]
    fn dropping_the_owners_mainloop_before_the_context_keeps_the_loop_alive() {
        let mainloop = Mainloop::new().unwrap();
        let observed = Rc::downgrade(&mainloop.0);
        let context = Context::new(&mainloop).unwrap();
        let hal = PulseHal {
            mainloop,
            context,
            collector: Collector::default(),
        };
        let PulseHal {
            mainloop, context, ..
        } = hal;
        drop(mainloop);
        assert_eq!(observed.strong_count(), 1);
        drop(context);
        assert_eq!(observed.strong_count(), 0);
    }
}
