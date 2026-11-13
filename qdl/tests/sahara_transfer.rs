// SPDX-License-Identifier: BSD-3-Clause
mod support;

use qdl::{
    sahara::{SaharaMode, sahara_run, sahara_send_img_to_device},
    types::QdlDevice,
};
use std::io::ErrorKind;
use support::*;

fn packet(command: u32, words: &[u32]) -> Vec<u8> {
    [command, 8 + words.len() as u32 * 4]
        .into_iter()
        .chain(words.iter().copied())
        .flat_map(u32::to_le_bytes)
        .collect()
}

fn run(dev: &mut QdlDevice<ScriptedIo>, images: &mut [Option<Vec<u8>>]) -> anyhow::Result<Vec<u8>> {
    sahara_run(
        dev,
        SaharaMode::WaitingForImage,
        None,
        images,
        vec![],
        false,
    )
}

#[test]
fn image_range_overflow_out_of_bounds_and_empty_requests_do_not_write() {
    for (offset, len) in [(u64::MAX, 1), (1, u64::MAX), (3, 2), (5, 0), (0, 0)] {
        let (mut dev, trace) = device([]);
        assert!(
            sahara_send_img_to_device(&mut dev, &mut [Some(b"abcd".to_vec())], 13, offset, len)
                .is_err()
        );
        drop(dev);
        assert!(trace.lock().unwrap().writes.is_empty());
    }
}

#[test]
fn missing_image_id_is_an_error_without_writes() {
    for index in [1, u64::MAX] {
        let (mut dev, trace) = device([]);
        assert!(
            sahara_send_img_to_device(&mut dev, &mut [Some(vec![1]), None], index, 0, 1).is_err()
        );
        assert!(trace.lock().unwrap().writes.is_empty());
    }
}

#[test]
fn requested_range_and_single_image_id_fallback_are_preserved() {
    let (mut dev, trace) = device([]);
    dev.rw.output.extend([Output::Accept(1), Output::Accept(1)]);
    let n = sahara_send_img_to_device(&mut dev, &mut [Some(b"abcdef".to_vec())], 13, 1, 3).unwrap();
    assert_eq!(n, 3);
    assert_eq!(trace.lock().unwrap().accepted, b"bcd");
    assert_eq!(trace.lock().unwrap().flushes, 1);
}

#[test]
fn image_write_zero_error_and_flush_error_propagate() {
    for case in 0..3 {
        let (mut dev, trace) = device([]);
        match case {
            0 => dev.rw.output.push_back(Output::Accept(0)),
            1 => dev
                .rw
                .output
                .push_back(Output::Error(ErrorKind::BrokenPipe)),
            _ => dev.rw.flush_error = Some(ErrorKind::TimedOut),
        }
        let error = sahara_send_img_to_device(&mut dev, &mut [Some(b"abcd".to_vec())], 13, 0, 4)
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            ErrorKind::WriteZero | ErrorKind::BrokenPipe | ErrorKind::TimedOut
        ));
        drop(dev);
        assert_no_reset(&trace);
        assert_eq!(trace.lock().unwrap().writes.len(), 1);
    }
}

#[test]
fn fragmented_handshake_and_coalesced_image_requests_complete() {
    let hello = packet(1, &[2, 1, 4096, 0, 0, 0, 0, 0, 0, 0]);
    let request = packet(3, &[13, 1, 3]);
    let eoi = packet(4, &[13, 0]);
    let done = packet(6, &[1]);
    let inbound = [hello, request, eoi, done].concat();
    for chunk_size in [1, 3, 7, 4096] {
        let (mut dev, trace) = device(inbound.chunks(chunk_size).map(bytes));
        run(&mut dev, &mut [Some(b"abcdef".to_vec())]).unwrap();
        drop(dev);
        let record = trace.lock().unwrap();
        assert_eq!(record.writes.len(), 3);
        assert_eq!(&record.writes[0][..8], &packet(2, &[0; 10])[..8]);
        assert_eq!(&record.writes[1], b"bcd");
        assert_eq!(&record.writes[2], &packet(5, &[]));
    }
}

#[test]
fn read64_request_overflow_is_rejected_in_the_actual_state_machine() {
    let (mut dev, trace) = device([bytes(&packet(0x12, &[13, 0, u32::MAX, u32::MAX, 1, 0]))]);
    assert!(run(&mut dev, &mut [Some(vec![0; 8])]).is_err());
    drop(dev);
    assert!(trace.lock().unwrap().writes.is_empty());
}

#[test]
fn read32_and_read64_out_of_bounds_are_errors() {
    for request in [packet(3, &[13, 6, 3]), packet(0x12, &[13, 0, 6, 0, 3, 0])] {
        let (mut dev, trace) = device([bytes(&request)]);
        assert!(run(&mut dev, &mut [Some(vec![0; 8])]).is_err());
        assert!(trace.lock().unwrap().writes.is_empty());
    }
}

#[test]
fn truncated_packets_fail_without_recovery_commands() {
    let request = packet(3, &[13, 0, 4]);
    for len in 0..request.len() {
        let mut input = Vec::new();
        if len > 0 {
            input.push(bytes(&request[..len]));
        }
        input.push(Input::Eof);
        let (mut dev, trace) = device(input);
        let error = run(&mut dev, &mut [Some(vec![0; 8])]).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            ErrorKind::UnexpectedEof
        );
        drop(dev);
        assert!(trace.lock().unwrap().writes.is_empty());
    }
}

#[test]
fn oversized_fixed_packet_bodies_return_errors_instead_of_panicking() {
    for (command, words) in [
        (1, 10),
        (3, 3),
        (4, 2),
        (6, 1),
        (8, 0),
        (0xb, 0),
        (0xe, 2),
        (0x10, 4),
        (0x12, 6),
    ] {
        let (mut dev, trace) = device([bytes(&packet(command, &vec![0; words + 1]))]);
        assert!(run(&mut dev, &mut [Some(vec![0; 8])]).is_err());
        assert!(trace.lock().unwrap().writes.is_empty());
    }
}

#[test]
fn malformed_header_and_unknown_command_are_errors() {
    for header in [[3u32, 7], [3, 4097], [u32::MAX, 8]] {
        let input: Vec<_> = header.into_iter().flat_map(u32::to_le_bytes).collect();
        let (mut dev, trace) = device([bytes(&input)]);
        assert!(run(&mut dev, &mut [Some(vec![0; 8])]).is_err());
        assert!(trace.lock().unwrap().writes.is_empty());
    }
}

#[test]
fn failed_end_of_image_does_not_send_done_or_reset() {
    let (mut dev, trace) = device([bytes(&packet(4, &[13, 1]))]);
    assert!(run(&mut dev, &mut [Some(vec![0; 8])]).is_err());
    drop(dev);
    assert!(trace.lock().unwrap().writes.is_empty());
}

#[test]
fn invalid_completion_status_never_reports_loader_success() {
    let (mut dev, trace) = device([bytes(&packet(6, &[2]))]);
    assert!(run(&mut dev, &mut [Some(vec![0; 8])]).is_err());
    assert!(trace.lock().unwrap().writes.is_empty());
}
