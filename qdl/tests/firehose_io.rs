// SPDX-License-Identifier: BSD-3-Clause
mod support;

use qdl::{
    firehose_program_storage, firehose_read, firehose_read_storage,
    parsers::firehose_parser_ack_nak,
};
use std::io::{ErrorKind, Read};
use support::*;

struct ShortSource(&'static [u8]);
impl Read for ShortSource {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let len = out.len().min(self.0.len()).min(2);
        out[..len].copy_from_slice(&self.0[..len]);
        self.0 = &self.0[len..];
        Ok(len)
    }
}

#[test]
fn program_continues_short_file_reads() {
    let (mut dev, trace) = device([bytes(ACK), bytes(ACK)]);
    firehose_program_storage(&mut dev, &mut ShortSource(b"abcdefgh"), "p", 2, 0, 0, "0").unwrap();
    assert!(trace.lock().unwrap().accepted.ends_with(b"abcdefgh"));
}

#[test]
fn program_rejects_premature_file_eof() {
    let (mut dev, trace) = device([bytes(ACK), bytes(ACK)]);
    assert!(firehose_program_storage(&mut dev, &mut &b"ab"[..], "p", 2, 0, 0, "0").is_err());
    drop(dev);
    assert_no_reset(&trace);
}

#[test]
fn log_then_timeout_is_not_ack() {
    let (mut dev, _) = device([bytes(LOG), Input::Error(ErrorKind::TimedOut)]);
    assert!(firehose_read(&mut dev, firehose_parser_ack_nak).is_err());
}

#[test]
fn read_rejects_transport_eof() {
    let (mut dev, _) = device([bytes(ACK), Input::Eof]);
    assert!(firehose_read_storage(&mut dev, &mut Vec::new(), 1, 0, 0, 0).is_err());
}

use qdl::{
    firehose_configure, firehose_get_storage_info, firehose_patch, firehose_read_welcome,
    firehose_reset, firehose_set_bootable, firehose_write,
    parsers::firehose_parser_configure_response,
    types::{
        FirehoseResetMode, FirehoseStatus, MutationConfirmation, MutationFailure, QdlBackend,
        ReadRejected,
    },
};
use std::io::BufRead;

fn program(dev: &mut qdl::types::QdlDevice<ScriptedIo>) -> Result<(), MutationFailure> {
    firehose_program_storage(dev, &mut &b"abcdefgh"[..], "p", 2, 0, 0, "0")
}

fn io_kind(error: &anyhow::Error) -> ErrorKind {
    error
        .downcast_ref::<std::io::Error>()
        .expect("original IO error retained")
        .kind()
}

fn one_program_no_followup(trace: &Trace) {
    assert_no_reset(trace);
    assert_eq!(
        String::from_utf8_lossy(&trace.lock().unwrap().accepted)
            .matches("<program ")
            .count(),
        1
    );
}

#[test]
fn scripted_read_and_bufread_share_position() {
    let mut io = ScriptedIo::new([bytes(b"abcdef"), Input::Eof]);
    assert_eq!(io.fill_buf().unwrap(), b"abcdef");
    assert_eq!(io.fill_buf().unwrap(), b"abcdef");
    io.consume(2);
    let mut out = [0; 2];
    io.read_exact(&mut out).unwrap();
    assert_eq!(&out, b"cd");
    assert_eq!(io.fill_buf().unwrap(), b"ef");
    io.consume(2);
    assert!(io.fill_buf().unwrap().is_empty());
}

#[test]
fn program_continues_serial_short_writes() {
    let (mut dev, trace) = device([bytes(ACK), bytes(ACK)]);
    dev.rw.output.extend([
        Output::Accept(usize::MAX),
        Output::Accept(3),
        Output::Accept(2),
    ]);
    program(&mut dev).unwrap();
    assert!(trace.lock().unwrap().accepted.ends_with(b"abcdefgh"));
    assert_eq!(trace.lock().unwrap().writes.len(), 4);
}

#[test]
fn program_rejects_usb_short_write_without_resending() {
    let (mut dev, trace) = device([bytes(ACK)]);
    dev.fh_cfg.backend = QdlBackend::Usb;
    dev.rw
        .output
        .extend([Output::Accept(usize::MAX), Output::Accept(3)]);
    let error = program(&mut dev).unwrap_err();
    assert_eq!(error.confirmation, MutationConfirmation::Unknown);
    assert_eq!(io_kind(&error.cause), ErrorKind::WriteZero);
    drop(dev);
    assert_eq!(trace.lock().unwrap().writes.len(), 2);
    assert!(trace.lock().unwrap().accepted.ends_with(b"abc"));
    one_program_no_followup(&trace);
}

