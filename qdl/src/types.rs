// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
use std::{
    fmt::Display,
    io::{BufRead, ErrorKind, Read, Write},
    str::FromStr,
};

use anyhow::{Error, bail};
/// Confirmation available when a modifying request fails.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationConfirmation {
    NotSent,
    Rejected,
    Unknown,
}

/// A failed modification, retaining the original error and wire confirmation.
#[derive(Debug)]
pub struct MutationFailure {
    pub confirmation: MutationConfirmation,
    pub cause: anyhow::Error,
}

impl MutationFailure {
    pub fn not_sent(cause: impl Into<anyhow::Error>) -> Self {
        Self {
            confirmation: MutationConfirmation::NotSent,
            cause: cause.into(),
        }
    }

    pub fn rejected(cause: impl Into<anyhow::Error>) -> Self {
        Self {
            confirmation: MutationConfirmation::Rejected,
            cause: cause.into(),
        }
    }

    pub fn unknown(cause: impl Into<anyhow::Error>) -> Self {
        Self {
            confirmation: MutationConfirmation::Unknown,
            cause: cause.into(),
        }
    }
}

impl Display for MutationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.confirmation, self.cause)
    }
}

impl std::error::Error for MutationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

/// A complete initial read NAK with no retry/restart hint or raw-mode indication.
/// Other read failures require stopping the session.
#[derive(Debug)]
pub struct ReadRejected;

impl Display for ReadRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Read request was NAKed before raw data")
    }
}

impl std::error::Error for ReadRejected {}

/// Common respones indicating success/failure respectively
#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(u32)]
pub enum FirehoseStatus {
    Ack = 0,
    Nak = 1,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QdlBackend {
    Serial,
    Usb,
}

impl FromStr for QdlBackend {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "serial" => Ok(QdlBackend::Serial),
            "usb" => Ok(QdlBackend::Usb),
            _ => bail!("Unknown backend"),
        }
    }
}

impl Default for QdlBackend {
    fn default() -> Self {
        match cfg!(target_os = "windows") {
            true => QdlBackend::Serial,
            false => QdlBackend::Usb,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FirehoseConfiguration {
    // send/recv are from Host PoV
    pub send_buffer_size: usize,
    pub recv_buffer_size: usize,
    pub xml_buf_size: usize,

    pub storage_sector_size: usize,
    pub storage_type: FirehoseStorageType,

    pub bypass_storage: bool,
    pub hash_packets: bool,
    pub read_back_verify: bool,

    pub backend: QdlBackend,
    pub skip_firehose_log: bool,
    pub verbose_firehose: bool,
    /// Suppress library logs and progress on the loader/storage paths.
    pub quiet: bool,
}

impl Default for FirehoseConfiguration {
    fn default() -> Self {
        Self {
            send_buffer_size: 1024 * 1024,
            recv_buffer_size: 4096,
            xml_buf_size: 4096,
            storage_sector_size: 512,
            storage_type: FirehoseStorageType::Emmc,
            bypass_storage: false,
            hash_packets: false,
            read_back_verify: false,
            backend: QdlBackend::default(),
            skip_firehose_log: true,
            verbose_firehose: false,
            quiet: false,
        }
    }
}

impl FirehoseConfiguration {
    /// Bound allocations and reject sizes that cannot make sector progress.
    pub fn validate(&self) -> anyhow::Result<()> {
        const MAX_BUFFER: usize = 16 * 1024 * 1024;
        if self.storage_sector_size == 0 || self.storage_sector_size > MAX_BUFFER {
            bail!("Invalid storage sector size {}", self.storage_sector_size);
        }
        for size in [self.send_buffer_size, self.recv_buffer_size] {
            if size == 0 || size > MAX_BUFFER || !size.is_multiple_of(self.storage_sector_size) {
                bail!("Invalid sector-aligned payload size {size}");
            }
        }
        if self.xml_buf_size == 0 || self.xml_buf_size > MAX_BUFFER {
            bail!("Invalid XML buffer size {}", self.xml_buf_size);
        }
        Ok(())
    }
}
pub trait QdlChan: BufRead + Write {
    fn fh_config(&self) -> &FirehoseConfiguration;
    fn mut_fh_config(&mut self) -> &mut FirehoseConfiguration;
}

pub trait QdlReadWrite: BufRead + Write + Send + Sync {}
impl<T> QdlReadWrite for &mut T where T: QdlReadWrite + ?Sized {}

pub struct QdlDevice<T>
where
    T: QdlReadWrite + ?Sized,
{
    pub rw: Box<T>,
    pub fh_cfg: FirehoseConfiguration,
    /// Retained for source compatibility; ignored. Drop never sends commands.
    pub reset_on_drop: bool,
}

impl<T> Read for QdlDevice<T>
where
    T: QdlReadWrite + ?Sized,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.rw.read(buf)
    }
}

impl<T> Write for QdlDevice<T>
where
    T: QdlReadWrite + ?Sized,
{
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.rw.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.rw.flush()
    }
}

impl<T> std::io::BufRead for QdlDevice<T>
where
    T: QdlReadWrite + ?Sized,
{
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.rw.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        self.rw.consume(amt);
    }
}

impl<T> QdlChan for QdlDevice<T>
where
    T: QdlReadWrite + ?Sized,
{
    fn fh_config(&self) -> &FirehoseConfiguration {
        &self.fh_cfg
    }

    fn mut_fh_config(&mut self) -> &mut FirehoseConfiguration {
        &mut self.fh_cfg
    }
}

/// Supported storage media types
#[derive(Clone, Copy, Debug)]
pub enum FirehoseStorageType {
    Emmc,
    Ufs,
    Nand,
    Nvme,
    Spinor,
}

impl FromStr for FirehoseStorageType {
    type Err = Error;

    fn from_str(input: &str) -> Result<FirehoseStorageType, Self::Err> {
        match input {
            "emmc" => Ok(FirehoseStorageType::Emmc),
            "ufs" => Ok(FirehoseStorageType::Ufs),
            "nand" => Ok(FirehoseStorageType::Nand),
            "nvme" => Ok(FirehoseStorageType::Nvme),
            "spinor" => Ok(FirehoseStorageType::Spinor),
            _ => bail!("Unknown storage type"),
        }
    }
}

impl Display for FirehoseStorageType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FirehoseStorageType::Emmc => write!(f, "emmc"),
            FirehoseStorageType::Ufs => write!(f, "ufs"),
            FirehoseStorageType::Nand => write!(f, "nand"),
            FirehoseStorageType::Nvme => write!(f, "nvme"),
            FirehoseStorageType::Spinor => write!(f, "spinor"),
        }
    }
}

/// List of supported reboot modes, supplied to the \<reset\> command
pub enum FirehoseResetMode {
    ResetToEdl,
    Reset,
    Off,
}

impl FromStr for FirehoseResetMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "edl" => Ok(FirehoseResetMode::ResetToEdl),
            "system" => Ok(FirehoseResetMode::Reset),
            "off" => Ok(FirehoseResetMode::Off),
            _ => Err(std::io::Error::from(ErrorKind::InvalidInput).into()),
        }
    }
}

impl Display for FirehoseResetMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FirehoseResetMode::ResetToEdl => write!(f, "edl"),
            FirehoseResetMode::Reset => write!(f, "system"),
            FirehoseResetMode::Off => write!(f, "off"),
        }
    }
}
