// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
use anstream::println;
use anyhow::Context;
use anyhow::Result;
use indexmap::{Equivalent, IndexMap};
use owo_colors::OwoColorize;
use parsers::firehose_parser_ack_nak;
use serde::{Deserialize, Serialize};
use std::cmp::min;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::path::Path;
use std::str::{self, FromStr};
use types::FirehoseStatus;
use types::FirehoseStorageType;
use types::QdlBackend;
use types::QdlChan;
use types::QdlReadWrite;
use types::{FirehoseResetMode, MutationFailure, ReadRejected};

use anyhow::bail;
use pbr::{ProgressBar, Units};
use xmltree::{self, Element, XMLNode};

pub mod parsers;
pub mod sahara;
#[cfg(feature = "serial")]
pub mod serial;
pub mod types;
#[cfg(feature = "usb")]
pub mod usb;

pub const SAHARA_ID_EHOSTDL_IMG: usize = 13;

const CPIO_MAGIC: &[u8; 6] = b"070701";

#[repr(C)]
#[derive(Debug, Serialize, Deserialize)]
struct CpioNewcHeader {
    c_magic: [u8; 6],
    c_ino: [u8; 8],
    c_mode: [u8; 8],
    c_uid: [u8; 8],
    c_gid: [u8; 8],
    c_nlink: [u8; 8],
    c_mtime: [u8; 8],
    c_filesize: [u8; 8],
    c_devmajor: [u8; 8],
    c_devminor: [u8; 8],
    c_rdevmajor: [u8; 8],
    c_rdevminor: [u8; 8],
    c_namesize: [u8; 8],
    c_check: [u8; 8],
}

fn align_up_4(n: usize) -> usize {
    (n + 3) & !3
}

fn parse_ascii_hex_u32(field: &[u8]) -> Result<u32> {
    let s = std::str::from_utf8(field)?;
    if s.len() != 8 {
        bail!("invalid cpio header field size");
    }

    u32::from_str_radix(s, 16).map_err(|_| anyhow::anyhow!("invalid hex field \"{}\"", s))
}

fn decode_programmer_archive(blob: &[u8], images: &mut Vec<Option<Vec<u8>>>) -> Result<bool> {
    if blob.len() < size_of::<CpioNewcHeader>() || &blob[..6] != CPIO_MAGIC {
        return Ok(false);
    }

    let mut ptr = 0usize;

    loop {
        if ptr + size_of::<CpioNewcHeader>() > blob.len() {
            bail!("programmer archive is truncated");
        }

        let hdr =
            bincode::deserialize::<CpioNewcHeader>(&blob[ptr..ptr + size_of::<CpioNewcHeader>()])?;
        if !hdr.c_magic.equivalent(CPIO_MAGIC) {
            bail!("expected cpio header in programmer archive");
        }

        let filesize = parse_ascii_hex_u32(&hdr.c_filesize)? as usize;
        let namesize = parse_ascii_hex_u32(&hdr.c_namesize)? as usize;

        ptr += size_of::<CpioNewcHeader>();
        if ptr + namesize > blob.len() {
            bail!("programmer archive is truncated");
        }

        if namesize == 0 {
            bail!("missing filename in programmer archive entry");
        }

        let name_raw = &blob[ptr..ptr + namesize];
        let name_end = name_raw
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(name_raw.len());
        let name = std::str::from_utf8(&name_raw[..name_end])
            .map_err(|_| anyhow::anyhow!("invalid utf8 in programmer archive filename"))?;

        if name == "TRAILER!!!" {
            break;
        }

        let id_str = name.split(':').next().unwrap_or("");
        if id_str.is_empty() {
            bail!("missing image id in programmer archive entry");
        }

        let id = id_str
            .parse::<u32>()
            .map_err(|_| anyhow::anyhow!("invalid decimal image id \"{}\"", id_str))?;
        if id == 0 {
            bail!("invalid image id \"{}\" in programmer archive", id_str);
        }

        ptr += namesize;
        ptr = align_up_4(ptr);
        if ptr + filesize > blob.len() {
            bail!("programmer archive is truncated");
        }

        let file_data = &blob[ptr..ptr + filesize];
        if id as usize >= images.len() {
            images.resize(id as usize + 1, None);
        }
        images[id as usize] = Some(file_data.to_vec());

        ptr += filesize;
        ptr = align_up_4(ptr);
    }

    Ok(true)
}

