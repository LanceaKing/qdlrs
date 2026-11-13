// SPDX-License-Identifier: BSD-3-Clause
use anyhow::{Context, Result, bail};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Take},
};

/// A known file range followed by only its permitted final-sector padding.
pub struct ProgramInput {
    source: Take<File>,
    padding_left: u64,
    num_sectors: usize,
}

fn byte_count(num_sectors: usize, sector_size: usize) -> Result<u64> {
    if sector_size == 0 {
        bail!("Sector size must be nonzero");
    }
    Ok(u64::try_from(
        num_sectors
            .checked_mul(sector_size)
            .context("Program byte count overflow")?,
    )?)
}

impl ProgramInput {
    pub fn from_file(
        mut file: File,
        sector_size: usize,
        offset: u64,
        requested_sectors: Option<usize>,
    ) -> Result<Self> {
        if sector_size == 0 {
            bail!("Sector size must be nonzero");
        }
        let available = file
            .metadata()?
            .len()
            .checked_sub(offset)
            .context("Program source offset exceeds file length")?;
        let num_sectors = match requested_sectors {
            Some(count) => count,
            None => usize::try_from(available.div_ceil(sector_size as u64))?,
        };
        let target_len = byte_count(num_sectors, sector_size)?;
        let source_len = target_len.min(available);
        let padding_left = target_len - source_len;
        if padding_left >= sector_size as u64 {
            bail!("Program source range exceeds file length beyond final-sector padding");
        }
        file.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            source: file.take(source_len),
            padding_left,
            num_sectors,
        })
    }

    pub fn num_sectors(&self) -> usize {
        self.num_sectors
    }
}

impl Read for ProgramInput {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.source.limit() != 0 {
            let n = self.source.read(out)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Program source was truncated",
                ));
            }
            return Ok(n);
        }
        let n = (out.len() as u64).min(self.padding_left) as usize;
        out[..n].fill(0);
        self.padding_left -= n as u64;
        Ok(n)
    }
}

/// Erase callers intentionally provide every requested byte as zero.
pub fn zero_input(num_sectors: usize, sector_size: usize) -> Result<Take<io::Repeat>> {
    Ok(io::repeat(0).take(byte_count(num_sectors, sector_size)?))
}
