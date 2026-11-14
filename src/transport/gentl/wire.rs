//! Length-delimited helper control and pixel payloads. All limits are checked
//! before allocation, including replies from a replaced helper executable.
use super::{MAX_FRAME, MAX_XML, Selector};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    io::{Read, Write},
    path::PathBuf,
};

const MAX_JSON: usize = 1024 * 1024;
pub(super) const PROTOCOL: u32 = 4;
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Hello {
    pub(super) protocol: u32,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Request {
    Hello {
        protocol: u32,
    },
    Discover {
        producer: PathBuf,
        timeout_ms: u64,
    },
    Open {
        selector: Selector,
        timeout_ms: u64,
    },
    Read {
        address: u64,
        length: usize,
    },
    Write {
        address: u64,
    },
    Xml {
        timeout_ms: u64,
    },
    ConfigurePixelEncodings {
        encodings: Vec<crate::types::PixelEncoding>,
    },
    EnableMultipartProjection,
    StreamPayloadSize,
    Start {
        payload_size: usize,
    },
    Next {
        timeout_ms: u64,
    },
    Stop,
    Close,
}
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Discovery {
    pub(super) cameras: Vec<crate::types::CameraInfo>,
    pub(super) warnings: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reply {
    pub(super) ok: bool,
    #[serde(default)]
    pub(super) value: Value,
    #[serde(default)]
    pub(super) error: Option<String>,
    #[serde(default)]
    pub(super) tag: Option<crate::types::ErrorTag>,
}
impl Reply {
    pub(super) const fn ok(value: Value) -> Self {
        Self {
            ok: true,
            value,
            error: None,
            tag: None,
        }
    }
    pub(super) fn error(error: &anyhow::Error) -> Self {
        Self {
            ok: false,
            value: Value::Null,
            error: Some(crate::scrub::scrub(&format!("{error:#}")).into_owned()),
            tag: crate::types::error_tag(error),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Image {
    pub(super) id: u64,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) pixel_format: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) pixel_encoding: Option<crate::types::PixelEncoding>,
    pub(super) device_ts_ns: Option<u64>,
    pub(super) device_timestamp_ticks: Option<u64>,
    pub(super) device_timestamp_frequency_hz: Option<u64>,
    pub(super) offset: usize,
    pub(super) x_padding: usize,
    pub(super) y_padding: usize,
    pub(super) byte_order: Option<crate::transport::chunks::ByteOrder>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) chunks: Option<ChunkLayout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) multipart: Option<crate::transport::multipart::Layout>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChunkLayout {
    pub(super) layout_id: Option<u64>,
    pub(super) entries: Vec<crate::transport::chunks::Chunk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) descriptor_error: Option<String>,
}
pub(super) struct Packet<T> {
    pub(super) request: T,
    pub(super) payload: Vec<u8>,
}
pub(super) enum Payload {
    Bytes(Vec<u8>),
    Frame(crate::framebuf::PoolVec),
}
impl Payload {
    pub(super) fn into_bytes(self) -> Result<Vec<u8>> {
        match self {
            Self::Bytes(bytes) => Ok(bytes),
            Self::Frame(_) => anyhow::bail!("unexpected GenTL frame payload"),
        }
    }
    pub(super) fn into_frame(self) -> Result<crate::framebuf::PoolVec> {
        match self {
            Self::Frame(frame) => Ok(frame),
            Self::Bytes(_) => anyhow::bail!("GenTL reply omitted its image buffer"),
        }
    }
}
pub(super) struct Response {
    pub(super) reply: Reply,
    pub(super) payload: Payload,
}

fn write(output: &mut impl Write, header: &impl Serialize, payload: &[u8]) -> Result<()> {
    write_parts(output, header, &[payload])
}
fn write_parts(output: &mut impl Write, header: &impl Serialize, parts: &[&[u8]]) -> Result<()> {
    let length = parts.iter().try_fold(0usize, |sum, part| {
        sum.checked_add(part.len())
            .context("GenTL payload length overflow")
    })?;
    let header = serde_json::to_vec(header)?;
    ensure!(
        header.len() <= MAX_JSON && length <= MAX_FRAME,
        "GenTL helper packet exceeds size limit"
    );
    output.write_all(&u32::try_from(header.len())?.to_le_bytes())?;
    output.write_all(&u64::try_from(length)?.to_le_bytes())?;
    output.write_all(&header)?;
    for part in parts {
        output.write_all(part)?;
    }
    output.flush()?;
    Ok(())
}
fn read<T: DeserializeOwned>(input: &mut impl Read, maximum: usize) -> Result<Option<(T, usize)>> {
    let mut prefix = [0; 12];
    loop {
        match input.read(&mut prefix[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    input.read_exact(&mut prefix[1..])?;
    let json = usize::try_from(u32::from_le_bytes(prefix[..4].try_into()?))?;
    let bytes = usize::try_from(u64::from_le_bytes(prefix[4..].try_into()?))?;
    ensure!(
        (1..=MAX_JSON).contains(&json) && bytes <= maximum,
        "GenTL helper reply exceeds size limit"
    );
    let mut header = vec![0; json];
    input.read_exact(&mut header)?;
    let request =
        serde_json::from_slice(&header).context("invalid GenTL helper control message")?;
    Ok(Some((request, bytes)))
}
pub(super) fn write_request(
    output: &mut impl Write,
    request: &Request,
    payload: &[u8],
) -> Result<()> {
    write(output, request, payload)
}
pub(super) fn read_request(input: &mut impl Read) -> Result<Option<Packet<Request>>> {
    let Some((request, bytes)) = read(input, 64 * 1024)? else {
        return Ok(None);
    };
    let mut payload = vec![0; bytes];
    input.read_exact(&mut payload)?;
    let packet = Packet { request, payload };
    match &packet.request {
        Request::Write { .. } => {}
        _ => ensure!(
            packet.payload.is_empty(),
            "unexpected GenTL request payload"
        ),
    }
    if let Request::Read { length, .. } = packet.request {
        ensure!(length <= MAX_XML, "GenTL register read exceeds size limit");
    }
    Ok(Some(packet))
}
pub(super) fn write_reply(output: &mut impl Write, reply: &Reply, payload: &[u8]) -> Result<()> {
    write(output, reply, payload)
}
pub(super) fn write_frame(output: &mut impl Write, reply: &Reply, parts: &[&[u8]]) -> Result<()> {
    write_parts(output, reply, parts)
}
pub(super) fn read_reply(input: &mut impl Read, maximum: usize) -> Result<Response> {
    let (reply, bytes): (Reply, usize) =
        read(input, maximum)?.context("GenTL helper closed its output")?;
    ensure!(
        reply.ok == reply.error.is_none()
            && (reply.ok || (bytes == 0 && reply.value.is_null()))
            && (!reply.ok || reply.tag.is_none()),
        "inconsistent GenTL helper status"
    );
    let payload = if maximum == MAX_FRAME && reply.ok {
        // Every byte is filled by read_exact before it can become a frame.
        let mut frame = crate::framebuf::pool().take_stale(bytes);
        input.read_exact(&mut frame)?;
        Payload::Frame(frame)
    } else {
        let mut payload = vec![0; bytes];
        input.read_exact(&mut payload)?;
        Payload::Bytes(payload)
    };
    Ok(Response { reply, payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_limits_are_checked_before_reading_or_allocating_the_body() {
        for (json, bytes, maximum) in [
            (MAX_JSON + 1, 0, MAX_FRAME),
            (1, MAX_FRAME + 1, MAX_FRAME),
            (1, MAX_XML + 1, MAX_XML),
        ] {
            let mut input = Vec::new();
            input.extend_from_slice(&u32::try_from(json).unwrap().to_le_bytes());
            input.extend_from_slice(&u64::try_from(bytes).unwrap().to_le_bytes());
            assert!(read_reply(&mut input.as_slice(), maximum).is_err());
        }
    }

    #[test]
    fn truncated_and_inconsistent_replies_cannot_become_frames() {
        let mut packet = Vec::new();
        write_reply(&mut packet, &Reply::ok(Value::Null), &[1, 2, 3]).unwrap();
        packet.pop().unwrap();
        assert!(read_reply(&mut packet.as_slice(), MAX_FRAME).is_err());
        for reply in [
            Reply {
                ok: false,
                value: Value::Null,
                error: None,
                tag: None,
            },
            Reply {
                ok: true,
                value: Value::Null,
                error: Some("conflicting error".into()),
                tag: None,
            },
        ] {
            packet.clear();
            write_reply(&mut packet, &reply, &[]).unwrap();
            assert!(read_reply(&mut packet.as_slice(), MAX_FRAME).is_err());
        }
    }

    #[test]
    fn image_bytes_are_received_into_a_pool_buffer() {
        let mut packet = Vec::new();
        write_reply(&mut packet, &Reply::ok(Value::Null), &[1, 2, 3]).unwrap();
        let frame = read_reply(&mut packet.as_slice(), MAX_FRAME)
            .unwrap()
            .payload
            .into_frame()
            .unwrap();
        assert_eq!(&*frame, &[1, 2, 3]);
        let pointer = frame.as_ptr();
        assert_eq!(frame.freeze().as_ptr(), pointer);
    }
}
