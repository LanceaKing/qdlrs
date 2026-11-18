// SPDX-License-Identifier: BSD-3-Clause

mod support;

use qdl::{
    firehose_program_storage, firehose_read, firehose_read_storage, firehose_read_welcome,
    parsers::firehose_parser_ack_nak,
    types::{
        FirehoseConfiguration, FirehoseStatus, MutationConfirmation, QdlChan, QdlDevice,
        QdlReadWrite,
    },
};
use std::{
    collections::VecDeque,
    io::{self, BufRead, ErrorKind, Read, Write},
    sync::Mutex,
    time::{Duration, Instant},
};

use support::{ACK, Input, LOG, NAK, ScriptedIo, Trace, assert_no_reset, bytes, device};

const RAW_ACK: &[u8] = b"<data><response value=\"ACK\" rawmode=\"true\"/></data>";
const WELCOME_END: &[u8] = b"<data><log value=\"INFO: End of supported functions\"/></data>";

#[derive(Debug)]
struct StopGraceExpired;

impl std::fmt::Display for StopGraceExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("scripted response wait expired")
    }
}

impl std::error::Error for StopGraceExpired {}

struct TimedRead {
    input: Input,
    elapsed: u64,
    request_stop: bool,
}

impl TimedRead {
    fn data(data: &[u8], elapsed: u64) -> Self {
        Self {
            input: bytes(data),
            elapsed,
            request_stop: false,
        }
    }

    fn error(kind: ErrorKind, elapsed: u64) -> Self {
        Self {
            input: Input::Error(kind),
            elapsed,
            request_stop: false,
        }
    }

    fn requesting_stop(mut self) -> Self {
        self.request_stop = true;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Event {
    Check,
    ResponseRead,
    Consume,
    RawRead,
    Write,
}

struct ClockAdvance {
    event: Event,
    remaining: usize,
    elapsed: Duration,
}

struct ScriptedChannel {
    inner: QdlDevice<ScriptedIo>,
    clock: Instant,
    stop_requested_at: Option<Instant>,
    schedule: VecDeque<(u64, bool)>,
    events: Mutex<Vec<Event>>,
    advance: Option<ClockAdvance>,
}

impl ScriptedChannel {
    fn new(reads: impl IntoIterator<Item = TimedRead>) -> (Self, Trace) {
        Self::with_raw_tail(reads, [])
    }

    fn with_raw_tail(
        reads: impl IntoIterator<Item = TimedRead>,
        raw_tail: impl IntoIterator<Item = Input>,
    ) -> (Self, Trace) {
        let (inputs, schedule): (Vec<_>, VecDeque<_>) = reads
            .into_iter()
            .map(|step| (step.input, (step.elapsed, step.request_stop)))
            .unzip();
        let (inner, trace) = device(inputs.into_iter().chain(raw_tail));
        (
            Self {
                inner,
                clock: Instant::now(),
                stop_requested_at: None,
                schedule,
                events: Mutex::new(Vec::new()),
                advance: None,
            },
            trace,
        )
    }

    fn advance_after(&mut self, event: Event, occurrence: usize, seconds: u64) {
        self.advance = Some(ClockAdvance {
            event,
            remaining: occurrence,
            elapsed: Duration::from_secs(seconds),
        });
    }

    fn record(&mut self, event: Event) {
        self.events.get_mut().unwrap().push(event);
        if let Some(advance) = self.advance.as_mut().filter(|a| a.event == event) {
            advance.remaining -= 1;
            if advance.remaining == 0 {
                self.clock += advance.elapsed;
                self.advance = None;
            }
        }
    }
}

impl Read for ScriptedChannel {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(out)?;
        self.record(Event::RawRead);
        Ok(n)
    }
}

impl BufRead for ScriptedChannel {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        panic!("response parsing must use the response-specific refill")
    }

    fn consume(&mut self, amount: usize) {
        self.inner.consume(amount);
        self.record(Event::Consume);
    }
}

impl QdlReadWrite for ScriptedChannel {
    fn fill_response_buf(&mut self) -> io::Result<&[u8]> {
        self.record(Event::ResponseRead);
        let (elapsed, request_stop) = self.schedule.pop_front().expect("unscripted XML read");
        if request_stop {
            self.stop_requested_at.get_or_insert(self.clock);
        }
        self.clock += Duration::from_secs(elapsed);
        self.inner.fill_buf()
    }
}

