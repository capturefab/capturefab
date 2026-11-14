//! PLC-5/E PCCC 0F/68. Rockwell 1770-RM516 pp. 7-28, 7-36–37 and 13-15
//! specify typed reads, descriptors and logical ASCII addresses. Rockwell's
//! EtherNet/IP Explicit Messaging Rev. 1.2 pp. 32–33 shows the complete exchange.
use super::{Address, Signal, protocol, typed_data};
use crate::types::Usage;
use anyhow::Result;

const MAX_ELEMENTS: u32 = 2000;
const MAX_TYPED_DATA: usize = 240;

pub(super) fn validate(signals: &[Signal], addresses: &[Address]) -> Result<()> {
    for (signal, address) in signals.iter().zip(addresses) {
        if !matches!(address.file_type, 0x85 | 0x89 | 0x8a)
            || !(3..=999).contains(&address.file)
            || u32::from(address.element) + u32::from(signal.elements) > MAX_ELEMENTS
        {
            return Err(Usage(
                "PCCC plc5 profile requires N/B/F files 3–999 and elements within 0–1999".into(),
            )
            .into());
        }
    }
    Ok(())
}

pub(super) fn request(
    output: &mut Vec<u8>,
    address: Address,
    element: u16,
    bytes: usize,
    transaction: u16,
) -> Result<()> {
    let file_type = match address.file_type {
        0x85 => 'B',
        0x89 => 'N',
        0x8a => 'F',
        _ => return Err(protocol("invalid PLC-5 file type")),
    };
    // Each segment is an independent one-packet transaction. Count elements
    // in both fields, even when a floating-point element takes two words.
    let elements = u16::try_from(bytes / address.width())?.to_le_bytes();
    output.extend([0x0f, 0]);
    output.extend(transaction.to_le_bytes());
    output.extend([0x68, 0, 0]); // FNC, packet offset 0
    output.extend(elements); // TotalTrans
    output.extend_from_slice(format!("\0${file_type}{}:{element}\0", address.file).as_bytes());
    output.extend(elements); // Size
    Ok(())
}

pub(super) fn response(
    body: &[u8],
    address: Address,
    transaction: u16,
    expected_bytes: usize,
) -> Result<&[u8]> {
    let mut data = typed_data(body, transaction)?;
    if data.len() > MAX_TYPED_DATA {
        return Err(protocol("PLC-5 typed reply exceeds 240 bytes"));
    }
    let mut descriptor = parse_descriptor(&mut data)?;
    if descriptor.id == 9 {
        // The array size includes its nested element descriptor.
        if descriptor.bytes != u64::try_from(data.len())? {
            return Err(protocol(
                "PLC-5 array descriptor length differs from payload",
            ));
        }
        descriptor = parse_descriptor(&mut data)?;
    } else if expected_bytes != address.width() {
        return Err(protocol(
            "PLC-5 multi-element reply is missing its array descriptor",
        ));
    }
    let correct_type = match address.file_type {
        // The binary file can be represented as a bit string or integer word.
        0x85 => matches!(descriptor.id, 2 | 4),
        0x89 => descriptor.id == 4,
        0x8a => descriptor.id == 8,
        _ => false,
    };
    if !correct_type || descriptor.bytes != u64::try_from(address.width())? {
        return Err(protocol(
            "PLC-5 element descriptor differs from requested file type",
        ));
    }
    if data.len() != expected_bytes {
        return Err(protocol(
            "PLC-5 data length differs from requested elements",
        ));
    }
    Ok(data)
}

struct Descriptor {
    id: u64,
    bytes: u64,
}

fn parse_descriptor(data: &mut &[u8]) -> Result<Descriptor> {
    let (&flags, remaining) = data
        .split_first()
        .ok_or_else(|| protocol("missing PLC-5 type/data descriptor"))?;
    *data = remaining;
    let id = descriptor_number(data, flags >> 4)?;
    let bytes = descriptor_number(data, flags & 0x0f)?;
    Ok(Descriptor { id, bytes })
}

fn descriptor_number(data: &mut &[u8], flags: u8) -> Result<u64> {
    if flags < 8 {
        return Ok(u64::from(flags));
    }
    let width = usize::from(flags & 7);
    if width == 0 {
        return Err(protocol("PLC-5 descriptor has a zero-length extension"));
    }
    let (value, rest) = data
        .split_at_checked(width)
        .ok_or_else(|| protocol("truncated PLC-5 type/data descriptor"))?;
    *data = rest;
    let mut bytes = [0; 8];
    bytes[..width].copy_from_slice(value);
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::{Address, request, response};
    use crate::timeseries::pccc::DataType;

    #[test]
    fn published_plc5e_ten_word_read_matches_the_packet_capture() {
        let address = Address {
            file_type: 0x89,
            file: 7,
            element: 0,
            bit: None,
            kind: DataType::I16,
        };
        // Rockwell EtherNet/IP Explicit Messaging Rev. 1.2, packets 5/6.
        let mut body = Vec::new();
        request(&mut body, address, 0, 20, 0x094c).unwrap();
        assert_eq!(body, b"\x0f\0\x4c\x09\x68\0\0\x0a\0\0$N7:0\0\x0a\0");
        let reply =
            b"\x4f\0\x4c\x09\x99\x09\x15\x42\x55\x55\xaa\xaa\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";
        let data = response(reply, address, 0x094c, 20).unwrap();
        assert_eq!(data, b"\x55\x55\xaa\xaa\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
    }
}