#[test]
fn program_sends_one_explicit_usb_zlp() {
    let (mut dev, trace) = device([bytes(ACK), bytes(ACK)]);
    dev.fh_cfg.backend = QdlBackend::Usb;
    program(&mut dev).unwrap();
    drop(dev);
    let record = trace.lock().unwrap();
    assert_eq!(record.writes.iter().filter(|w| w.is_empty()).count(), 1);
    assert_eq!(record.flushes, 2);
}

#[test]
fn program_reports_zlp_failure_as_unknown() {
    let (mut dev, trace) = device([bytes(ACK)]);
    dev.fh_cfg.backend = QdlBackend::Usb;
    dev.rw.output.extend([
        Output::Accept(usize::MAX),
        Output::Accept(usize::MAX),
        Output::Error(ErrorKind::BrokenPipe),
    ]);
    let error = program(&mut dev).unwrap_err();
    assert_eq!(error.confirmation, MutationConfirmation::Unknown);
    assert_eq!(io_kind(&error.cause), ErrorKind::BrokenPipe);
    drop(dev);
    one_program_no_followup(&trace);
}

#[test]
fn program_never_sends_incomplete_input_buffer() {
    for last in [Input::Eof, Input::Error(ErrorKind::PermissionDenied)] {
        let (mut dev, trace) = device([bytes(ACK)]);
        let mut source = ScriptedIo::new([bytes(b"abc"), last]);
        let error = firehose_program_storage(&mut dev, &mut source, "p", 2, 0, 0, "0").unwrap_err();
        assert_eq!(error.confirmation, MutationConfirmation::Unknown);
        assert!(matches!(
            io_kind(&error.cause),
            ErrorKind::UnexpectedEof | ErrorKind::PermissionDenied
        ));
        drop(dev);
        assert_eq!(trace.lock().unwrap().writes.len(), 1);
        one_program_no_followup(&trace);
    }
}

#[test]
fn program_stops_after_later_input_failure() {
    let (mut dev, trace) = device([bytes(ACK)]);
    let mut source = ScriptedIo::new([bytes(b"abcdefgh"), bytes(b"ij"), Input::Eof]);
    let error = firehose_program_storage(&mut dev, &mut source, "p", 4, 0, 0, "0").unwrap_err();
    assert_eq!(io_kind(&error.cause), ErrorKind::UnexpectedEof);
    assert!(trace.lock().unwrap().accepted.ends_with(b"abcdefgh"));
    assert_eq!(trace.lock().unwrap().writes.len(), 2);
}

#[test]
fn program_zero_progress_and_hard_write_errors_are_unknown() {
    for output in [Output::Accept(0), Output::Error(ErrorKind::BrokenPipe)] {
        let (mut dev, trace) = device([bytes(ACK)]);
        dev.rw.output.extend([Output::Accept(usize::MAX), output]);
        let error = program(&mut dev).unwrap_err();
        assert_eq!(error.confirmation, MutationConfirmation::Unknown);
        assert!(matches!(
            io_kind(&error.cause),
            ErrorKind::WriteZero | ErrorKind::BrokenPipe
        ));
        drop(dev);
        one_program_no_followup(&trace);
    }
}

#[test]
fn interrupted_file_and_serial_io_can_continue() {
    let (mut dev, _) = device([Input::Error(ErrorKind::Interrupted), bytes(ACK), bytes(ACK)]);
    dev.rw.output.extend([
        Output::Error(ErrorKind::Interrupted),
        Output::Accept(usize::MAX),
        Output::Error(ErrorKind::Interrupted),
    ]);
    let mut source = ScriptedIo::new([Input::Error(ErrorKind::Interrupted), bytes(b"abcdefgh")]);
    firehose_program_storage(&mut dev, &mut source, "p", 2, 0, 0, "0").unwrap();
}

#[test]
fn final_ack_timeout_is_unknown_and_drop_sends_nothing() {
    let (mut dev, trace) = device([bytes(ACK), bytes(LOG), Input::Error(ErrorKind::TimedOut)]);
    dev.reset_on_drop = true; // Old callers cannot re-enable implicit commands.
    let error = program(&mut dev).unwrap_err();
    assert_eq!(error.confirmation, MutationConfirmation::Unknown);
    assert_eq!(io_kind(&error.cause), ErrorKind::TimedOut);
    drop(dev);
    one_program_no_followup(&trace);
}