impl Write for ScriptedChannel {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(data)?;
        self.record(Event::Write);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl QdlChan for ScriptedChannel {
    fn fh_config(&self) -> &FirehoseConfiguration {
        self.inner.fh_config()
    }

    fn mut_fh_config(&mut self) -> &mut FirehoseConfiguration {
        self.inner.mut_fh_config()
    }

    fn fill_response_buf(&mut self) -> io::Result<&[u8]> {
        QdlReadWrite::fill_response_buf(self)
    }

    fn check_response_wait(&self) -> anyhow::Result<()> {
        self.events.lock().unwrap().push(Event::Check);
        if self.stop_requested_at.is_some_and(|requested| {
            self.clock.duration_since(requested) >= Duration::from_secs(10)
        }) {
            return Err(StopGraceExpired.into());
        }
        Ok(())
    }
}

#[test]
fn retryable_empty_completion_observes_the_deadline_before_another_read() {
    let (mut channel, trace) = ScriptedChannel::new([
        TimedRead::error(ErrorKind::Interrupted, 3).requesting_stop(),
        TimedRead::error(ErrorKind::Interrupted, 4),
        TimedRead::error(ErrorKind::Interrupted, 4),
        TimedRead::data(ACK, 0),
    ]);
    let error = firehose_read(&mut channel, firehose_parser_ack_nak).unwrap_err();
    assert!(error.is::<StopGraceExpired>());
    assert_eq!(channel.schedule.len(), 1);
    assert!(trace.lock().unwrap().writes.is_empty());
}

#[test]
fn device_forwards_response_refills_through_a_borrowed_transport() {
    let packet = [ACK, b"raw"].concat();
    let (mut transport, trace) = ScriptedChannel::new([
        TimedRead::error(ErrorKind::Interrupted, 0),
        TimedRead::data(&packet, 0),
    ]);
    let config = *transport.fh_config();
    let borrowed: &mut dyn QdlReadWrite = &mut transport;
    let mut dev = QdlDevice {
        rw: Box::new(borrowed),
        fh_cfg: config,
        reset_on_drop: false,
    };
    assert_eq!(
        firehose_read(&mut dev, firehose_parser_ack_nak).unwrap(),
        FirehoseStatus::Ack
    );
    let mut raw = [0; 3];
    dev.read_exact(&mut raw).unwrap();
    assert_eq!(&raw, b"raw");
    assert!(trace.lock().unwrap().writes.is_empty());
}

#[test]
fn expired_deadline_prevents_starting_the_next_read() {
    let (mut channel, trace) = ScriptedChannel::new([]);
    channel.stop_requested_at = Some(channel.clock - Duration::from_secs(11));
    let error = firehose_read(&mut channel, firehose_parser_ack_nak).unwrap_err();
    assert!(error.is::<StopGraceExpired>());
    assert!(trace.lock().unwrap().writes.is_empty());
}

#[test]
fn logs_and_partial_xml_do_not_extend_the_grace_period() {
    let (mut channel, trace) = ScriptedChannel::new([
        TimedRead::data(b"\r\n ", 0).requesting_stop(),
        TimedRead::data(LOG, 4),
        TimedRead::data(b"<data><response", 3),
        TimedRead::data(b" value=\"ACK\"/></data>", 4),
    ]);
    let error = firehose_read(&mut channel, firehose_parser_ack_nak).unwrap_err();
    assert!(error.is::<StopGraceExpired>());
    assert!(channel.schedule.is_empty());
    assert!(trace.lock().unwrap().writes.is_empty());
}

#[test]
fn logs_or_unfinished_xml_expire_without_a_terminal_response_or_mutation() {
    for tail in [LOG, b"<data><response"] {
        let (mut channel, trace) = ScriptedChannel::new([
            TimedRead::data(LOG, 3).requesting_stop(),
            TimedRead::data(LOG, 5),
            TimedRead::data(tail, 3),
        ]);
        let error = firehose_read(&mut channel, firehose_parser_ack_nak).unwrap_err();
        assert!(error.is::<StopGraceExpired>());
        assert!(channel.schedule.is_empty());
        assert!(trace.lock().unwrap().writes.is_empty());
    }
}

#[test]
fn command_checks_after_the_log_and_before_the_next_response_read() {
    use Event::{Check, Consume, ResponseRead};

    let (mut channel, _) = ScriptedChannel::new([
        TimedRead::data(LOG, 7).requesting_stop(),
        TimedRead::data(ACK, 2),
    ]);
    assert_eq!(
        firehose_read(&mut channel, firehose_parser_ack_nak).unwrap(),
        FirehoseStatus::Ack
    );
    assert_eq!(
        *channel.events.lock().unwrap(),
        [
            Check,
            ResponseRead,
            Check,
            Consume,
            Check,
            Check,
            ResponseRead,
            Check,
            Consume
        ]
    );
}

#[test]
fn welcome_timeout_after_stop_is_not_a_successful_idle_boundary() {
    let (mut channel, trace) = ScriptedChannel::new([
        TimedRead::data(LOG, 1).requesting_stop(),
        TimedRead::error(ErrorKind::TimedOut, 10),
    ]);
    let error = firehose_read_welcome(&mut channel).unwrap_err();
    assert!(error.is::<StopGraceExpired>());
    assert!(trace.lock().unwrap().writes.is_empty());
}

#[test]
fn welcome_marker_checks_deadline_after_consuming_the_log() {
    let packet = [WELCOME_END, ACK].concat();
    let (mut channel, trace) =
        ScriptedChannel::new([TimedRead::data(&packet, 9).requesting_stop()]);
    channel.advance_after(Event::Consume, 1, 2);
    let error = firehose_read_welcome(&mut channel).unwrap_err();
    assert!(error.is::<StopGraceExpired>());
    assert_eq!(channel.inner.fill_buf().unwrap(), ACK);
    assert!(trace.lock().unwrap().writes.is_empty());
}

#[test]
fn timely_welcome_marker_leaves_the_ack_buffered() {
    let packet = [WELCOME_END, ACK].concat();
    let (mut channel, _) = ScriptedChannel::new([TimedRead::data(&packet, 9).requesting_stop()]);
    let logs = firehose_read_welcome(&mut channel).unwrap();
    assert_eq!(logs, ["INFO: End of supported functions"]);
    assert_eq!(channel.inner.fill_buf().unwrap(), ACK);
}

#[test]
fn program_finishes_raw_writes_before_checking_the_expired_deadline() {
    let (mut channel, trace) =
        ScriptedChannel::new([TimedRead::data(RAW_ACK, 0).requesting_stop()]);
    channel.mut_fh_config().send_buffer_size = 4;
    // The XML command is write one. The first raw chunk is write two.
    channel.advance_after(Event::Write, 2, 11);
    let error =
        firehose_program_storage(&mut channel, &mut &b"datatail"[..], "sample", 2, 0, 0, "0")
            .unwrap_err();
    assert_eq!(error.confirmation, MutationConfirmation::Unknown);
    assert!(error.cause.is::<StopGraceExpired>());
    assert_no_reset(&trace);
    let trace = trace.lock().unwrap();
    assert_eq!(trace.writes.len(), 3);
    assert!(String::from_utf8_lossy(&trace.writes[0]).contains("<program "));
    assert_eq!(trace.writes[1], b"data");
    assert_eq!(trace.writes[2], b"tail");
}

#[test]
fn storage_read_finishes_raw_reads_before_checking_the_expired_deadline() {
    let (mut channel, trace) = ScriptedChannel::with_raw_tail(
        [TimedRead::data(RAW_ACK, 0).requesting_stop()],
        [bytes(b"data"), bytes(b"tail")],
    );
    channel.mut_fh_config().recv_buffer_size = 4;
    channel.advance_after(Event::RawRead, 1, 11);
    let mut out = Vec::new();
    let error = firehose_read_storage(&mut channel, &mut out, 2, 0, 0, 0).unwrap_err();
    assert!(error.is::<StopGraceExpired>());
    assert_eq!(out, b"datatail");
    assert_no_reset(&trace);
    let trace = trace.lock().unwrap();
    assert_eq!(trace.writes.len(), 1);
    assert!(String::from_utf8_lossy(&trace.writes[0]).contains("<read "));
}

#[test]
fn program_initial_response_expiry_leaves_only_the_command_sent() {
    let (mut channel, trace) = ScriptedChannel::new([
        TimedRead::data(LOG, 3).requesting_stop(),
        TimedRead::data(LOG, 8),
    ]);
    let error = firehose_program_storage(&mut channel, &mut &b"test"[..], "sample", 1, 0, 0, "0")
        .unwrap_err();
    assert_eq!(error.confirmation, MutationConfirmation::Unknown);
    assert!(error.cause.is::<StopGraceExpired>());
    assert_no_reset(&trace);
    let trace = trace.lock().unwrap();
    assert_eq!(trace.writes.len(), 1);
    assert!(String::from_utf8_lossy(&trace.writes[0]).contains("<program "));
}

#[test]
fn program_completion_expiry_preserves_transferred_bytes_and_unknown_write() {
    for tail in [LOG, b"<data><resp"] {
        let (mut channel, trace) = ScriptedChannel::new([
            TimedRead::data(ACK, 0),
            TimedRead::data(LOG, 0).requesting_stop(),
            TimedRead::data(LOG, 5),
            TimedRead::data(tail, 6),
        ]);
        let error =
            firehose_program_storage(&mut channel, &mut &b"test"[..], "sample", 1, 0, 0, "0")
                .unwrap_err();
        assert_eq!(error.confirmation, MutationConfirmation::Unknown);
        assert!(error.cause.is::<StopGraceExpired>());
        assert_no_reset(&trace);
        let trace = trace.lock().unwrap();
        assert_eq!(trace.writes.len(), 2);
        assert!(String::from_utf8_lossy(&trace.writes[0]).contains("<program "));
        assert_eq!(trace.writes[1], b"test");
    }
}

#[test]
fn timely_program_completion_preserves_the_device_ack_or_nak() {
    for (terminal, acknowledged) in [(ACK, true), (NAK, false)] {
        let (mut channel, trace) = ScriptedChannel::new([
            TimedRead::data(ACK, 0),
            TimedRead::data(LOG, 0).requesting_stop(),
            TimedRead::data(terminal, 9),
        ]);
        let result =
            firehose_program_storage(&mut channel, &mut &b"test"[..], "sample", 1, 0, 0, "0");
        if acknowledged {
            result.unwrap();
        } else {
            assert_eq!(
                result.unwrap_err().confirmation,
                MutationConfirmation::Rejected
            );
        }
        assert_no_reset(&trace);
        let trace = trace.lock().unwrap();
        assert_eq!(trace.writes.len(), 2);
        assert_eq!(trace.writes[1], b"test");
    }
}
