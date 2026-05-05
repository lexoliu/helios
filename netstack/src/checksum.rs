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
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        return sum_words_neon(bytes);
    }

    #[cfg(not(all(target_arch = "aarch64", target_feature = "neon")))]
    {
        sum_words_scalar(bytes)
    }
}

#[cfg(not(all(target_arch = "aarch64", target_feature = "neon")))]
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

#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
fn sum_words_neon(bytes: &[u8]) -> u32 {
    use core::arch::aarch64::{vaddlvq_u16, vaddq_u16, vld1q_u8, vreinterpretq_u16_u8, vrev16q_u8};

    let mut offset = 0usize;
    let mut lanes;
    unsafe {
        lanes = core::mem::zeroed();
    }
    while offset + 16 <= bytes.len() {
        unsafe {
            let vector = vld1q_u8(bytes.as_ptr().add(offset));
            let words = vreinterpretq_u16_u8(vrev16q_u8(vector));
            lanes = vaddq_u16(lanes, words);
        }
        offset += 16;
    }

    let mut sum = unsafe { u32::from(vaddlvq_u16(lanes)) };
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