#[test]
fn initial_and_final_program_nak_are_rejected() {
    for (input, writes) in [(vec![bytes(NAK)], 1), (vec![bytes(ACK), bytes(NAK)], 2)] {
        let (mut dev, trace) = device(input);
        dev.reset_on_drop = true;
        assert_eq!(
            program(&mut dev).unwrap_err().confirmation,
            MutationConfirmation::Rejected
        );
        drop(dev);
        assert_eq!(trace.lock().unwrap().writes.len(), writes);
        one_program_no_followup(&trace);
    }
}

#[test]
fn partial_command_failure_retains_original_error() {
    let (mut dev, trace) = device([]);
    dev.rw.output.extend([
        Output::Accept(10),
        Output::Error(ErrorKind::ConnectionReset),
    ]);
    let error = program(&mut dev).unwrap_err();
    assert_eq!(error.confirmation, MutationConfirmation::Unknown);
    assert_eq!(io_kind(&error.cause), ErrorKind::ConnectionReset);
    assert_eq!(trace.lock().unwrap().accepted.len(), 10);
    drop(dev);
    assert_no_reset(&trace);
}

#[test]
fn command_write_and_flush_errors_are_not_replaced_by_reset() {
    for flush in [false, true] {
        let (mut dev, trace) = device([]);
        if flush {
            dev.rw.flush_error = Some(ErrorKind::BrokenPipe);
        } else {
            dev.rw
                .output
                .push_back(Output::Error(ErrorKind::BrokenPipe));
        }
        let error = firehose_write(&mut dev, &mut b"<data><nop/></data>".to_vec()).unwrap_err();
        assert_eq!(io_kind(&error), ErrorKind::BrokenPipe);
        drop(dev);
        assert_no_reset(&trace);
        assert_eq!(trace.lock().unwrap().writes.len(), 1);
    }
}

#[test]
fn program_invalid_sizes_are_not_sent() {
    for case in 0..6 {
        let (mut dev, trace) = device([]);
        match case {
            0 => dev.fh_cfg.storage_sector_size = 0,
            1 => dev.fh_cfg.send_buffer_size = 0,
            2 => dev.fh_cfg.send_buffer_size = 3,
            3 => dev.fh_cfg.recv_buffer_size = 0,
            4 => dev.fh_cfg.xml_buf_size = 0,
            _ => dev.fh_cfg.send_buffer_size = usize::MAX,
        }
        assert_eq!(
            program(&mut dev).unwrap_err().confirmation,
            MutationConfirmation::NotSent
        );
        assert!(trace.lock().unwrap().writes.is_empty());
    }
    let (mut dev, trace) = device([]);
    let error =
        firehose_program_storage(&mut dev, &mut &b""[..], "p", usize::MAX, 0, 0, "0").unwrap_err();
    assert_eq!(error.confirmation, MutationConfirmation::NotSent);
    assert!(trace.lock().unwrap().writes.is_empty());
}

#[test]
fn read_handles_coalesced_xml_raw_data_and_short_local_writes() {
    let (mut dev, _) = device([bytes(&[ACK, b"abcdefgh", ACK, ACK].concat())]);
    let mut out = ScriptedIo::new([]);
    out.output.extend([Output::Accept(2), Output::Accept(1)]);
    firehose_read_storage(&mut dev, &mut out, 2, 0, 0, 0).unwrap();
    assert_eq!(out.trace.lock().unwrap().accepted, b"abcdefgh");
    assert_eq!(out.trace.lock().unwrap().flushes, 1);
    assert_eq!(
        firehose_read(&mut dev, firehose_parser_ack_nak).unwrap(),
        FirehoseStatus::Ack
    );
}

#[test]
fn read_handles_fragmented_raw_data() {
    let (mut dev, _) = device([
        bytes(ACK),
        bytes(b"a"),
        Input::Error(ErrorKind::Interrupted),
        bytes(b"bc"),
        bytes(b"defgh"),
        bytes(ACK),
    ]);
    let mut out = Vec::new();
    firehose_read_storage(&mut dev, &mut out, 2, 0, 0, 0).unwrap();
    assert_eq!(out, b"abcdefgh");
}