/// Load Sahara programmer image(s) from disk.
///
/// If `path` points to a CPIO `newc` archive, this decodes entries named like
/// `<id>:<name>` (or just `<id>`) and stores each file in its Sahara image slot.
/// Otherwise, the file is returned as a single-slot image list, preserving
/// legacy single-image Sahara behavior.
pub fn load_programmer_images(path: impl AsRef<Path>) -> Result<Vec<Option<Vec<u8>>>> {
    let blob = fs::read(path.as_ref())
        .with_context(|| format!("Couldn't read programmer image {}", path.as_ref().display()))?;
    let mut images: Vec<Option<Vec<u8>>> = Vec::new();

    if decode_programmer_archive(&blob, &mut images)? {
        return Ok(images);
    }

    Ok(vec![Some(blob)])
}

/// Open exactly one explicitly selected candidate. Enumeration never opens a device.
pub fn select_and_open<T>(
    keys: &[String],
    requested: Option<&str>,
    open: impl FnOnce(usize) -> Result<T>,
) -> Result<T> {
    let requested =
        requested.with_context(|| format!("Select a device explicitly; candidates: {keys:?}"))?;
    let mut matches = keys
        .iter()
        .enumerate()
        .filter(|(_, key)| key.as_str() == requested);
    let (index, _) = matches
        .next()
        .with_context(|| format!("No device matches {requested:?}; candidates: {keys:?}"))?;
    if matches.next().is_some() {
        bail!("Device selection {requested:?} is ambiguous");
    }
    open(index)
}

pub fn setup_target_device(
    backend: QdlBackend,
    _serial_no: Option<String>,
    _port: Option<String>,
) -> Result<Box<dyn QdlReadWrite>> {
    match backend {
        #[cfg(feature = "serial")]
        QdlBackend::Serial => match serial::setup_serial_device(_port) {
            Ok(d) => Ok(Box::new(d)),
            Err(e) => Err(e),
        },
        #[cfg(feature = "usb")]
        QdlBackend::Usb => match usb::setup_usb_device(_serial_no) {
            Ok(d) => Ok(Box::new(d)),
            Err(e) => Err(e),
        },
        // If all back-ends are compiled in, this throws a warning
        #[allow(unreachable_patterns)]
        _ => bail!("The {:?} backend is not supported in this build", backend),
    }
}

/// Wrapper for easily creating Firehose-y XML packets
fn firehose_xml_setup(op: &str, kvps: &[(&str, &str)]) -> anyhow::Result<Vec<u8>> {
    let mut xml = Element::new("data");
    let mut op_node = Element::new(op);
    for kvp in kvps.iter() {
        op_node
            .attributes
            .insert(kvp.0.to_owned(), kvp.1.to_owned());
    }

    xml.children.push(XMLNode::Element(op_node));

    // TODO: define a more verbose level
    // println!("SEND: {}", format!("{:?}", xml).bright_cyan());

    let mut buf = Vec::<u8>::new();
    xml.write(&mut buf)?;

    Ok(buf)
}

