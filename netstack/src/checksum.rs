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

pub fn udp_checksum_valid(
    source: crate::IpAddress,
    destination: crate::IpAddress,
    datagram: &[u8],
) -> bool {
    match (source, destination) {
        (crate::IpAddress::Ipv4(_), crate::IpAddress::Ipv4(_))
            if udp_checksum_field(datagram) == 0 =>
        {
            true
        }
        (crate::IpAddress::Ipv6(_), crate::IpAddress::Ipv6(_))
            if udp_checksum_field(datagram) == 0 =>
        {
            false
        }
        _ => udp_checksum(source, destination, datagram) == 0,
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

pub fn tcp_checksum_valid(
    source: crate::IpAddress,
    destination: crate::IpAddress,
    segment: &[u8],
) -> bool {
    match (source, destination) {
        (crate::IpAddress::Ipv4(source), crate::IpAddress::Ipv4(destination)) => {
            tcpv4_checksum(source, destination, segment) == 0
        }
        (crate::IpAddress::Ipv6(source), crate::IpAddress::Ipv6(destination)) => {
            tcpv6_checksum(source, destination, segment) == 0
        }
        _ => panic!("TCP pseudo-header address families must match"),
    }
}

/// Folded, uninverted pseudo-header sum seeded into the TCP checksum
/// field when transmit checksum offload is negotiated: the device
/// continues the one's-complement sum from `csum_start` and stores the
/// final complement at `csum_offset` (virtio 1.2 §5.1.6.2).
pub fn tcp_pseudo_header_checksum(
    source: crate::IpAddress,
    destination: crate::IpAddress,
    segment_len: usize,
) -> u16 {
    match (source, destination) {
        (crate::IpAddress::Ipv4(source), crate::IpAddress::Ipv4(destination)) => fold_sum(
            ipv4_pseudo_sum(source, destination, IpProtocol::Tcp, segment_len),
        ),
        (crate::IpAddress::Ipv6(source), crate::IpAddress::Ipv6(destination)) => fold_sum(
            ipv6_pseudo_sum(source, destination, IpProtocol::Tcp, segment_len),
        ),
        _ => panic!("TCP pseudo-header address families must match"),
    }
}

/// Folded, uninverted pseudo-header sum for offloaded UDP transmit
/// checksums; see [`tcp_pseudo_header_checksum`].
pub fn udp_pseudo_header_checksum(
    source: crate::IpAddress,
    destination: crate::IpAddress,
    datagram_len: usize,
) -> u16 {
    match (source, destination) {
        (crate::IpAddress::Ipv4(source), crate::IpAddress::Ipv4(destination)) => fold_sum(
            ipv4_pseudo_sum(source, destination, IpProtocol::Udp, datagram_len),
        ),
        (crate::IpAddress::Ipv6(source), crate::IpAddress::Ipv6(destination)) => fold_sum(
            ipv6_pseudo_sum(source, destination, IpProtocol::Udp, datagram_len),
        ),
        _ => panic!("UDP pseudo-header address families must match"),
    }
}

/// Completes a partial transport checksum a device handed over
/// unfinished, and reports whether the completion is consistent with the
/// packet the stack parsed.
///
/// A frame delivered with virtio's `VIRTIO_NET_HDR_F_NEEDS_CSUM` carries
/// no finished checksum: the sender stopped after the pseudo-header sum
/// and left it in the checksum field, expecting whoever takes the frame
/// off the wire to fold the rest of the segment into it. Completing it
/// means storing `C = !fold(sum(segment))` — the sum runs over the field
/// itself, which holds the partial sum `F` — and verifying the result
/// then asks for `fold(P + sum(segment with C in place)) == !0` for the
/// pseudo-header sum `P` this stack computes from the parsed addresses,
/// protocol and length. In one's-complement arithmetic that reduces
/// exactly to `F == P`, because `sum(segment) + C ≡ !0` holds by
/// construction of `C`: completing the checksum can never disagree with
/// itself, and the only thing left to check is whether the sender
/// checksummed the packet the stack is about to deliver.
///
/// So this is that completion, evaluated in closed form. A frame that
/// fails it is one whose length, addresses or protocol do not match what
/// the sender summed, and the caller drops it exactly as it would drop a
/// frame with a bad software checksum.
pub fn partial_transport_checksum_completes(
    source: crate::IpAddress,
    destination: crate::IpAddress,
    protocol: IpProtocol,
    segment: &[u8],
    field_offset: usize,
) -> bool {
    let Some(field) = segment
        .get(field_offset..field_offset + 2)
        .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
    else {
        return false;
    };
    let pseudo = match (source, destination) {
        (crate::IpAddress::Ipv4(source), crate::IpAddress::Ipv4(destination)) => fold_sum(
            ipv4_pseudo_sum(source, destination, protocol, segment.len()),
        ),
        (crate::IpAddress::Ipv6(source), crate::IpAddress::Ipv6(destination)) => fold_sum(
            ipv6_pseudo_sum(source, destination, protocol, segment.len()),
        ),
        _ => return false,
    };
    // 0x0000 and 0xffff are the same value in one's-complement
    // arithmetic, so compare the difference against zero rather than the
    // two representations against each other.
    matches!(
        fold_sum(u32::from(field) + u32::from(!pseudo)),
        0 | u16::MAX
    )
}

pub fn icmpv6_checksum(source: Ipv6Address, destination: Ipv6Address, message: &[u8]) -> u16 {
    finish_checksum(
        ipv6_pseudo_sum(source, destination, IpProtocol::Icmpv6, message.len())
            + sum_words(message),
    )
}

pub fn icmpv6_checksum_valid(
    source: Ipv6Address,
    destination: Ipv6Address,
    message: &[u8],
) -> bool {
    icmpv6_checksum(source, destination, message) == 0
}

fn udp_checksum_field(datagram: &[u8]) -> u16 {
    if datagram.len() < 8 {
        return 0;
    }
    u16::from_be_bytes([datagram[6], datagram[7]])
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

fn finish_checksum(sum: u32) -> u16 {
    !fold_sum(sum)
}

/// Folds a 32-bit one's-complement accumulator to 16 bits without the
/// final complement, as stored for offloaded transmit checksums.
fn fold_sum(mut sum: u32) -> u16 {
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16
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

    /// The closed form
    /// [`super::partial_transport_checksum_completes`] evaluates must
    /// agree with actually finishing the checksum: a segment whose
    /// checksum field holds the pseudo-header sum becomes a segment with
    /// a valid checksum once the sum over the segment is folded in, and
    /// only then.
    #[test]
    fn completing_a_partial_checksum_produces_a_valid_segment() {
        use crate::{IpAddress, IpProtocol, Ipv4Address};

        let source = Ipv4Address::new([192, 0, 2, 20]);
        let destination = Ipv4Address::new([192, 0, 2, 10]);
        let mut segment = [0u8; 64];
        for (index, byte) in segment.iter_mut().enumerate() {
            *byte = index.wrapping_mul(11).wrapping_add(3) as u8;
        }
        let partial = super::tcp_pseudo_header_checksum(
            IpAddress::Ipv4(source),
            IpAddress::Ipv4(destination),
            segment.len(),
        );
        segment[16..18].copy_from_slice(&partial.to_be_bytes());

        assert!(super::partial_transport_checksum_completes(
            IpAddress::Ipv4(source),
            IpAddress::Ipv4(destination),
            IpProtocol::Tcp,
            &segment,
            16,
        ));

        // Finish the checksum the way the device asked and the segment
        // verifies in software.
        let completed = super::finish_checksum(super::sum_words(&segment));
        segment[16..18].copy_from_slice(&completed.to_be_bytes());
        assert!(super::tcp_checksum_valid(
            IpAddress::Ipv4(source),
            IpAddress::Ipv4(destination),
            &segment,
        ));
    }

    #[test]
    fn a_partial_checksum_for_a_different_packet_does_not_complete() {
        use crate::{IpAddress, IpProtocol, Ipv4Address};

        let source = Ipv4Address::new([192, 0, 2, 20]);
        let destination = Ipv4Address::new([192, 0, 2, 10]);
        let mut segment = [0u8; 64];
        let partial = super::tcp_pseudo_header_checksum(
            IpAddress::Ipv4(source),
            IpAddress::Ipv4(destination),
            segment.len() + 16,
        );
        segment[16..18].copy_from_slice(&partial.to_be_bytes());

        assert!(!super::partial_transport_checksum_completes(
            IpAddress::Ipv4(source),
            IpAddress::Ipv4(destination),
            IpProtocol::Tcp,
            &segment,
            16,
        ));
    }

    #[test]
    fn a_partial_checksum_field_outside_the_segment_does_not_complete() {
        use crate::{IpAddress, IpProtocol, Ipv4Address};

        let source = Ipv4Address::new([192, 0, 2, 20]);
        let destination = Ipv4Address::new([192, 0, 2, 10]);
        let segment = [0u8; 8];

        assert!(!super::partial_transport_checksum_completes(
            IpAddress::Ipv4(source),
            IpAddress::Ipv4(destination),
            IpProtocol::Tcp,
            &segment,
            16,
        ));
    }
}