#[test]
fn read_local_zero_write_hard_error_and_flush_failure_are_errors() {
    for case in 0..3 {
        let (mut dev, trace) = device([bytes(ACK), bytes(b"abcdefgh")]);
        let mut out = ScriptedIo::new([]);
        match case {
            0 => out.output.push_back(Output::Accept(0)),
            1 => out
                .output
                .push_back(Output::Error(ErrorKind::PermissionDenied)),
            _ => out.flush_error = Some(ErrorKind::Other),
        }
        let error = firehose_read_storage(&mut dev, &mut out, 2, 0, 0, 0).unwrap_err();
        assert!(matches!(
            io_kind(&error),
            ErrorKind::WriteZero | ErrorKind::PermissionDenied | ErrorKind::Other
        ));
        drop(dev);
        assert_no_reset(&trace);
    }
}

#[test]
fn read_missing_final_ack_is_failure_after_complete_output() {
    for final_input in [Input::Error(ErrorKind::TimedOut), Input::Eof, bytes(NAK)] {
        let (mut dev, trace) = device([bytes(ACK), bytes(b"abcdefgh"), final_input]);
        let mut out = Vec::new();
        assert!(firehose_read_storage(&mut dev, &mut out, 2, 0, 0, 0).is_err());
        assert_eq!(out, b"abcdefgh");
        drop(dev);
        assert_no_reset(&trace);
    }
}

#[test]
fn read_failure_in_raw_mode_is_not_read_rejection() {
    for final_input in [
        Input::Error(ErrorKind::TimedOut),
        Input::Error(ErrorKind::ConnectionReset),
        Input::Eof,
    ] {
        let (mut dev, _) = device([bytes(ACK), bytes(b"abc"), final_input]);
        let error = firehose_read_storage(&mut dev, &mut Vec::new(), 2, 0, 0, 0).unwrap_err();
        assert!(error.downcast_ref::<ReadRejected>().is_none());
        assert!(error.downcast_ref::<std::io::Error>().is_some());
    }
}

#[test]
fn complete_initial_read_nak_is_typed_and_leaves_command_boundary() {
    for nak in [
        NAK,
        b"<data><response value=\"NAK\" rawmode=\"false\"/></data>",
    ] {
        let (mut dev, trace) = device([bytes(&[nak, ACK].concat())]);
        let error = firehose_read_storage(&mut dev, &mut Vec::new(), 2, 0, 0, 0).unwrap_err();
        assert!(error.downcast_ref::<ReadRejected>().is_some());
        assert_eq!(
            firehose_read(&mut dev, firehose_parser_ack_nak).unwrap(),
            FirehoseStatus::Ack
        );
        drop(dev);
        assert_eq!(trace.lock().unwrap().writes.len(), 1);
        assert_no_reset(&trace);
    }
}

#[test]
fn read_nak_with_restart_retry_or_raw_mode_requires_session_stop() {
    for attributes in [
        "AttemptRestart=\"1\"",
        "AttemptRetry=\"1\"",
        "AttemptRetry=\"0\" rawmode=\"false\"",
        "rawmode=\"true\"",
        "rawmode=\"invalid\"",
        "rawmode=\"\"",
    ] {
        let xml = format!("<data><response value=\"NAK\" {attributes}/></data>");
        let (mut dev, trace) = device([bytes(xml.as_bytes())]);
        let mut out = Vec::new();
        let error = firehose_read_storage(&mut dev, &mut out, 2, 0, 0, 0).unwrap_err();
        assert!(
            error.downcast_ref::<ReadRejected>().is_none(),
            "{attributes}"
        );
        assert!(error.to_string().contains("command boundary"));
        assert!(out.is_empty());
        drop(dev);
        assert_eq!(trace.lock().unwrap().writes.len(), 1);
        assert_no_reset(&trace);
    }
}

#[test]
fn invalid_read_sizes_do_not_send_commands() {
    let (mut dev, trace) = device([]);
    assert!(firehose_read_storage(&mut dev, &mut Vec::new(), usize::MAX, 0, 0, 0).is_err());
    dev.fh_cfg.recv_buffer_size = 0;
    assert!(firehose_read_storage(&mut dev, &mut Vec::new(), 1, 0, 0, 0).is_err());
    assert!(trace.lock().unwrap().writes.is_empty());
}

