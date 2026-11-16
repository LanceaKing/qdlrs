// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) 2026 LanceaKing.
mod support;
use qdl::storage::{self, *};
use qdl::types::{MutationConfirmation, QdlChan};
use std::io::{self, Cursor, ErrorKind, Read};
use support::*;
// Exercise the public API with a non-default slot, also used by the fork CLI.
fn read_gpt(ch: &mut impl QdlChan, lun: u8) -> anyhow::Result<Table> {
    storage::read_gpt(ch, 3, lun)
}
fn program_partition(
    ch: &mut impl QdlChan,
    p: &Partition,
    source: &mut impl Read,
    bytes: u64,
) -> Result<(), qdl::types::MutationFailure> {
    storage::program_partition(ch, p, source, bytes, 3)
}
fn part() -> Partition {
    Partition {
        lun: 2,
        name: "boot".into(),
        start_sector: 64,
        sectors: 2,
        sector_size: 512,
    }
}
struct ShortSource(Cursor<Vec<u8>>);
impl Read for ShortSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = out.len().min(3);
        self.0.read(&mut out[..n])
    }
}
#[test]
fn duplicate_exact_names_are_not_selected() {
    let disk = disk(512, &["boot", "boot"]);
    let (mut dev, _) = storage_device(gpt_script(&disk, 512), 512);
    let table = read_gpt(&mut dev, 0).unwrap();
    assert!(find_partition(&table, 0, "boot").is_err());
    assert!(find_partition(&table, 0, "missing").is_err());
}
#[test]
fn invalid_header_is_rejected_before_second_read() {
    for kind in 0..5 {
        let disk = disk(512, &["boot"]);
        let mut header = gptman::GPTHeader::read_from(&mut Cursor::new(&disk[512..])).unwrap();
        match kind {
            0 => header.first_usable_lba = u64::MAX,
            1 => header.size_of_partition_entry = 256,
            2 => header.number_of_partition_entries = u32::MAX,
            3 => header.partition_entry_lba = u64::MAX,
            _ => header.last_usable_lba = header.backup_lba,
        }
        header.update_crc32_checksum();
        let mut encoded = bincode::serialize(&header).unwrap();
        encoded.resize(512, 0);
        let (mut dev, trace) = storage_device([bytes(ACK), bytes(&encoded), bytes(ACK)], 512);
        assert!(read_gpt(&mut dev, 0).is_err());
        assert_eq!(trace.lock().unwrap().writes.len(), 1);
    }
}
#[test]
fn corrupted_entries_and_out_of_range_partitions_are_rejected() {
    for kind in 0..4 {
        let mut disk = disk(512, &["boot", "system"]);
        if kind == 0 {
            disk[1024] ^= 1;
        } else {
            let mut cursor = Cursor::new(disk);
            let mut gpt = gptman::GPT::read_from(&mut cursor, 512).unwrap();
            if kind == 1 {
                gpt[1].ending_lba = 255;
            } else if kind == 2 {
                gpt[2].starting_lba = 65;
            } else {
                gpt[1].starting_lba = 0;
                gpt[2].starting_lba = 0;
            }
            let partitions = gpt.iter().map(|(_, p)| p.clone()).collect::<Vec<_>>();
            gpt.header
                .write_into(&mut cursor, 512, &partitions)
                .unwrap();
            disk = cursor.into_inner();
        }
        let (mut dev, _) = storage_device(gpt_script(&disk, 512), 512);
        assert!(read_gpt(&mut dev, 0).is_err());
    }
}
#[test]
fn program_pads_only_the_sector_tail_and_handles_short_reads_and_serial_writes() {
    let (mut dev, trace) = storage_device([bytes(ACK), bytes(ACK)], 512);
    dev.rw.output.extend([
        Output::Accept(usize::MAX),
        Output::Accept(3),
        Output::Accept(7),
    ]);
    program_partition(
        &mut dev,
        &part(),
        &mut ShortSource(Cursor::new(vec![0x61; 513])),
        513,
    )
    .unwrap();
    let wire = &trace.lock().unwrap().accepted;
    let data = &wire[wire.len() - 1024..];
    assert_eq!(&data[..513], &vec![0x61; 513]);
    assert!(data[513..].iter().all(|b| *b == 0));
}
#[test]
fn invalid_program_ranges_do_not_send_anything() {
    let (mut dev, trace) = storage_device([], 512);
    let mut invalid = part();
    invalid.start_sector = u64::MAX / 512;
    assert_eq!(
        program_partition(&mut dev, &invalid, &mut &b"a"[..], 1)
            .unwrap_err()
            .confirmation,
        MutationConfirmation::NotSent
    );
    assert!(trace.lock().unwrap().writes.is_empty());
    for len in [0, 1025, u64::MAX] {
        let (mut dev, trace) = storage_device([], 512);
        let error = program_partition(&mut dev, &part(), &mut &b"a"[..], len).unwrap_err();
        assert_eq!(error.confirmation, MutationConfirmation::NotSent);
        assert!(trace.lock().unwrap().writes.is_empty());
    }
}
#[test]
fn truncated_input_is_unknown_and_not_silently_padded() {
    let (mut dev, trace) = storage_device([bytes(ACK)], 512);
    let e = program_partition(&mut dev, &part(), &mut &b"ab"[..], 3).unwrap_err();
    assert_eq!(e.confirmation, MutationConfirmation::Unknown);
    assert_eq!(trace.lock().unwrap().writes.len(), 1);
    drop(dev);
    assert_no_reset(&trace);
}
#[test]
fn program_final_ack_is_required() {
    for (response, expected) in [
        (
            Input::Error(ErrorKind::TimedOut),
            MutationConfirmation::Unknown,
        ),
        (bytes(NAK), MutationConfirmation::Rejected),
    ] {
        let (mut dev, trace) = storage_device([bytes(ACK), response], 512);
        let e = program_partition(&mut dev, &part(), &mut &b"ab"[..], 2).unwrap_err();
        assert_eq!(e.confirmation, expected);
        drop(dev);
        assert_no_reset(&trace);
    }
}
#[test]
fn changed_gpt_header_is_not_trusted() {
    let disk = disk(512, &["boot"]);
    let mut changed = disk.clone();
    let mut h = gptman::GPTHeader::read_from(&mut Cursor::new(&disk[512..])).unwrap();
    h.disk_guid = [3; 16];
    h.update_crc32_checksum();
    changed[512..604].copy_from_slice(&bincode::serialize(&h).unwrap());
    let script = [
        bytes(ACK),
        bytes(&disk[512..1024]),
        bytes(ACK),
        bytes(ACK),
        bytes(&changed[..h.first_usable_lba as usize * 512]),
        bytes(ACK),
    ];
    let (mut dev, _) = storage_device(script, 512);
    assert!(read_gpt(&mut dev, 0).is_err());
}

