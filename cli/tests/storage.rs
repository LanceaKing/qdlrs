// SPDX-License-Identifier: BSD-3-Clause
#[path = "../../qdl/tests/support/mod.rs"]
mod support;
#[allow(dead_code)]
#[path = "../src/util.rs"]
mod util;
use support::*;

#[test]
fn cli_rejects_ambiguous_partition_before_payload_read() {
    let disk = disk(512, &["boot", "boot"]);
    let (mut dev, trace) = storage_device(gpt_script(&disk, 512), 512);
    assert!(util::read_storage_logical_partition(&mut dev, &mut Vec::new(), "boot", 3, 2).is_err());
    assert_eq!(trace.lock().unwrap().writes.len(), 2);
}

#[test]
fn cli_reads_through_shared_partition_api() {
    let disk = disk(4096, &["boot"]);
    let mut script = gpt_script(&disk, 4096);
    script.extend([bytes(ACK), bytes(&vec![5; 8192]), bytes(ACK)]);
    let (mut dev, trace) = storage_device(script, 4096);
    let mut out = Vec::new();
    util::read_storage_logical_partition(&mut dev, &mut out, "boot", 3, 2).unwrap();
    assert_eq!(out, vec![5; 8192]);
    let record = trace.lock().unwrap();
    assert_eq!(record.writes.len(), 3);
    let xml = String::from_utf8_lossy(&record.writes[2]);
    assert!(xml.contains("slot=\"3\""));
    assert!(xml.contains("physical_partition_number=\"2\""));
    assert!(xml.contains("start_sector=\"64\""));
}