#[test]
fn response_end_marker_can_split_at_every_byte() {
    for split in 1..ACK.len() {
        let (mut dev, _) = device([bytes(&ACK[..split]), bytes(&ACK[split..])]);
        assert_eq!(
            firehose_read(&mut dev, firehose_parser_ack_nak).unwrap(),
            FirehoseStatus::Ack
        );
    }
}

#[test]
fn welcome_end_marker_does_not_acknowledge_a_command() {
    let end = b"<data><log value=\"INFO: End of supported functions\"/></data>";
    let (mut dev, _) = device([bytes(end), Input::Error(ErrorKind::TimedOut)]);
    assert_eq!(
        io_kind(&firehose_read(&mut dev, firehose_parser_ack_nak).unwrap_err()),
        ErrorKind::TimedOut
    );
    let (mut dev, _) = device([bytes(end)]);
    assert_eq!(firehose_read_welcome(&mut dev).unwrap().len(), 1);
}

#[test]
fn welcome_timeout_only_completes_at_document_boundary() {
    let (mut dev, _) = device([bytes(LOG), Input::Error(ErrorKind::TimedOut)]);
    assert_eq!(firehose_read_welcome(&mut dev).unwrap(), ["hello"]);
    for welcome in [true, false] {
        let (mut dev, _) = device([bytes(b"<data><response"), Input::Error(ErrorKind::TimedOut)]);
        let error = if welcome {
            firehose_read_welcome(&mut dev).map(|_| ()).unwrap_err()
        } else {
            firehose_read(&mut dev, firehose_parser_ack_nak)
                .map(|_| ())
                .unwrap_err()
        };
        assert_eq!(io_kind(&error), ErrorKind::TimedOut);
    }
}

#[test]
fn welcome_accepts_trailing_xml_whitespace_before_idle_timeout() {
    for whitespace in [&b"\n"[..], &b"\r\n"[..], &b" \t\r\n"[..]] {
        let (mut dev, _) = device([
            bytes(&[LOG, whitespace].concat()),
            Input::Error(ErrorKind::TimedOut),
        ]);
        assert_eq!(firehose_read_welcome(&mut dev).unwrap(), ["hello"]);
    }
}

#[test]
fn welcome_accepts_fragmented_whitespace_between_declared_documents() {
    let second = b"<?xml version=\"1.0\"?><data><log value=\"second\"/></data>";
    let (mut dev, _) = device([
        bytes(&[LOG, b"\r"].concat()),
        bytes(b"\n\t"),
        bytes(&[&b" "[..], second, b"\r\n"].concat()),
        Input::Error(ErrorKind::TimedOut),
    ]);
    assert_eq!(
        firehose_read_welcome(&mut dev).unwrap(),
        ["hello", "second"]
    );
}

#[test]
fn welcome_whitespace_does_not_hide_partial_xml_or_invalid_bytes() {
    for tail in [&b"\n<data><log"[..], &b" \t\x0b"[..], &b"\r\n\x0c"[..]] {
        let (mut dev, _) = device([
            bytes(&[LOG, tail].concat()),
            Input::Error(ErrorKind::TimedOut),
        ]);
        assert_eq!(
            io_kind(&firehose_read_welcome(&mut dev).unwrap_err()),
            ErrorKind::TimedOut
        );
    }
}

#[test]
fn command_logs_with_trailing_whitespace_still_require_a_response() {
    let (mut dev, _) = device([
        bytes(&[LOG, b"\r\n"].concat()),
        Input::Error(ErrorKind::TimedOut),
    ]);
    assert_eq!(
        io_kind(&firehose_read(&mut dev, firehose_parser_ack_nak).unwrap_err()),
        ErrorKind::TimedOut
    );
}

#[test]
fn storage_read_preserves_raw_leading_whitespace() {
    let raw = b" \t\r\nDATA";
    let (mut dev, _) = device([bytes(&[ACK, raw, ACK].concat())]);
    let mut out = Vec::new();
    firehose_read_storage(&mut dev, &mut out, 2, 0, 0, 0).unwrap();
    assert_eq!(out, raw);
}

#[test]
fn response_eof_malformed_and_oversized_xml_are_errors() {
    for input in [
        Input::Eof,
        bytes(b"<data><response value=\"INVALID\"/></data>"),
        bytes(b"<data><bad/></data>"),
        bytes(b"<data></data>"),
        bytes(b"<data><response value=\"ACK\"/><response value=\"ACK\"/></data>"),
    ] {
        let (mut dev, _) = device([input]);
        assert!(firehose_read(&mut dev, firehose_parser_ack_nak).is_err());
    }
    let (mut dev, _) = device([bytes(b"<data>xxxxxxxxxx")]);
    dev.fh_cfg.xml_buf_size = 8;
    assert!(firehose_read(&mut dev, firehose_parser_ack_nak).is_err());
}