#[test]
fn public_reads_preserve_slot_lun_and_sector_size() {
    for sector in [512, 4096] {
        let disk = disk(sector, &["boot"]);
        let mut script = gpt_script(&disk, sector);
        script.extend([bytes(ACK), bytes(&vec![7; 2 * sector]), bytes(ACK)]);
        let (mut dev, trace) = storage_device(script, sector);
        let table = read_gpt(&mut dev, 2).unwrap();
        assert_eq!(partitions(&table, 2).unwrap().len(), 1);
        let p = find_partition(&table, 2, "boot").unwrap();
        let mut out = Vec::new();
        storage::read_partition(&mut dev, &p, &mut out, 3).unwrap();
        assert_eq!(out, vec![7; 2 * sector]);
        for command in &trace.lock().unwrap().writes {
            let xml = String::from_utf8_lossy(command);
            assert!(xml.contains("slot=\"3\""));
            assert!(xml.contains("physical_partition_number=\"2\""));
        }
    }
}
#[test]
fn callback_short_success_is_rejected_without_panicking() {
    let (mut dev, _) = storage_device([], 512);
    assert!(read_gpt_with(&mut dev, |_, _, _, _| Ok(())).is_err());
    let disk = disk(512, &["boot"]);
    assert!(
        read_gpt_with(&mut dev, |_, out, _, start| {
            if start == 1 {
                out.extend_from_slice(&disk[512..1024]);
            }
            Ok(())
        })
        .is_err()
    );
}
#[test]
fn invalid_partition_does_not_invoke_transport() {
    let (mut dev, trace) = storage_device([], 512);
    let mut p = part();
    p.sectors = 0;
    assert!(
        storage::read_partition_with(&mut dev, &p, &mut Vec::new(), |_, _, _, _, _| panic!(
            "invalid partition reached transport"
        ))
        .is_err()
    );
    assert!(trace.lock().unwrap().writes.is_empty());
}
