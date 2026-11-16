// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
// GPT loading adapted from qdlrs cli/src/util.rs at 7e0a31a5be2ab5d01b27bd743ee67a445ad0b84d.
// Bounds checks and transfer helpers: Copyright (c) 2026 LanceaKing.
use crate::types::{MutationFailure, QdlChan};
use anyhow::{Context, Result, bail};
use gptman::{GPT, GPTHeader};
use std::io::{self, Cursor, Read, Write};

pub struct Table {
    pub gpt: GPT,
    pub primary: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct Partition {
    pub lun: u8,
    pub name: String,
    pub start_sector: u64,
    pub sectors: u64,
    pub sector_size: usize,
}
impl Partition {
    pub fn bytes(&self) -> Result<u64> {
        self.sectors
            .checked_mul(self.sector_size as u64)
            .context("partition size overflow")
    }
    pub fn start_byte(&self) -> Result<u64> {
        self.start_sector
            .checked_mul(self.sector_size as u64)
            .context("partition offset overflow")
    }
    fn check(&self, channel: &impl QdlChan) -> Result<()> {
        if self.sector_size == 0
            || self.sector_size != channel.fh_config().storage_sector_size
            || self.sectors == 0
        {
            bail!("invalid partition geometry");
        }
        self.start_sector
            .checked_add(self.sectors)
            .and_then(|end| end.checked_mul(self.sector_size as u64))
            .context("partition end overflow")?;
        self.bytes()?;
        self.start_byte()?;
        Ok(())
    }
}
/// Bounds the primary metadata allocation before trusting any device-controlled LBA.
pub fn read_gpt(channel: &mut impl QdlChan, slot: u8, lun: u8) -> Result<Table> {
    read_gpt_with(channel, |channel, out, sectors, start| {
        crate::firehose_read_storage(channel, out, sectors, slot, lun, start)
    })
}

/// Read and validate primary GPT metadata through an observable transport.
/// The callback must append exactly the requested sectors and check completion.
/// Requests are bounded to 16 MiB; callback implementations must preserve this bound.
/// Callers may capture partial data and report each read without duplicating validation.
pub fn read_gpt_with<T: QdlChan>(
    channel: &mut T,
    mut read: impl FnMut(&mut T, &mut Vec<u8>, usize, u64) -> Result<()>,
) -> Result<Table> {
    let sector = channel.fh_config().storage_sector_size as u64;
    if !matches!(sector, 512 | 4096) {
        bail!("unsupported GPT sector size");
    }
    let mut probe = Vec::new();
    read(channel, &mut probe, 1, 1)?;
    anyhow::ensure!(probe.len() == sector as usize, "GPT probe length mismatch");
    let h = GPTHeader::read_from(&mut Cursor::new(&probe))?;
    let size = validate_header(&h, sector)?;
    let mut primary = Vec::with_capacity(size as usize);
    read(channel, &mut primary, usize::try_from(size / sector)?, 0)?;
    anyhow::ensure!(
        primary.len() == size as usize,
        "GPT metadata length mismatch"
    );
    // Validate the second header as well: the device may return different metadata.
    let h2 = GPTHeader::read_from(&mut Cursor::new(&primary[sector as usize..]))?;
    if validate_header(&h2, sector)? != size || h != h2 {
        bail!("GPT changed between reads");
    }
    // gptman 1.x computes alignment before validating partition bounds.
    // Reject invalid entries first, including all-used-starts-at-zero inputs.
    let mut entries = Cursor::new(&primary[h.partition_entry_lba as usize * sector as usize..]);
    let mut ranges = Vec::new();
    for _ in 0..h.number_of_partition_entries {
        let p = gptman::GPTPartitionEntry::read_from(&mut entries)?;
        if p.is_unused() {
            continue;
        }
        if p.starting_lba < h.first_usable_lba
            || p.ending_lba > h.last_usable_lba
            || p.ending_lba < p.starting_lba
        {
            bail!("partition outside GPT usable range");
        }
        p.ending_lba
            .checked_add(1)
            .and_then(|v| v.checked_mul(sector))
            .context("partition byte range overflow")?;
        ranges.push((p.starting_lba, p.ending_lba));
    }
    ranges.sort_unstable();
    if ranges.windows(2).any(|w| w[0].1 >= w[1].0) {
        bail!("overlapping GPT partitions");
    }
    let gpt = GPT::read_from(&mut Cursor::new(&primary), sector)?;
    Ok(Table { gpt, primary })
}
fn validate_header(h: &GPTHeader, sector: u64) -> Result<u64> {
    let size = h
        .first_usable_lba
        .checked_mul(sector)
        .context("GPT size overflow")?;
    if h.primary_lba != 1
        || h.partition_entry_lba < 2
        || h.size_of_partition_entry != 128
        || h.number_of_partition_entries == 0
        || size > 16 * 1024 * 1024
        || h.first_usable_lba > h.last_usable_lba
        || h.last_usable_lba >= h.backup_lba
    {
        bail!("unsupported or invalid GPT geometry");
    }
    let entries_end = h
        .partition_entry_lba
        .checked_mul(sector)
        .and_then(|v| v.checked_add(u64::from(h.number_of_partition_entries) * 128))
        .context("GPT entries overflow")?;
    if entries_end > size {
        bail!("GPT entries exceed metadata range");
    }
    h.backup_lba
        .checked_add(1)
        .and_then(|v| v.checked_mul(sector))
        .context("disk byte size overflow")?;
    Ok(size)
}
pub fn find_partition(table: &Table, lun: u8, name: &str) -> Result<Partition> {
    let mut matches = table
        .gpt
        .iter()
        .filter(|(_, p)| p.is_used() && p.partition_name.as_str() == name);
    let (_, p) = matches.next().context("partition not found")?;
    if matches.next().is_some() {
        bail!("partition name is ambiguous within LUN");
    }
    Ok(Partition {
        lun,
        name: name.into(),
        start_sector: p.starting_lba,
        sectors: p.size()?,
        sector_size: table.gpt.sector_size as usize,
    })
}
/// Read a partition through the standard Firehose transport.
pub fn read_partition(
    channel: &mut impl QdlChan,
    p: &Partition,
    out: &mut impl Write,
    slot: u8,
) -> Result<()> {
    read_partition_with(channel, p, out, |ch, out, sectors, lun, start| {
        crate::firehose_read_storage(ch, out, sectors, slot, lun, start)
    })
}

/// Validate the partition before invoking a caller-supplied read transport.
/// The callback must transfer exactly the requested sectors and check completion.
pub fn read_partition_with<T: QdlChan, W: Write>(
    channel: &mut T,
    p: &Partition,
    out: &mut W,
    read: impl FnOnce(&mut T, &mut W, usize, u8, u64) -> Result<()>,
) -> Result<()> {
    p.check(channel)?;
    read(
        channel,
        out,
        usize::try_from(p.sectors)?,
        p.lun,
        p.start_sector,
    )
}
/// Write a bounded image and pad only its final sector.
/// Callers own readback verification and session stop policy.
/// source_bytes is the expected input length; an early EOF is never replaced with padding.
pub fn program_partition(
    channel: &mut impl QdlChan,
    p: &Partition,
    source: &mut impl Read,
    source_bytes: u64,
    slot: u8,
) -> Result<(), MutationFailure> {
    let prepare = || -> Result<(usize, u64)> {
        p.check(channel)?;
        if source_bytes == 0 {
            bail!("empty image");
        }
        let sector = p.sector_size as u64;
        let sectors = source_bytes
            .checked_add(sector - 1)
            .context("image rounding overflow")?
            / sector;
        if sectors > p.sectors {
            bail!("image exceeds target partition");
        }
        Ok((usize::try_from(sectors)?, sectors * sector - source_bytes))
    };
    let (sectors, padding) = prepare().map_err(MutationFailure::not_sent)?;
    let mut data = ExactSource {
        source,
        remaining: source_bytes,
    }
    .chain(io::repeat(0).take(padding));
    crate::firehose_program_storage(
        channel,
        &mut data,
        &p.name,
        sectors,
        slot,
        p.lun,
        &p.start_sector.to_string(),
    )
}
struct ExactSource<'a, R> {
    source: &'a mut R,
    remaining: u64,
}
impl<R: Read> Read for ExactSource<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || out.is_empty() {
            return Ok(0);
        }
        let limit = self.remaining.min(out.len() as u64) as usize;
        let n = self.source.read(&mut out[..limit])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "image truncated before declared length",
            ));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

pub fn partitions(table: &Table, lun: u8) -> Result<Vec<Partition>> {
    table
        .gpt
        .iter()
        .filter(|(_, p)| p.is_used())
        .map(|(_, p)| {
            Ok(Partition {
                lun,
                name: p.partition_name.as_str().into(),
                start_sector: p.starting_lba,
                sectors: p.size()?,
                sector_size: table.gpt.sector_size as usize,
            })
        })
        .collect()
}
