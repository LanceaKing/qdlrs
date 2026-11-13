// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
use anyhow::{Context, Result};
use nusb::{
    self, Device, DeviceInfo, Endpoint, MaybeFuture,
    io::EndpointRead,
    transfer::{Bulk, Out, TransferError},
};
use std::{
    io::{BufRead, Error, ErrorKind, Read, Write},
    time::Duration,
};

use crate::{select_and_open, types::QdlReadWrite};

pub struct QdlUsbConfig {
    _dev: nusb::Device,
    reader: EndpointRead<nusb::transfer::Bulk>,
    writer: Endpoint<Bulk, Out>,
    buf: Vec<u8>,
    pos: usize,
    cap: usize,
}

// Use completions directly: EndpointWrite 0.2.1 does not check actual_len.
impl Write for QdlUsbConfig {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        let completion = self
            .writer
            .transfer_blocking(buf.to_vec().into(), Duration::from_secs(10));
        checked_write_len(
            buf.len(),
            completion.actual_len,
            completion.status.map_err(usb_transfer_error),
        )
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        // Every write already waits for completion.
        Ok(())
    }
}

// nusb maps cancellation to Interrupted, which write_all retries. A cancelled
// USB transfer can already have sent bytes, so it must stop the protocol call.
fn usb_transfer_error(error: TransferError) -> Error {
    match error {
        TransferError::Cancelled => Error::new(ErrorKind::TimedOut, error),
        other => other.into(),
    }
}

fn usb_read_error(error: Error) -> Error {
    if error
        .get_ref()
        .and_then(|e| e.downcast_ref::<TransferError>())
        == Some(&TransferError::Cancelled)
    {
        return Error::new(ErrorKind::TimedOut, error);
    }
    error
}

fn checked_write_len(
    expected: usize,
    actual: usize,
    status: std::io::Result<()>,
) -> std::io::Result<usize> {
    status?;
    if actual != expected {
        return Err(Error::new(
            ErrorKind::WriteZero,
            format!("Short USB write: {actual}/{expected}"),
        ));
    }
    Ok(actual)
}