#[test]
fn restart_and_retry_requests_never_send_commands() {
    for hint in ["AttemptRestart", "AttemptRetry"] {
        let xml = format!("<data><response value=\"NAK\" {hint}=\"1\"/></data>");
        let (mut dev, trace) = device([bytes(xml.as_bytes())]);
        assert_eq!(
            firehose_read(&mut dev, firehose_parser_ack_nak).unwrap(),
            FirehoseStatus::Nak
        );
        drop(dev);
        assert!(trace.lock().unwrap().writes.is_empty());
    }
}

#[test]
fn storage_info_returns_logs_and_attributes_and_rejects_nak() {
    let (mut dev, _) = device([
        bytes(LOG),
        bytes(b"<data><response value=\"ACK\" sectors=\"32\"/></data>"),
    ]);
    let info = firehose_get_storage_info(&mut dev, 2).unwrap();
    assert_eq!(info.logs, ["hello"]);
    assert_eq!(info.attributes["sectors"], "32");
    let (mut dev, trace) = device([bytes(LOG), bytes(NAK)]);
    assert!(firehose_get_storage_info(&mut dev, 0).is_err());
    drop(dev);
    assert_no_reset(&trace);
}

const CONFIG: &[u8] = b"<data><response value=\"ACK\" Version=\"1\" MinVersionSupported=\"1\" MaxPayloadSizeToTargetInBytesSupported=\"16\" MaxPayloadSizeToTargetInBytes=\"8\" MaxXMLSizeInBytes=\"4096\"/></data>";

#[test]
fn configure_defaults_to_real_writes_and_never_reconfigures_implicitly() {
    let (mut dev, trace) = device([bytes(CONFIG)]);
    firehose_configure(&mut dev, false).unwrap();
    assert_eq!(
        firehose_read(&mut dev, firehose_parser_configure_response).unwrap(),
        FirehoseStatus::Ack
    );
    assert_eq!(dev.fh_cfg.send_buffer_size, 8);
    let record = trace.lock().unwrap();
    assert_eq!(record.writes.len(), 1);
    assert!(String::from_utf8_lossy(&record.accepted).contains("SkipWrite=\"0\""));
}

#[test]
fn configure_nak_and_invalid_attributes_never_reset_or_replace_configuration() {
    let (mut dev, trace) = device([bytes(NAK)]);
    assert_eq!(
        firehose_read(&mut dev, firehose_parser_configure_response).unwrap(),
        FirehoseStatus::Nak
    );
    drop(dev);
    assert!(trace.lock().unwrap().writes.is_empty());
    let config = String::from_utf8(CONFIG.to_vec()).unwrap();
    for xml in [
        config.replace("value=\"ACK\"", "value=\"bad\""),
        config.replace("InBytes=\"8\"", "InBytes=\"0\""),
        config.replace("InBytes=\"8\"", "InBytes=\"3\""),
        config.replace("InBytes=\"8\"", "InBytes=\"32\""),
        config.replace("InBytes=\"4096\"", "InBytes=\"0\""),
        config.replace("InBytes=\"8\"", "InBytes=\"bad\""),
        config.replace("MinVersionSupported=\"1\"", "MinVersionSupported=\"2\""),
    ] {
        let (mut dev, trace) = device([bytes(xml.as_bytes())]);
        assert!(firehose_read(&mut dev, firehose_parser_configure_response).is_err());
        assert_eq!(dev.fh_cfg.send_buffer_size, 8);
        assert_eq!(dev.fh_cfg.xml_buf_size, 4096);
        drop(dev);
        assert!(trace.lock().unwrap().writes.is_empty());
    }
}

#[test]
fn invalid_configure_sizes_fail_before_send() {
    let (mut dev, trace) = device([]);
    dev.fh_cfg.storage_sector_size = 0;
    assert!(firehose_configure(&mut dev, false).is_err());
    assert!(trace.lock().unwrap().writes.is_empty());
}

