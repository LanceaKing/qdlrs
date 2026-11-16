// SPDX-License-Identifier: BSD-3-Clause
#![allow(dead_code)] // Shared by independent integration-test binaries.

use std::{
    collections::VecDeque,
    io::{self, BufRead, ErrorKind, Read, Write},
    sync::{Arc, Mutex},
};

use qdl::types::{FirehoseConfiguration, QdlBackend, QdlDevice, QdlReadWrite};

pub const ACK: &[u8] = b"<data><response value=\"ACK\"/></data>";
pub const NAK: &[u8] = b"<data><response value=\"NAK\"/></data>";
pub const LOG: &[u8] = b"<data><log value=\"hello\"/></data>";

pub enum Input {
    Bytes(Vec<u8>),
    Eof,
    Error(ErrorKind),
}

pub fn bytes(value: &[u8]) -> Input {
    Input::Bytes(value.to_vec())
}

pub enum Output {
    Accept(usize),
    Error(ErrorKind),
}

#[derive(Default, Debug)]
pub struct Record {
    pub accepted: Vec<u8>,
    pub writes: Vec<Vec<u8>>,
    pub flushes: usize,
}

pub type Trace = Arc<Mutex<Record>>;

pub struct ScriptedIo {
    input: VecDeque<Input>,
    pending: Vec<u8>,
    pos: usize,
    pub output: VecDeque<Output>,
    pub flush_error: Option<ErrorKind>,
    pub trace: Trace,
}

impl ScriptedIo {
    pub fn new(input: impl IntoIterator<Item = Input>) -> Self {
        Self {
            input: input.into_iter().collect(),
            pending: Vec::new(),
            pos: 0,
            output: VecDeque::new(),
            flush_error: None,
            trace: Trace::default(),
        }
    }
}

impl BufRead for ScriptedIo {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos == self.pending.len() {
            self.pending.clear();
            self.pos = 0;
            match self.input.pop_front().expect("unscripted read") {
                Input::Bytes(data) => {
                    assert!(!data.is_empty(), "use explicit EOF for zero progress");
                    self.pending = data;
                }
                Input::Eof => return Ok(&[]),
                Input::Error(kind) => return Err(io::Error::new(kind, "scripted read failure")),
            }
        }
        Ok(&self.pending[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        assert!(amt <= self.pending.len() - self.pos);
        self.pos += amt;
    }
}

impl Read for ScriptedIo {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let available = self.fill_buf()?;
        let len = out.len().min(available.len());
        out[..len].copy_from_slice(&available[..len]);
        self.consume(len);
        Ok(len)
    }
}

impl Write for ScriptedIo {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut record = self.trace.lock().unwrap();
        record.writes.push(data.to_vec());
        let len = match self.output.pop_front() {
            Some(Output::Accept(limit)) => data.len().min(limit),
            Some(Output::Error(kind)) => {
                return Err(io::Error::new(kind, "scripted write failure"));
            }
            None => data.len(),
        };
        record.accepted.extend_from_slice(&data[..len]);
        Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.trace.lock().unwrap().flushes += 1;
        match self.flush_error.take() {
            Some(kind) => Err(io::Error::new(kind, "scripted flush failure")),
            None => Ok(()),
        }
    }
}

impl QdlReadWrite for ScriptedIo {}

pub fn device(input: impl IntoIterator<Item = Input>) -> (QdlDevice<ScriptedIo>, Trace) {
    let io = ScriptedIo::new(input);
    let trace = io.trace.clone();
    (
        QdlDevice {
            rw: Box::new(io),
            fh_cfg: FirehoseConfiguration {
                storage_sector_size: 4,
                send_buffer_size: 8,
                recv_buffer_size: 8,
                backend: QdlBackend::Serial,
                quiet: true,
                ..Default::default()
            },
            reset_on_drop: false,
        },
        trace,
    )
}

pub fn assert_no_reset(trace: &Trace) {
    let record = trace.lock().unwrap();
    let data = String::from_utf8_lossy(&record.accepted);
    for forbidden in ["<power", "<patch", "<setbootablestoragedrive"] {
        assert!(!data.contains(forbidden), "unexpected command: {data}");
    }
}

pub fn disk(sector: usize, names: &[&str]) -> Vec<u8> {
    let mut disk = std::io::Cursor::new(vec![0; 256 * sector]);
    let mut gpt = gptman::GPT::new_from(&mut disk, sector as u64, [42; 16]).unwrap();
    for (index, name) in names.iter().enumerate() {
        gpt[(index + 1) as u32] = gptman::GPTPartitionEntry {
            partition_type_guid: [1; 16],
            unique_partition_guid: [index as u8 + 1; 16],
            starting_lba: 64 + index as u64 * 4,
            ending_lba: 65 + index as u64 * 4,
            attribute_bits: 0,
            partition_name: (*name).into(),
        };
    }
    gpt.write_into(&mut disk).unwrap();
    disk.into_inner()
}
pub fn gpt_script(disk: &[u8], sector: usize) -> Vec<Input> {
    let h = gptman::GPTHeader::read_from(&mut std::io::Cursor::new(&disk[sector..])).unwrap();
    vec![
        bytes(ACK),
        bytes(&disk[sector..2 * sector]),
        bytes(ACK),
        bytes(ACK),
        bytes(&disk[..h.first_usable_lba as usize * sector]),
        bytes(ACK),
    ]
}
pub fn storage_device(
    input: impl IntoIterator<Item = Input>,
    sector: usize,
) -> (QdlDevice<ScriptedIo>, Trace) {
    let (mut dev, trace) = device(input);
    dev.fh_cfg.storage_sector_size = sector;
    dev.fh_cfg.send_buffer_size = 4096;
    dev.fh_cfg.recv_buffer_size = 4096;
    (dev, trace)
}
