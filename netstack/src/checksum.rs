use crate::{IpProtocol, Ipv4Address, Ipv6Address};

pub fn internet_checksum(bytes: &[u8]) -> u16 {
    finish_checksum(sum_words(bytes))
}

pub fn ipv4_checksum(header: &[u8]) -> u16 {
    internet_checksum(header)
}

pub fn udp_checksum(
    source: crate::IpAddress,
    destination: crate::IpAddress,
    datagram: &[u8],
) -> u16 {
    match (source, destination) {
        (crate::IpAddress::Ipv4(source), crate::IpAddress::Ipv4(destination)) => finish_checksum(
            ipv4_pseudo_sum(source, destination, IpProtocol::Udp, datagram.len())
                + sum_words(datagram),
        ),
        (crate::IpAddress::Ipv6(source), crate::IpAddress::Ipv6(destination)) => finish_checksum(
            ipv6_pseudo_sum(source, destination, IpProtocol::Udp, datagram.len())
                + sum_words(datagram),
        ),
        _ => panic!("UDP pseudo-header address families must match"),
    }
}

pub fn tcpv4_checksum(source: Ipv4Address, destination: Ipv4Address, segment: &[u8]) -> u16 {
    finish_checksum(
        ipv4_pseudo_sum(source, destination, IpProtocol::Tcp, segment.len()) + sum_words(segment),
    )
}

pub fn tcpv6_checksum(source: Ipv6Address, destination: Ipv6Address, segment: &[u8]) -> u16 {
    finish_checksum(
        ipv6_pseudo_sum(source, destination, IpProtocol::Tcp, segment.len()) + sum_words(segment),
    )
}

pub fn icmpv6_checksum(source: Ipv6Address, destination: Ipv6Address, message: &[u8]) -> u16 {
    finish_checksum(
        ipv6_pseudo_sum(source, destination, IpProtocol::Icmpv6, message.len())
            + sum_words(message),
    )
}

fn ipv4_pseudo_sum(
    source: Ipv4Address,
    destination: Ipv4Address,
    protocol: IpProtocol,
    payload_len: usize,
) -> u32 {
    assert!(
        payload_len <= u16::MAX as usize,
        "IPv4 payload exceeds pseudo-header length field"
    );
    let mut sum = 0u32;
    sum += sum_words(&source.octets());
    sum += sum_words(&destination.octets());
    sum += u32::from(protocol.number());
    sum += payload_len as u32;
    sum
}

fn ipv6_pseudo_sum(
    source: Ipv6Address,
    destination: Ipv6Address,
    protocol: IpProtocol,
    payload_len: usize,
) -> u32 {
    let payload_len =
        u32::try_from(payload_len).unwrap_or_else(|_| panic!("IPv6 payload exceeds u32 length"));
    let mut sum = 0u32;
    sum += sum_words(&source.octets());
    sum += sum_words(&destination.octets());
    sum += sum_words(&payload_len.to_be_bytes());
    sum += u32::from(protocol.number());
    sum
}

fn sum_words(bytes: &[u8]) -> u32 {
    sum_words_wide(bytes)
}

#[cfg(test)]
fn sum_words_scalar(bytes: &[u8]) -> u32 {
    let mut chunks = bytes.chunks_exact(2);
    let mut sum = chunks
        .by_ref()
        .map(|chunk| u32::from(u16::from_be_bytes([chunk[0], chunk[1]])))
        .sum::<u32>();
    if let [last] = chunks.remainder() {
        sum += u32::from(*last) << 8;
    }
    sum
}

fn sum_words_wide(bytes: &[u8]) -> u32 {
    use wide::{u8x16, u16x8, u32x8};

    let mut offset = 0usize;
    let mut lanes = u32x8::default();
    let high_byte_indices = u8x16::new([
        0, 2, 4, 6, 8, 10, 12, 14, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80,
    ]);
    let low_byte_indices = u8x16::new([
        1, 3, 5, 7, 9, 11, 13, 15, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80,
    ]);
    while offset + 16 <= bytes.len() {
        let vector = u8x16::from(&bytes[offset..offset + 16]);
        let high_bytes = vector.swizzle_relaxed(high_byte_indices);
        let low_bytes = vector.swizzle_relaxed(low_byte_indices);
        let words = (u16x8::from_u8x16_low(high_bytes) << 8u8) | u16x8::from_u8x16_low(low_bytes);
        lanes += u32x8::from(words);
        offset += 16;
    }

    let mut sum = lanes.to_array().into_iter().sum::<u32>();
    let mut chunks = bytes[offset..].chunks_exact(2);
    for chunk in chunks.by_ref() {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
    }
    if let [last] = chunks.remainder() {
        sum += u32::from(*last) << 8;
    }
    sum
}

fn finish_checksum(mut sum: u32) -> u16 {
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    #[test]
    fn checksum_matches_scalar_reference_for_large_payload() {
        let mut bytes = [0u8; 4097];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = index.wrapping_mul(37).wrapping_add(19) as u8;
        }

        assert_eq!(
            super::finish_checksum(super::sum_words(&bytes)),
            super::finish_checksum(super::sum_words_scalar(&bytes))
        );
    }
}