#[test]
fn patch_and_bootable_expose_rejected_and_unknown_confirmation() {
    for patch in [true, false] {
        for (response, expected) in [
            (bytes(NAK), MutationConfirmation::Rejected),
            (
                Input::Error(ErrorKind::TimedOut),
                MutationConfirmation::Unknown,
            ),
        ] {
            let (mut dev, trace) = device([response]);
            let error = if patch {
                firehose_patch(&mut dev, 0, 0, 0, 4, "NUM_DISK_SECTORS-1.", "CRC32(0,1)")
                    .unwrap_err()
            } else {
                firehose_set_bootable(&mut dev, 0).unwrap_err()
            };
            assert_eq!(error.confirmation, expected);
            drop(dev);
            assert_eq!(trace.lock().unwrap().writes.len(), 1);
            assert!(!String::from_utf8_lossy(&trace.lock().unwrap().accepted).contains("<power"));
        }
    }
}

#[test]
fn explicit_reset_sends_exactly_once_and_requires_ack() {
    for (response, expected) in [
        (bytes(ACK), None),
        (bytes(NAK), Some(MutationConfirmation::Rejected)),
        (
            Input::Error(ErrorKind::TimedOut),
            Some(MutationConfirmation::Unknown),
        ),
    ] {
        let (mut dev, trace) = device([response]);
        let result = firehose_reset(&mut dev, &FirehoseResetMode::Reset, 0);
        assert_eq!(result.err().map(|e| e.confirmation), expected);
        drop(dev);
        assert_eq!(trace.lock().unwrap().writes.len(), 1);
        assert_eq!(
            String::from_utf8_lossy(&trace.lock().unwrap().accepted)
                .matches("<power ")
                .count(),
            1
        );
    }
}

#[test]
fn hinted_program_nak_is_still_rejected() {
    for attributes in [
        "AttemptRestart=\"1\"",
        "AttemptRetry=\"1\"",
        "rawmode=\"true\"",
    ] {
        for initial in [true, false] {
            let xml = format!("<data><response value=\"NAK\" {attributes}/></data>");
            let input = if initial {
                vec![bytes(xml.as_bytes())]
            } else {
                vec![bytes(ACK), bytes(xml.as_bytes())]
            };
            let (mut dev, trace) = device(input);
            assert_eq!(
                program(&mut dev).unwrap_err().confirmation,
                MutationConfirmation::Rejected
            );
            drop(dev);
            one_program_no_followup(&trace);
            assert_eq!(
                trace.lock().unwrap().writes.len(),
                if initial { 1 } else { 2 }
            );
        }
    }
}

#[test]
fn quiet_output_probe() {
    if std::env::var_os("QDL_TEST_QUIET_CHILD").is_none() {
        return;
    }
    use std::io::Write;
    std::io::stdout().write_all(b"QDL_QUIET_BEGIN\n").unwrap();
    let (mut dev, _) = device([
        bytes(CONFIG),
        bytes(LOG),
        bytes(ACK),
        bytes(ACK),
        bytes(ACK),
        bytes(b"abcdefgh"),
        bytes(ACK),
        bytes(LOG),
        bytes(ACK),
    ]);
    dev.fh_cfg.verbose_firehose = true;
    dev.fh_cfg.skip_firehose_log = false;
    firehose_configure(&mut dev, false).unwrap();
    firehose_read(&mut dev, firehose_parser_configure_response).unwrap();
    program(&mut dev).unwrap();
    firehose_read_storage(&mut dev, &mut Vec::new(), 2, 0, 0, 0).unwrap();
    firehose_get_storage_info(&mut dev, 0).unwrap();
    let done: Vec<_> = [6u32, 12, 1]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    let (mut dev, _) = device([bytes(&done)]);
    qdl::sahara::sahara_run(
        &mut dev,
        qdl::sahara::SaharaMode::WaitingForImage,
        None,
        &mut [Some(vec![1])],
        vec![],
        true,
    )
    .unwrap();
    std::io::stdout().write_all(b"QDL_QUIET_END\n").unwrap();
}

#[test]
fn quiet_loader_and_storage_paths_emit_no_output() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "quiet_output_probe", "--nocapture"])
        .env("QDL_TEST_QUIET_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let between = stdout
        .split_once("QDL_QUIET_BEGIN\n")
        .unwrap()
        .1
        .split_once("QDL_QUIET_END\n")
        .unwrap()
        .0;
    assert!(between.is_empty(), "unexpected library stdout: {between:?}");
    assert!(
        output.stderr.is_empty(),
        "unexpected library stderr: {:?}",
        output.stderr
    );
}
