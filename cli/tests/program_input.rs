// SPDX-License-Identifier: BSD-3-Clause
#[path = "../src/input.rs"]
mod input;
#[path = "../src/programfile.rs"]
mod programfile;
#[path = "../../qdl/tests/support/mod.rs"]
mod support;

use input::{ProgramInput, zero_input};
use qdl::{firehose_program_storage, types::MutationConfirmation};
use std::{
    fs::{self, File},
    io::{ErrorKind, Read},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use support::*;

struct Fixture(PathBuf);

impl Fixture {
    fn new(data: &[u8]) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "qdlrs-program-input-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("image.bin"), data).unwrap();
        Self(dir)
    }

    fn file(&self) -> File {
        File::open(self.0.join("image.bin")).unwrap()
    }

    fn xml(&self, sectors: usize, offset: &str) -> xmltree::Element {
        xmltree::Element::parse(
            format!(
                "<data><program SECTOR_SIZE_IN_BYTES=\"4\" num_partition_sectors=\"{sectors}\" \
                 physical_partition_number=\"0\" start_sector=\"0\" label=\"p\" \
                 filename=\"image.bin\" file_sector_offset=\"{offset}\"/></data>"
            )
            .as_bytes(),
        )
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn payload(trace: &Trace) -> Vec<u8> {
    let record = trace.lock().unwrap();
    assert!(String::from_utf8_lossy(&record.writes[0]).contains("<program "));
    record.writes[1..].concat()
}

#[test]
fn raw_image_input_pads_only_the_final_sector() {
    for (source, expected) in [
        (&b"abcdefgh"[..], &b"abcdefgh"[..]),
        (&b"a"[..], &b"a\0\0\0"[..]),
        (&b"abcdefghi"[..], &b"abcdefghi\0\0\0"[..]),
    ] {
        let fixture = Fixture::new(source);
        let mut input = ProgramInput::from_file(fixture.file(), 4, 0, None).unwrap();
        let (mut dev, trace) = device([bytes(ACK), bytes(ACK)]);
        let sectors = input.num_sectors();
        assert_eq!(sectors * 4, expected.len());
        firehose_program_storage(&mut dev, &mut input, "p", sectors, 0, 0, "0").unwrap();
        assert_eq!(payload(&trace), expected);
        assert_eq!(input.read(&mut [0; 1]).unwrap(), 0);
        drop(dev);
        assert_no_reset(&trace);
    }
}

#[test]
fn erase_input_is_a_finite_full_length_zero_stream() {
    let mut input = zero_input(5, 4).unwrap();
    let (mut dev, trace) = device([bytes(ACK), bytes(ACK)]);
    firehose_program_storage(&mut dev, &mut input, "p", 5, 0, 0, "0").unwrap();
    assert_eq!(payload(&trace), [0; 20]);
    assert_eq!(
        trace.lock().unwrap().writes[1..]
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
        [8, 8, 4]
    );
    assert_eq!(input.read(&mut [0; 1]).unwrap(), 0);
    drop(dev);
    assert_no_reset(&trace);
}

#[test]
fn rawprogram_caller_pads_tail_across_multiple_payloads() {
    let fixture = Fixture::new(b"abcdefghi");
    let (mut dev, trace) = device([bytes(ACK), bytes(ACK)]);
    assert_eq!(
        programfile::parse_program_xml(
            &mut dev,
            &fixture.xml(3, "0"),
            &fixture.0,
            &fixture.0,
            false,
            false
        )
        .unwrap(),
        None
    );
    assert_eq!(payload(&trace), b"abcdefghi\0\0\0");
    assert_eq!(
        trace.lock().unwrap().writes[1..]
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
        [8, 4]
    );
    drop(dev);
    assert_no_reset(&trace);
}

#[test]
fn rawprogram_caller_respects_selected_file_range() {
    let fixture = Fixture::new(b"abcdefghijklmnopq");
    for (sectors, offset, expected) in [
        (2, "1", &b"efghijkl"[..]),
        (3, "2", &b"ijklmnopq\0\0\0"[..]),
    ] {
        let (mut dev, trace) = device([bytes(ACK), bytes(ACK)]);
        programfile::parse_program_xml(
            &mut dev,
            &fixture.xml(sectors, offset),
            &fixture.0,
            &fixture.0,
            false,
            false,
        )
        .unwrap();
        assert_eq!(payload(&trace), expected);
    }
}

#[test]
fn rawprogram_invalid_ranges_fail_before_program_command() {
    for (source, sectors, offset) in [
        (&b""[..], 1, "0"),
        (&b"abcde"[..], 3, "0"),
        (&b"abcd"[..], 1, "1"),
        (&b"abcd"[..], 1, "2"),
        (&b"abcd"[..], 1, "not-a-number"),
        (&b"abcd"[..], 1, "18446744073709551615"),
        (&b"abcd"[..], usize::MAX, "0"),
    ] {
        let fixture = Fixture::new(source);
        let (mut dev, trace) = device([]);
        assert!(
            programfile::parse_program_xml(
                &mut dev,
                &fixture.xml(sectors, offset),
                &fixture.0,
                &fixture.0,
                false,
                false
            )
            .is_err()
        );
        drop(dev);
        assert!(trace.lock().unwrap().writes.is_empty());
    }
}

#[test]
fn file_truncation_is_not_converted_to_padding() {
    let fixture = Fixture::new(b"abcdefghi");
    let mut input = ProgramInput::from_file(fixture.file(), 4, 0, None).unwrap();
    File::options()
        .write(true)
        .open(fixture.0.join("image.bin"))
        .unwrap()
        .set_len(8)
        .unwrap();
    let (mut dev, trace) = device([bytes(ACK)]);
    let error = firehose_program_storage(&mut dev, &mut input, "p", 3, 0, 0, "0").unwrap_err();
    assert_eq!(error.confirmation, MutationConfirmation::Unknown);
    assert_eq!(
        error.cause.downcast_ref::<std::io::Error>().unwrap().kind(),
        ErrorKind::UnexpectedEof
    );
    assert_eq!(payload(&trace), b"abcdefgh");
    assert_eq!(trace.lock().unwrap().writes.len(), 2);
    drop(dev);
    assert_no_reset(&trace);
}

#[test]
fn padded_rawprogram_still_requires_final_ack() {
    for final_response in [bytes(NAK), Input::Error(ErrorKind::TimedOut)] {
        let fixture = Fixture::new(b"a");
        let (mut dev, trace) = device([bytes(ACK), final_response]);
        assert!(
            programfile::parse_program_xml(
                &mut dev,
                &fixture.xml(1, "0"),
                &fixture.0,
                &fixture.0,
                false,
                false
            )
            .is_err()
        );
        assert_eq!(payload(&trace), b"a\0\0\0");
        drop(dev);
        assert_no_reset(&trace);
    }
}

#[test]
fn input_size_validation_rejects_zero_sector_and_overflow() {
    let fixture = Fixture::new(b"abcd");
    assert!(ProgramInput::from_file(fixture.file(), 0, 0, None).is_err());
    assert!(ProgramInput::from_file(fixture.file(), 4, 0, Some(usize::MAX)).is_err());
    assert!(zero_input(1, 0).is_err());
    assert!(zero_input(usize::MAX, 4).is_err());
}
