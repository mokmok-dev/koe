//! `PulseAudio` wait loops, stream sizing, and the server access they need.

use std::ffi::CString;
use std::mem::size_of;
use std::time::{Duration, Instant};

use koe_capture_core::{DeviceId, DeviceInfo, Error};

use crate::enumerate::{ServerDefaults, SourceRecord, describe_source};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
pub const POLL: Duration = Duration::from_millis(1);

pub const PA_CONTEXT_READY: i32 = 4;
pub const PA_CONTEXT_FAILED: i32 = 5;
pub const PA_CONTEXT_TERMINATED: i32 = 6;
pub const PA_OPERATION_DONE: i32 = 1;
pub const PA_OPERATION_CANCELLED: i32 = 2;

pub const fn backend(operation: &'static str, code: i32) -> Error {
    Error::Backend { operation, code }
}

/// One in-flight server request. The real session packs a `pa_operation`
/// pointer; a test hands out any number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Operation(pub usize);

pub trait Session {
    fn iterate(&mut self) -> Result<(), Error>;
    fn errno(&mut self) -> i32;
    fn context_state(&mut self) -> i32;
    fn operation_state(&mut self, operation: Operation) -> i32;
    fn begin_server_query(&mut self) -> Result<Operation, Error>;
    fn wait_server(&mut self, operation: Operation) -> Result<ServerDefaults, Error>;
    fn begin_source_query(&mut self) -> Result<Operation, Error>;
    fn wait_sources(&mut self, operation: Operation) -> Result<Vec<SourceRecord>, Error>;
    /// Consumes the handle, cancelling first when `cancelled`.
    fn end_operation(&mut self, operation: Operation, cancelled: bool);
    fn now(&self) -> Instant;
    fn rest(&mut self);
}

pub fn wait_ready(session: &mut impl Session) -> Result<(), Error> {
    let deadline = session.now() + CONNECT_TIMEOUT;
    loop {
        match session.context_state() {
            PA_CONTEXT_READY => return Ok(()),
            PA_CONTEXT_FAILED | PA_CONTEXT_TERMINATED => {
                return Err(backend("pa_context_connect", session.errno()));
            }
            _ => {}
        }
        session.iterate()?;
        if session.now() >= deadline {
            return Err(Error::Timeout {
                operation: "pa_context_connect",
            });
        }
        session.rest();
    }
}

pub fn wait_operation(
    session: &mut impl Session,
    operation: Option<Operation>,
    name: &'static str,
) -> Result<(), Error> {
    let Some(operation) = operation else {
        return Err(backend(name, session.errno()));
    };
    let deadline = session.now() + QUERY_TIMEOUT;
    loop {
        match session.operation_state(operation) {
            PA_OPERATION_DONE => {
                session.end_operation(operation, false);
                return Ok(());
            }
            PA_OPERATION_CANCELLED => {
                session.end_operation(operation, false);
                let code = session.errno();
                return Err(if code == 0 {
                    Error::Timeout { operation: name }
                } else {
                    backend(name, code)
                });
            }
            _ => {}
        }
        if session.now() >= deadline {
            session.end_operation(operation, true);
            return Err(Error::Timeout { operation: name });
        }
        if let Err(error) = session.iterate() {
            session.end_operation(operation, true);
            return Err(error);
        }
        session.rest();
    }
}

pub fn enumerate(session: &mut impl Session) -> Result<Vec<DeviceInfo>, Error> {
    wait_ready(session)?;
    let operation = session.begin_server_query()?;
    let defaults = session.wait_server(operation)?;
    let operation = session.begin_source_query()?;
    let records = session.wait_sources(operation)?;
    Ok(records
        .into_iter()
        .filter_map(|record| describe_source(record, &defaults))
        .collect())
}

pub const fn bytes_for(samples: usize) -> Option<usize> {
    samples.checked_mul(size_of::<f32>())
}

pub const fn read_frames(
    status: i32,
    error: i32,
    samples: usize,
    channels: usize,
) -> Result<usize, Error> {
    if status < 0 {
        return Err(backend("pa_simple_read", error));
    }
    Ok(samples / channels)
}