impl Read for QdlUsbConfig {
    fn read(&mut self, out: &mut [u8]) -> Result<usize, std::io::Error> {
        if out.is_empty() {
            return Ok(0);
        }
        // Drain internal buffer first
        if self.pos < self.cap {
            let n = std::cmp::min(out.len(), self.cap - self.pos);
            out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        // Otherwise, read directly from USB
        self.reader.read(out).map_err(usb_read_error)
    }
}

impl BufRead for QdlUsbConfig {
    fn fill_buf(&mut self) -> Result<&[u8], std::io::Error> {
        if self.pos >= self.cap {
            self.pos = 0;
            self.cap = 0;
            if self.buf.is_empty() {
                self.buf.resize(4096, 0);
            }
            {
                let n = self.reader.read(&mut self.buf).map_err(usb_read_error)?;
                self.cap = n;
            }
        }
        Ok(&self.buf[self.pos..self.cap])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = std::cmp::min(self.pos + amt, self.cap);
    }
}

impl QdlReadWrite for QdlUsbConfig {}

const USB_VID_QCOM: u16 = 0x05c6;
const USB_PID_EDL: [u16; 2] = [0x9008 /* EDL */, 0x900e /* Ramdump */];
const INTF_DESC_PROTO_CODES: [u8; 3] = [0x10, 0x11, 0xFF];

fn edl_devices() -> Result<Vec<DeviceInfo>> {
    Ok(nusb::list_devices()
        .wait()?
        .filter(|d| d.vendor_id() == USB_VID_QCOM && USB_PID_EDL.contains(&d.product_id()))
        .collect())
}

fn location_key(info: &DeviceInfo) -> String {
    format!(
        "{}:{}",
        info.bus_id(),
        info.port_chain()
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(".")
    )
}

/// Stable physical-location identifiers. This enumerates but does not open devices.
pub fn list_usb_device_keys() -> Result<Vec<String>> {
    Ok(edl_devices()?.iter().map(location_key).collect())
}

pub fn setup_usb_device_by_key(requested: Option<&str>) -> Result<QdlUsbConfig> {
    let devices = edl_devices()?;
    let keys = devices.iter().map(location_key).collect::<Vec<_>>();
    select_and_open(&keys, requested, |index| {
        setup_usb_device_from_device(devices[index].open().wait()?)
    })
}

/// Legacy serial-number selection remains explicit and rejects duplicate serials.
pub fn setup_usb_device(serial_no: Option<String>) -> Result<QdlUsbConfig> {
    let devices: Vec<_> = edl_devices()?
        .into_iter()
        .filter_map(|d| {
            let key = d.product_string()?.split_once("_SN:")?.1.to_owned();
            Some((key, d))
        })
        .collect();
    let keys = devices
        .iter()
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    select_and_open(&keys, serial_no.as_deref(), |index| {
        setup_usb_device_from_device(devices[index].1.open().wait()?)
    })
}

fn setup_usb_device_from_device(dev: Device) -> Result<QdlUsbConfig> {
    // TODO: is there always precisely one interface like this?
    let cfg_desc = dev.active_configuration()?;
    let intf_desc = cfg_desc
        .interface_alt_settings()
        .find(|d| {
            d.class() == 0xFF
                && d.subclass() == 0xFF
                && INTF_DESC_PROTO_CODES.contains(&d.protocol())
                && d.num_endpoints() >= 2
        })
        .ok_or::<anyhow::Error>(Error::from(ErrorKind::NotFound).into())?;

    let in_ep = intf_desc
        .endpoints()
        .find(|e| {
            e.direction() == nusb::transfer::Direction::In
                && e.transfer_type() == nusb::descriptors::TransferType::Bulk
        })
        .context("Missing bulk endpoint")?
        .address();
    let out_ep = intf_desc
        .endpoints()
        .find(|e| {
            e.direction() == nusb::transfer::Direction::Out
                && e.transfer_type() == nusb::descriptors::TransferType::Bulk
        })
        .context("Missing bulk endpoint")?
        .address();

    // Make sure we can actually poke at the device
    let intf = dev
        .detach_and_claim_interface(intf_desc.interface_number())
        .wait()
        .with_context(|| format!("Couldn't claim interface{}", intf_desc.interface_number()))?;

    let mut rd = intf.endpoint(in_ep)?.reader(1024 * 1024);
    let wr = intf.endpoint(out_ep)?;

    rd.set_read_timeout(Duration::from_secs(10));

    Ok(QdlUsbConfig {
        _dev: dev,
        reader: rd,
        writer: wr,
        buf: Vec::new(),
        pos: 0,
        cap: 0,
    })
}

/// Open an EDL device at a known physical USB location.
pub fn setup_usb_device_at(bus_id: &str, port_numbers: &[u8]) -> Result<QdlUsbConfig> {
    let key = format!(
        "{bus_id}:{}",
        port_numbers
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(".")
    );
    setup_usb_device_by_key(Some(&key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_cancellation_is_not_retryable_interrupted_io() {
        let error = usb_transfer_error(TransferError::Cancelled);
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert_eq!(
            error.get_ref().unwrap().downcast_ref::<TransferError>(),
            Some(&TransferError::Cancelled)
        );
        let error = usb_read_error(TransferError::Cancelled.into());
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert_eq!(
            usb_transfer_error(TransferError::Disconnected).kind(),
            ErrorKind::ConnectionAborted
        );
    }

    #[test]
    fn usb_completion_requires_full_length_and_success() {
        assert_eq!(checked_write_len(8, 8, Ok(())).unwrap(), 8);
        assert_eq!(checked_write_len(0, 0, Ok(())).unwrap(), 0);
        assert_eq!(
            checked_write_len(8, 4, Ok(())).unwrap_err().kind(),
            ErrorKind::WriteZero
        );
        assert_eq!(
            checked_write_len(8, 8, Err(ErrorKind::TimedOut.into()))
                .unwrap_err()
                .kind(),
            ErrorKind::TimedOut
        );
    }
}
