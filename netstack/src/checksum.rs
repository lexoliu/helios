use crate::{IpProtocol, Ipv4Address, Ipv6Address};

const SIMD_CHECKSUM_MIN_BYTES: usize = 64;

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
    if bytes.len() < SIMD_CHECKSUM_MIN_BYTES {
        return sum_words_scalar(bytes);
    }
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        // SAFETY: aarch64 + neon target feature guarantees the
        // intrinsics below are available; the routine reads only the
        // input slice and never goes past `bytes.len()`.
        unsafe { sum_words_neon(bytes) }
    }
    #[cfg(not(all(target_arch = "aarch64", target_feature = "neon")))]
    {
        sum_words_wide(bytes)
    }
}

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
#[inline(always)]
unsafe fn sum_words_neon(bytes: &[u8]) -> u32 {
    use core::arch::aarch64::{
        uint32x4_t, vaddvq_u32, vdupq_n_u32, vld1q_u8, vpadalq_u16, vreinterpretq_u16_u8,
        vrev16q_u8,
    };

    // Process 16 bytes per iteration as 8 big-endian u16 lanes.
    //
    // `vrev16q_u8` swaps each adjacent byte pair inside the vector,
    // turning the LE view of memory into the BE u16 view we need:
    //   memory:  [b0 b1 b2 b3 ... b14 b15]
    //   reversed -> reinterpret as u16x8:
    //            [(b0<<8)|b1, (b2<<8)|b3, ..., (b14<<8)|b15]
    //
    // `vpadalq_u16` does pairwise add-long-accumulate: it adds adjacent
    // u16 lanes and adds the resulting u32x4 into the accumulator. So
    // per iteration we issue exactly three NEON ops — load, byte
    // reverse, pair-add — vs five-plus in the swizzle/shift path.
    //
    // The u32x4 lanes can each absorb up to ~2 * 0xFFFF per iteration.
    // The pseudo-header length cap is u16::MAX, so the maximum number
    // of 16-byte iterations is 4096, giving each lane a worst case of
    // about 0x2_0000 * 4096 ≈ 2^30 — well inside u32 headroom.
    //
    // SAFETY (whole-block): caller has guaranteed the `+neon` target
    // feature is active (cfg gate above). All intrinsic calls below
    // touch only the input slice and stay within `bytes.len()`.
    //
    // Two-way unroll keeps two independent vpadalq dependency chains
    // in flight so Apple/Arm cores with multiple NEON pipes do not
    // stall on a single accumulator.
    let mut acc0: uint32x4_t = unsafe { vdupq_n_u32(0) };
    let mut acc1: uint32x4_t = unsafe { vdupq_n_u32(0) };
    let mut offset = 0usize;
    let unroll_limit = bytes.len() & !31;
    while offset < unroll_limit {
        let v0 = unsafe { vld1q_u8(bytes.as_ptr().add(offset)) };
        let v1 = unsafe { vld1q_u8(bytes.as_ptr().add(offset + 16)) };
        let w0 = unsafe { vreinterpretq_u16_u8(vrev16q_u8(v0)) };
        let w1 = unsafe { vreinterpretq_u16_u8(vrev16q_u8(v1)) };
        acc0 = unsafe { vpadalq_u16(acc0, w0) };
        acc1 = unsafe { vpadalq_u16(acc1, w1) };
        offset += 32;
    }
    let limit = bytes.len() & !15;
    while offset < limit {
        let v = unsafe { vld1q_u8(bytes.as_ptr().add(offset)) };
        let words = unsafe { vreinterpretq_u16_u8(vrev16q_u8(v)) };
        acc0 = unsafe { vpadalq_u16(acc0, words) };
        offset += 16;
    }
    let mut sum = unsafe { vaddvq_u32(acc0).wrapping_add(vaddvq_u32(acc1)) };

    let tail = &bytes[offset..];
    let mut chunks = tail.chunks_exact(2);
    for chunk in chunks.by_ref() {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
    }
    if let [last] = chunks.remainder() {
        sum += u32::from(*last) << 8;
    }
    sum
}

#[cfg(not(all(target_arch = "aarch64", target_feature = "neon")))]
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