/// Read one XML document without consuming any following raw bytes or documents.
fn firehose_read_xml<T: QdlChan>(channel: &mut T, welcome: bool) -> Result<Option<Element>> {
    let limit = channel.fh_config().xml_buf_size;
    if limit == 0 || limit > 16 * 1024 * 1024 {
        bail!("Invalid XML receive limit {limit}");
    }
    let mut pending = Vec::new();
    loop {
        let available = match channel.fill_buf() {
            Ok(buf) => buf,
            Err(e) if welcome && pending.is_empty() && e.kind() == ErrorKind::TimedOut => {
                return Ok(None);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        if available.is_empty() {
            return Err(
                io::Error::new(ErrorKind::UnexpectedEof, "EOF before Firehose response").into(),
            );
        }
        // Only welcome collection can discard whitespace between log documents.
        // Command ACKs return before any following raw-storage bytes are consumed.
        if welcome && pending.is_empty() {
            let whitespace = available
                .iter()
                .take_while(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
                .count();
            if whitespace != 0 {
                channel.consume(whitespace);
                continue;
            }
        }
        // Copy only up to the document limit, including a possible split end marker.
        let old_len = pending.len();
        let copied = available.len().min(limit - old_len);
        pending.extend_from_slice(&available[..copied]);
        if let Some(pos) = pending.windows(7).position(|w| w == b"</data>") {
            let end = pos + 7;
            channel.consume(end - old_len);
            let xml = Element::parse(&pending[..end]).context("Invalid Firehose XML")?;
            if xml.name != "data" {
                bail!("Got a Firehose packet without a data tag");
            }
            return Ok(Some(xml));
        }
        channel.consume(copied);
        if pending.len() == limit {
            bail!("Firehose XML exceeds receive limit {limit}");
        }
    }
}

/// Device-provided storage information, without parsing console output.
#[derive(Debug)]
pub struct FirehoseStorageInfo {
    pub logs: Vec<String>,
    pub attributes: IndexMap<String, String>,
}

fn log_message<T: QdlChan>(channel: &T, node: &Element) -> String {
    let message = node.attributes.get("value").cloned().unwrap_or_default();
    if !channel.fh_config().quiet && !channel.fh_config().skip_firehose_log {
        eprintln!("LOG: {message}");
    }
    message
}

fn firehose_response<T: QdlChan>(
    channel: &mut T,
    parser: fn(&mut T, &IndexMap<String, String>) -> Result<FirehoseStatus>,
    collect_logs: bool,
) -> Result<(FirehoseStatus, FirehoseStorageInfo)> {
    let mut logs = Vec::new();
    let mut log_bytes = 0usize;
    loop {
        let xml = firehose_read_xml(channel, false)?.context("Missing Firehose response")?;
        let nodes: Vec<_> = xml
            .children
            .iter()
            .filter_map(XMLNode::as_element)
            .collect();
        if nodes.len() != 1 {
            bail!("Expected one Firehose response or log node");
        }
        let node = nodes[0];
        if node.name == "log" {
            let message = log_message(channel, node);
            if collect_logs {
                log_bytes += message.len();
                if log_bytes > 16 * 1024 * 1024 {
                    bail!("Storage information exceeds log limit");
                }
                logs.push(message);
            }
            continue;
        }
        if node.name != "response" {
            bail!("Unexpected Firehose node {}", node.name);
        }
        let status = parser(channel, &node.attributes)?;
        // Preserve a complete NAK, but never act on retry/restart hints.
        if status != FirehoseStatus::Nak
            && (node.attributes.contains_key("AttemptRetry")
                || node.attributes.contains_key("AttemptRestart"))
        {
            bail!("Firehose requested retry/restart without NAK; session stopped");
        }
        if !channel.fh_config().quiet && channel.fh_config().verbose_firehose {
            eprintln!("RECV: {node:?}");
        }
        return Ok((
            status,
            FirehoseStorageInfo {
                logs,
                attributes: node.attributes.clone(),
            },
        ));
    }
}

/// Wait for a complete response. Logs and partial XML can never acknowledge a command.
pub fn firehose_read<T: QdlChan>(
    channel: &mut T,
    response_parser: fn(&mut T, &IndexMap<String, String>) -> Result<FirehoseStatus>,
) -> Result<FirehoseStatus> {
    Ok(firehose_response(channel, response_parser, false)?.0)
}

/// Collect startup logs separately. Only an idle timeout at an XML boundary ends collection.
pub fn firehose_read_welcome<T: QdlChan>(channel: &mut T) -> Result<Vec<String>> {
    let mut logs = Vec::new();
    let mut log_bytes = 0usize;
    while let Some(xml) = firehose_read_xml(channel, true)? {
        let nodes: Vec<_> = xml
            .children
            .iter()
            .filter_map(XMLNode::as_element)
            .collect();
        if nodes.len() != 1 || nodes[0].name != "log" {
            bail!("Expected a welcome log");
        }
        let message = log_message(channel, nodes[0]);
        let done = message.starts_with("INFO: End of supported functions");
        log_bytes += message.len();
        if log_bytes > 16 * 1024 * 1024 {
            bail!("Welcome logs exceed limit");
        }
        logs.push(message);
        if done {
            break;
        }
    }
    Ok(logs)
}

/// Send XML and preserve the original transport failure. No recovery commands are sent.
pub fn firehose_write<T: QdlChan>(channel: &mut T, buf: &mut [u8]) -> Result<()> {
    let mut bytes = buf.to_vec();
    if !bytes.is_empty() && bytes.len().is_multiple_of(512) {
        bytes.push(b'\n');
    }
    channel.write_all(&bytes)?;
    channel.flush()?;
    Ok(())
}

/// Send a request and wait for its final ACK, retaining confirmation on failure.
pub fn firehose_write_getack<T: QdlChan>(
    channel: &mut T,
    buf: &mut [u8],
    couldnt_what: String,
) -> Result<(), MutationFailure> {
    firehose_write(channel, buf).map_err(MutationFailure::unknown)?;
    match firehose_read(channel, firehose_parser_ack_nak).map_err(MutationFailure::unknown)? {
        FirehoseStatus::Ack => Ok(()),
        FirehoseStatus::Nak => Err(MutationFailure::rejected(anyhow::anyhow!(
            "Couldn't {couldnt_what}: NAK"
        ))),
    }
}

/// Test performance without sample data
pub fn firehose_benchmark<T: QdlChan>(
    channel: &mut T,
    trials: u32,
    test_write_perf: bool,
) -> anyhow::Result<()> {
    let mut xml = firehose_xml_setup(
        "benchmark",
        &[
            ("trials", &trials.to_string()),
            (
                "TestWritePerformance",
                &(test_write_perf as u32).to_string(),
            ),
            (
                "TestReadPerformance",
                &(!test_write_perf as u32).to_string(),
            ),
        ],
    )?;

    firehose_write_getack(channel, &mut xml, "issue a NOP".to_owned()).map_err(Into::into)
}

/// Send a "Hello"-type packet to the Device
pub fn firehose_configure<T: QdlChan>(
    channel: &mut T,
    skip_storage_init: bool,
) -> anyhow::Result<()> {
    let config = channel.fh_config();
    config.validate()?;
    let mut xml = firehose_xml_setup(
        "configure",
        &[
            ("AckRawDataEveryNumPackets", "0"), // TODO: (low prio)
            (
                "SkipWrite",
                &(channel.fh_config().bypass_storage as u32).to_string(),
            ),
            ("SkipStorageInit", &(skip_storage_init as u32).to_string()),
            ("MemoryName", &config.storage_type.to_string()),
            ("AlwaysValidate", &(config.hash_packets as u32).to_string()),
            ("Verbose", &(config.verbose_firehose as u32).to_string()),
            ("MaxDigestTableSizeInBytes", "8192"), // TODO: (low prio)
            (
                "MaxPayloadSizeToTargetInBytes",
                &config.send_buffer_size.to_string(),
            ),
            // Zero-length-packet aware host
            ("ZLPAwareHost", "1"),
        ],
    )?;

    firehose_write(channel, &mut xml)
}

/// Do nothing, hopefully succesfully
pub fn firehose_nop<T: QdlChan>(channel: &mut T) -> anyhow::Result<()> {
    let mut xml = firehose_xml_setup("nop", &[("value", "ping")])?;

    firehose_write_getack(channel, &mut xml, "issue a NOP".to_owned()).map_err(Into::into)
}

/// Get information about the physical partition of a storage medium (e.g. LUN)
/// Returns raw log messages and response attributes.
pub fn firehose_get_storage_info<T: QdlChan>(
    channel: &mut T,
    phys_part_idx: u8,
) -> Result<FirehoseStorageInfo> {
    let mut xml = firehose_xml_setup(
        "getstorageinfo",
        &[("physical_partition_number", &phys_part_idx.to_string())],
    )?;

    firehose_write(channel, &mut xml)?;

    let (status, info) = firehose_response(channel, firehose_parser_ack_nak, true)?;
    if status == FirehoseStatus::Nak {
        bail!("Storage information request was NAKed");
    }
    Ok(info)
}

/// Alter Device (TODO: or Host) storage
pub fn firehose_patch<T: QdlChan>(
    channel: &mut T,
    byte_off: u64,
    slot: u8,
    phys_part_idx: u8,
    size: u64,
    start_sector: &str,
    val: &str,
) -> Result<(), MutationFailure> {
    let mut xml: Vec<u8> = firehose_xml_setup(
        "patch",
        &[
            (
                "SECTOR_SIZE_IN_BYTES",
                &channel.fh_config().storage_sector_size.to_string(),
            ),
            ("byte_offset", &byte_off.to_string()),
            ("filename", "DISK"), // DISK means "patch device's storage"
            ("slot", &slot.to_string()),
            ("physical_partition_number", &phys_part_idx.to_string()),
            ("size_in_bytes", &size.to_string()),
            ("start_sector", start_sector),
            ("value", val),
        ],
    )
    .map_err(MutationFailure::not_sent)?;

    firehose_write_getack(channel, &mut xml, "patch".to_string())
}

/// Peek at memory
/// Prints to \<log\> only
pub fn firehose_peek<T: QdlChan>(
    channel: &mut T,
    addr: u64,
    byte_count: u64,
) -> anyhow::Result<()> {
    if channel.fh_config().skip_firehose_log {
        println!(
            "{}",
            "Warning: firehose <peek> only prints to <log>, remove --skip-firehose-log"
                .bright_red()
        );
    }

    let mut xml: Vec<u8> = firehose_xml_setup(
        "peek",
        &[
            ("address64", &addr.to_string()),
            ("size_in_bytes", &byte_count.to_string()),
        ],
    )?;

    firehose_write_getack(channel, &mut xml, format!("peek @ {addr:#x}")).map_err(Into::into)
}

/// Poke at memory
/// This can lead to lock-ups and resets
// TODO:x
pub fn firehose_poke<T: QdlChan>(
    channel: &mut T,
    addr: u64,
    // TODO: byte count is 1..=8
    byte_count: u8,
    val: u64,
) -> anyhow::Result<()> {
    let mut xml: Vec<u8> = firehose_xml_setup(
        "poke",
        &[
            ("address64", &addr.to_string()),
            ("size_in_bytes", &byte_count.to_string()),
            ("value", &val.to_string()),
        ],
    )?;

    firehose_write_getack(channel, &mut xml, format!("poke @ {addr:#x}")).map_err(Into::into)
}

/// Write to Device storage
pub fn firehose_program_storage<T: QdlChan>(
    channel: &mut T,
    data: &mut impl Read,
    label: &str,
    num_sectors: usize,
    slot: u8,
    phys_part_idx: u8,
    start_sector: &str,
) -> Result<(), MutationFailure> {
    let config = *channel.fh_config();
    config.validate().map_err(MutationFailure::not_sent)?;
    let mut bytes_left = num_sectors
        .checked_mul(config.storage_sector_size)
        .ok_or_else(|| MutationFailure::not_sent(anyhow::anyhow!("Program byte count overflow")))?;
    let mut xml = firehose_xml_setup(
        "program",
        &[
            (
                "SECTOR_SIZE_IN_BYTES",
                &channel.fh_config().storage_sector_size.to_string(),
            ),
            ("num_partition_sectors", &num_sectors.to_string()),
            ("slot", &slot.to_string()),
            ("physical_partition_number", &phys_part_idx.to_string()),
            ("start_sector", start_sector),
            (
                "read_back_verify",
                &(channel.fh_config().read_back_verify as u32).to_string(),
            ),
        ],
    )
    .map_err(MutationFailure::not_sent)?;

    firehose_write_getack(channel, &mut xml, "start program".to_owned())?;
    let mut pb = (!config.quiet).then(|| {
        let mut pb = ProgressBar::new(bytes_left as u64);
        pb.show_time_left = true;
        pb.message(&format!("Sending partition {label}: "));
        pb.set_units(Units::Bytes);
        pb
    });
    let mut buf = vec![0; min(bytes_left, config.send_buffer_size)];
    while bytes_left > 0 {
        let len = min(bytes_left, buf.len());
        data.read_exact(&mut buf[..len])
            .with_context(|| format!("Error reading input for partition {label}"))
            .map_err(MutationFailure::unknown)?;
        if config.backend == QdlBackend::Usb {
            // A USB short OUT completion must not be resumed as another packet.
            let n = channel
                .write(&buf[..len])
                .map_err(MutationFailure::unknown)?;
            if n != len {
                return Err(MutationFailure::unknown(io::Error::new(
                    ErrorKind::WriteZero,
                    "Short USB data write",
                )));
            }
        } else {
            channel
                .write_all(&buf[..len])
                .map_err(MutationFailure::unknown)?;
        }
        bytes_left -= len;
        if let Some(pb) = &mut pb {
            pb.add(len as u64);
        }
    }
    if config.backend == QdlBackend::Usb {
        channel
            .write(&[])
            .context("Error sending ZLP")
            .map_err(MutationFailure::unknown)?;
    }
    channel.flush().map_err(MutationFailure::unknown)?;
    match firehose_read(channel, firehose_parser_ack_nak).map_err(MutationFailure::unknown)? {
        FirehoseStatus::Ack => Ok(()),
        FirehoseStatus::Nak => Err(MutationFailure::rejected(anyhow::anyhow!(
            "Program completion was NAKed"
        ))),
    }
}

/// Get a SHA256 digest of a portion of Device storage
pub fn firehose_checksum_storage<T: QdlChan>(
    channel: &mut T,
    num_sectors: usize,
    phys_part_idx: u8,
    start_sector: u64,
) -> anyhow::Result<()> {
    let mut xml = firehose_xml_setup(
        "getsha256digest",
        &[
            (
                "SECTOR_SIZE_IN_BYTES",
                &channel.fh_config().storage_sector_size.to_string(),
            ),
            ("num_partition_sectors", &num_sectors.to_string()),
            ("physical_partition_number", &phys_part_idx.to_string()),
            ("start_sector", &start_sector.to_string()),
        ],
    )?;

    firehose_write(channel, &mut xml)?;

    // TODO: figure out some sane way to figure out the timeout
    if firehose_read::<T>(channel, firehose_parser_ack_nak)? != FirehoseStatus::Ack {
        bail!("Checksum request was NAKed");
    }

    Ok(())
}

/// Read (sector-aligned) parts of storage.
pub fn firehose_read_storage(
    channel: &mut impl QdlChan,
    out: &mut impl Write,
    num_sectors: usize,
    slot: u8,
    phys_part_idx: u8,
    start_sector: u64,
) -> anyhow::Result<()> {
    let config = *channel.fh_config();
    config.validate()?;
    let mut bytes_left = num_sectors
        .checked_mul(config.storage_sector_size)
        .context("Read byte count overflow")?;
    let mut xml = firehose_xml_setup(
        "read",
        &[
            (
                "SECTOR_SIZE_IN_BYTES",
                &channel.fh_config().storage_sector_size.to_string(),
            ),
            ("num_partition_sectors", &num_sectors.to_string()),
            ("slot", &slot.to_string()),
            ("physical_partition_number", &phys_part_idx.to_string()),
            ("start_sector", &start_sector.to_string()),
        ],
    )?;

    firehose_write(channel, &mut xml)?;
    let (status, response) = firehose_response(channel, firehose_parser_ack_nak, false)?;
    if status == FirehoseStatus::Nak {
        let attrs = &response.attributes;
        if attrs.contains_key("AttemptRestart")
            || attrs.contains_key("AttemptRetry")
            || attrs.get("rawmode").is_some_and(|mode| mode != "false")
        {
            bail!("Read was NAKed without a usable command boundary: {attrs:?}");
        }
        return Err(ReadRejected.into());
    }

    let mut pb = (!config.quiet).then(|| {
        let mut pb = ProgressBar::new(bytes_left as u64);
        pb.set_units(Units::Bytes);
        pb
    });
    let mut buf = vec![0; min(bytes_left, config.recv_buffer_size)];
    while bytes_left > 0 {
        let len = min(bytes_left, buf.len());
        let n = match channel.read(&mut buf[..len]) {
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            result => result.context("Error receiving data")?,
        };
        if n == 0 {
            return Err(io::Error::new(ErrorKind::UnexpectedEof, "EOF in storage data").into());
        }
        out.write_all(&buf[..n])
            .context("Error writing storage output")?;
        bytes_left -= n;
        if let Some(pb) = &mut pb {
            pb.add(n as u64);
        }
    }
    out.flush().context("Error flushing storage output")?;
    // nusb's streaming reader consumes USB packet delimiters itself.
    if firehose_read(channel, firehose_parser_ack_nak)? != FirehoseStatus::Ack {
        bail!("Failed to complete 'read' op");
    }

    Ok(())
}

/// Reboot or power off the Device
pub fn firehose_reset<T: QdlChan>(
    channel: &mut T,
    mode: &FirehoseResetMode,
    delay_in_sec: u32,
) -> Result<(), MutationFailure> {
    let mut xml = firehose_xml_setup(
        "power",
        &[
            (
                "value",
                match mode {
                    FirehoseResetMode::ResetToEdl => "reset_to_edl",
                    FirehoseResetMode::Reset => "reset",
                    FirehoseResetMode::Off => "off",
                },
            ),
            ("DelayInSeconds", &delay_in_sec.to_string()),
        ],
    )
    .map_err(MutationFailure::not_sent)?;

    firehose_write_getack(channel, &mut xml, "reset the Device".to_owned())?;

    Ok(())
}

/// Mark a physical storage partition as bootable
pub fn firehose_set_bootable<T: QdlChan>(
    channel: &mut T,
    drive_idx: u8,
) -> Result<(), MutationFailure> {
    let mut xml = firehose_xml_setup(
        "setbootablestoragedrive",
        &[("value", &drive_idx.to_string())],
    )
    .map_err(MutationFailure::not_sent)?;

    firehose_write_getack(
        channel,
        &mut xml,
        format!("set partition {drive_idx} as bootable"),
    )
}

pub fn firehose_get_default_sector_size(t: &str) -> Option<usize> {
    match FirehoseStorageType::from_str(t).ok()? {
        FirehoseStorageType::Emmc => Some(512),
        FirehoseStorageType::Nand => Some(4096),
        FirehoseStorageType::Nvme => Some(512),
        FirehoseStorageType::Ufs => Some(4096),
        FirehoseStorageType::Spinor => Some(4096),
    }
}
