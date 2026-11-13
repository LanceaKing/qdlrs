// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.

use indexmap::IndexMap;

use anyhow::{Context, bail};
use std::str::FromStr;

use crate::{FirehoseStatus, QdlChan};

/// The highest protocol version currently supported by the library
const FH_PROTO_VERSION_SUPPORTED: u32 = 1;

// Parsers are kept separate for more flexibility (e.g. log replay analysis)

/// Fetch an attribute and parse it, attaching context on a missing key or a
/// value that fails to parse.
fn parse_attr<T>(attrs: &IndexMap<String, String>, key: &str) -> anyhow::Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let val = attrs
        .get(key)
        .with_context(|| format!("Device response is missing the '{key}' attribute"))?;
    val.parse::<T>()
        .with_context(|| format!("Couldn't parse '{key}' attribute value \"{val}\""))
}

/// Check "value" for ack/nak (generic)
pub fn firehose_parser_ack_nak<T: QdlChan>(
    _: &mut T,
    attrs: &IndexMap<String, String>,
) -> Result<FirehoseStatus, anyhow::Error> {
    let val = attrs.get("value");
    match val.map(|s| s.as_str()) {
        Some("ACK") => Ok(FirehoseStatus::Ack),
        Some("NAK") => Ok(FirehoseStatus::Nak),
        _ => bail!("Got malformed data: {:?}", attrs),
    }
}

/// Parse the \<configure\> response
pub fn firehose_parser_configure_response<T: QdlChan>(
    channel: &mut T,
    attrs: &IndexMap<String, String>,
) -> Result<FirehoseStatus, anyhow::Error> {
    let status = firehose_parser_ack_nak(channel, attrs)?;
    if status == FirehoseStatus::Nak {
        return Ok(status);
    }

    let min_version_supported: u32 = parse_attr(attrs, "MinVersionSupported")?;
    if min_version_supported > FH_PROTO_VERSION_SUPPORTED {
        bail!(
            "Device requires protocol version >= {min_version_supported}, supported: {FH_PROTO_VERSION_SUPPORTED}"
        );
    }
    let version = attrs.get("Version").context("Missing protocol Version")?;
    let supported: usize = parse_attr(attrs, "MaxPayloadSizeToTargetInBytesSupported")?;
    let mut config = *channel.fh_config();
    config.xml_buf_size = parse_attr(attrs, "MaxXMLSizeInBytes")?;
    config.send_buffer_size = parse_attr(attrs, "MaxPayloadSizeToTargetInBytes")?;
    config.validate()?;
    if supported < config.send_buffer_size {
        bail!("Configured payload exceeds the device maximum");
    }
    // Accept only the acknowledged sizes. Do not issue hidden reconfiguration.
    *channel.mut_fh_config() = config;
    if !config.quiet {
        eprintln!("Found protocol version {version}");
    }
    Ok(status)
}