pub fn device_cstring(id: &DeviceId) -> Result<CString, Error> {
    CString::new(id.as_str()).map_err(|_| Error::InvalidId)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::enumerate::PA_INVALID_INDEX;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Iterate,
        BeginServer,
        WaitServer,
        BeginSource,
        WaitSources,
        End { cancelled: bool },
    }

    struct Fake {
        clock: Instant,
        context: i32,
        errno: i32,
        operation: i32,
        iteration_error: Option<Error>,
        calls: Vec<Call>,
        defaults: Option<Result<ServerDefaults, Error>>,
        sources: Option<Result<Vec<SourceRecord>, Error>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                clock: Instant::now(),
                context: 0,
                errno: 0,
                operation: PA_OPERATION_DONE,
                iteration_error: None,
                calls: Vec::new(),
                defaults: None,
                sources: None,
            }
        }

        fn calls(&self) -> &[Call] {
            &self.calls
        }

        fn cancels(&self) -> usize {
            self.calls
                .iter()
                .filter(|call| **call == Call::End { cancelled: true })
                .count()
        }
    }

    impl Session for Fake {
        fn iterate(&mut self) -> Result<(), Error> {
            self.calls.push(Call::Iterate);
            self.iteration_error.take().map_or(Ok(()), Err)
        }

        fn errno(&mut self) -> i32 {
            self.errno
        }

        fn context_state(&mut self) -> i32 {
            self.context
        }

        fn operation_state(&mut self, _operation: Operation) -> i32 {
            self.operation
        }

        fn begin_server_query(&mut self) -> Result<Operation, Error> {
            self.calls.push(Call::BeginServer);
            Ok(Operation(self.calls.len()))
        }

        fn wait_server(&mut self, operation: Operation) -> Result<ServerDefaults, Error> {
            self.calls.push(Call::WaitServer);
            wait_operation(self, Some(operation), "pa_context_get_server_info")?;
            self.defaults
                .take()
                .unwrap_or(Ok(ServerDefaults::default()))
        }

        fn begin_source_query(&mut self) -> Result<Operation, Error> {
            self.calls.push(Call::BeginSource);
            Ok(Operation(self.calls.len()))
        }

        fn wait_sources(&mut self, operation: Operation) -> Result<Vec<SourceRecord>, Error> {
            self.calls.push(Call::WaitSources);
            wait_operation(self, Some(operation), "pa_context_get_source_info_list")?;
            self.sources.take().unwrap_or(Ok(Vec::new()))
        }

        fn end_operation(&mut self, _operation: Operation, cancelled: bool) {
            self.calls.push(Call::End { cancelled });
        }

        fn now(&self) -> Instant {
            self.clock
        }

        fn rest(&mut self) {
            self.clock += POLL;
        }
    }

    fn record(name: &str) -> SourceRecord {
        SourceRecord {
            name: Some(name.to_owned()),
            description: None,
            monitor_of_sink: PA_INVALID_INDEX,
            monitor_of_sink_name: None,
            sample_rate: 48_000,
            channels: 2,
        }
    }

    #[test]
    fn device_order_follows_the_server_reply() {
        let mut fake = Fake::new();
        fake.context = PA_CONTEXT_READY;
        fake.sources = Some(Ok(vec![record("mic"), record("monitor"), record("second")]));
        let devices = enumerate(&mut fake).unwrap();
        let ids: Vec<&str> = devices.iter().map(|device| device.id().as_str()).collect();
        assert_eq!(ids, ["mic", "monitor", "second"]);
    }

    #[test]
    fn failed_server_query_does_not_issue_a_source_query() {
        let mut fake = Fake::new();
        fake.context = PA_CONTEXT_READY;
        let failure = Error::Backend {
            operation: "pa_context_get_server_info",
            code: -1,
        };
        fake.defaults = Some(Err(failure.clone()));
        assert_eq!(enumerate(&mut fake), Err(failure));
        assert!(!fake.calls().contains(&Call::BeginSource));
    }

    #[test]
    fn failed_source_query_returns_backend() {
        let mut fake = Fake::new();
        fake.context = PA_CONTEXT_READY;
        let failure = Error::Backend {
            operation: "pa_context_get_source_info_list",
            code: -22,
        };
        fake.sources = Some(Err(failure.clone()));
        assert_eq!(enumerate(&mut fake), Err(failure));
    }

    #[test]
    fn failed_and_terminated_contexts_are_connect_backend_errors() {
        for state in [PA_CONTEXT_FAILED, PA_CONTEXT_TERMINATED] {
            let mut fake = Fake::new();
            fake.context = state;
            fake.errno = -32;
            assert_eq!(
                wait_ready(&mut fake),
                Err(Error::Backend {
                    operation: "pa_context_connect",
                    code: -32,
                })
            );
        }
    }

    #[test]
    fn ready_context_returns_immediately() {
        let mut fake = Fake::new();
        fake.context = PA_CONTEXT_READY;
        assert_eq!(wait_ready(&mut fake), Ok(()));
        assert_eq!(fake.calls(), []);
    }

    #[test]
    fn never_ready_context_times_out() {
        let mut fake = Fake::new();
        assert_eq!(
            wait_ready(&mut fake),
            Err(Error::Timeout {
                operation: "pa_context_connect",
            })
        );
        assert_ne!(fake.calls(), []);
        assert!(fake.calls().iter().all(|call| *call == Call::Iterate));
    }

    #[test]
    fn done_operation_releases_without_cancelling() {
        let mut fake = Fake::new();
        assert_eq!(
            wait_operation(&mut fake, Some(Operation(7)), "query"),
            Ok(())
        );
        assert_eq!(fake.calls(), [Call::End { cancelled: false }].as_slice());
        assert_eq!(fake.cancels(), 0);
    }

    #[test]
    fn cancelled_operation_without_errno_is_a_timeout() {
        let mut fake = Fake::new();
        fake.operation = PA_OPERATION_CANCELLED;
        assert_eq!(
            wait_operation(&mut fake, Some(Operation(1)), "query"),
            Err(Error::Timeout { operation: "query" })
        );
        assert_eq!(fake.cancels(), 0);
    }

    #[test]
    fn cancelled_operation_with_errno_is_a_backend_error() {
        let mut fake = Fake::new();
        fake.operation = PA_OPERATION_CANCELLED;
        fake.errno = -32;
        assert_eq!(
            wait_operation(&mut fake, Some(Operation(1)), "query"),
            Err(Error::Backend {
                operation: "query",
                code: -32,
            })
        );
        assert_eq!(fake.cancels(), 0);
    }

    #[test]
    fn query_deadline_cancels_once_before_the_timeout() {
        let mut fake = Fake::new();
        fake.operation = 0;
        assert_eq!(
            wait_operation(&mut fake, Some(Operation(1)), "query"),
            Err(Error::Timeout { operation: "query" })
        );
        assert_eq!(fake.cancels(), 1);
    }

    #[test]
    fn iteration_failure_cancels_before_returning() {
        let mut fake = Fake::new();
        fake.operation = 0;
        let failure = Error::Backend {
            operation: "pa_mainloop_iterate",
            code: -1,
        };
        fake.iteration_error = Some(failure.clone());
        assert_eq!(
            wait_operation(&mut fake, Some(Operation(1)), "query"),
            Err(failure)
        );
        assert_eq!(
            fake.calls(),
            [Call::Iterate, Call::End { cancelled: true }].as_slice()
        );
    }

    #[test]
    fn missing_operation_is_a_backend_error() {
        let mut fake = Fake::new();
        fake.errno = -32;
        assert_eq!(
            wait_operation(&mut fake, None, "query"),
            Err(Error::Backend {
                operation: "query",
                code: -32,
            })
        );
        assert_eq!(fake.calls(), []);
    }

    #[test]
    fn bytes_for_rejects_overflow() {
        assert_eq!(bytes_for(4), Some(16));
        assert_eq!(bytes_for(usize::MAX), None);
    }

    #[test]
    fn read_reports_whole_frames() {
        assert_eq!(read_frames(0, 0, 8, 2), Ok(4));
    }

    #[test]
    fn negative_read_status_is_a_backend_error() {
        assert_eq!(
            read_frames(-1, -32, 8, 2),
            Err(Error::Backend {
                operation: "pa_simple_read",
                code: -32,
            })
        );
    }

    #[test]
    fn device_id_with_an_interior_nul_is_invalid() {
        let id = DeviceId::new("speaker\0suffix").unwrap();
        assert_eq!(device_cstring(&id), Err(Error::InvalidId));

        let id = DeviceId::new("speaker").unwrap();
        assert_eq!(device_cstring(&id).unwrap().to_str(), Ok("speaker"));
    }
}
