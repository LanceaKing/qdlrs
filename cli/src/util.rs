// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
use anyhow::Result;
use owo_colors::OwoColorize;
use qdl::{storage, types::QdlChan};
use std::io::Write;

pub fn read_gpt_from_storage(channel: &mut impl QdlChan, slot: u8, lun: u8) -> Result<gptman::GPT> {
    Ok(storage::read_gpt(channel, slot, lun)?.gpt)
}
pub fn find_part(
    channel: &mut impl QdlChan,
    name: &str,
    slot: u8,
    lun: u8,
) -> Result<storage::Partition> {
    let table = storage::read_gpt(channel, slot, lun)?;
    storage::find_partition(&table, lun, name)
}

pub fn print_partition_table<T: QdlChan>(
    channel: &mut T,
    slot: u8,
    phys_part_idx: u8,
) -> Result<()> {
    let gpt = read_gpt_from_storage(channel, slot, phys_part_idx)?;

    println!(
        "GPT on physical partition {} of {}:",
        phys_part_idx.bright_yellow(),
        channel.fh_config().storage_type.to_string().bright_yellow()
    );

    for (idx, part) in gpt.iter() {
        let size = part.size();

        println!(
            "{}] {}: start_sector = {}, {} bytes ({} kiB)",
            idx,
            part.partition_name.as_str(),
            part.starting_lba,
            match size {
                Ok(s) => (s * gpt.sector_size).to_string(),
                Err(_) => "ERROR".to_string(),
            },
            match size {
                Ok(s) => (s * gpt.sector_size / 1024).to_string(),
                Err(_) => "ERROR".to_string(),
            }
        );
    }

    Ok(())
}

pub fn read_storage_logical_partition(
    channel: &mut impl QdlChan,
    out: &mut impl Write,
    name: &str,
    slot: u8,
    lun: u8,
) -> Result<()> {
    let part = find_part(channel, name, slot, lun)?;
    storage::read_partition(channel, &part, out, slot)
}
