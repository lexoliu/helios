use bytes::Bytes;
use divan::counter::{BytesCount, ItemsCount};
use divan::{AllocProfiler, Bencher, black_box};
use helios_netstack::{
    BbrV3, DEFAULT_HOP_LIMIT, DEFAULT_TCP_LISTEN_BACKLOG, EthernetAddress, EthernetFrame,
    EthernetProtocol, IpAddress, IpProtocol, Ipv4Address, Ipv4Cidr, Ipv4Packet,
    MAX_OUTBOUND_FRAMES, NeighborEntry, NeighborState, OutboundBatchStatus, PacketBuffer,
    RxChecksumOffload, RxFrame, RxFrameOffload, Stack, StackConfig, StackInstant,
    TCP_TRANSMIT_BUFFER_BYTES, TcpEndpoint, TcpFlags, TcpHeader, TcpOptions, TcpPacket,
    TcpSegmentBudget, TcpSocket, TcpState, TransportChecksum, UdpEgress, UdpPacket,
    UdpSocketBinding, UdpSocketId, internet_checksum,
};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

const LOCAL_MAC: EthernetAddress = [0x02, 0, 0, 0, 0, 1];
const PEER_MAC: EthernetAddress = [0x02, 0, 0, 0, 0, 2];
const LOCAL_IP: Ipv4Address = Ipv4Address::new([192, 0, 2, 10]);
const PEER_IP: Ipv4Address = Ipv4Address::new([192, 0, 2, 20]);
const MULTICAST_IP: Ipv4Address = Ipv4Address::new([224, 0, 0, 251]);
const UDP_PAYLOAD: &[u8] = b"helios-netstack-divan-udp-payload";
const UDP_LARGE_PAYLOAD: &[u8; 1024] = &[0x5a; 1024];
const TCP_PAYLOAD_BYTES: usize = 1460;
const TCP_RECEIVE_SEGMENTS: usize = 128;
const TCP_ACK_COALESCE_SEGMENTS: usize = 16;
const TCP_ACK_BATCH_SEGMENTS: usize = 32;
const TCP_TRANSMIT_SEGMENTS: usize = 10;

fn main() {
    divan::main();
}

#[divan::bench(args = [20usize, 63, 64, 1500, 4096])]
fn checksum(bencher: Bencher, len: usize) {
    let mut bytes = vec![0u8; len];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = index.wrapping_mul(37).wrapping_add(19) as u8;
    }

    bencher
        .counter(BytesCount::new(len))
        .bench_local(|| internet_checksum(black_box(bytes.as_slice())));
}

#[divan::bench]
fn stack_new(bencher: Bencher) {
    bencher.bench_local(|| Stack::new(StackConfig::new(black_box(LOCAL_MAC), 1514)));
}

fn local_tcp_endpoint() -> TcpEndpoint {
    TcpEndpoint {
        address: IpAddress::Ipv4(LOCAL_IP),
        port: 49152,
    }
}

fn peer_tcp_endpoint() -> TcpEndpoint {
    TcpEndpoint {
        address: IpAddress::Ipv4(PEER_IP),
        port: 80,
    }
}

fn established_tcp_socket() -> TcpSocket<BbrV3> {
    let mut socket = TcpSocket::connect(
        local_tcp_endpoint(),
        peer_tcp_endpoint(),
        7,
        BbrV3::new(1460),
    );
    let _ = socket.on_segment(
        TcpPacket {
            source_port: 80,
            destination_port: 49152,
            sequence: 100,
            acknowledgement: 8,
            flags: TcpFlags::SYN.union(TcpFlags::ACK),
            window_size: u16::MAX,
            options: TcpOptions::empty(),
            payload: &[],
        },
        Bytes::new(),
        1,
    );
    assert_eq!(socket.state(), TcpState::Established);
    socket
}

fn tcp_ipv4_frame(header: TcpHeader, payload: &[u8]) -> PacketBuffer {
    let mut frame = PacketBuffer::new();
    let storage = frame.storage_mut();
    let mut offset =
        EthernetFrame::encode_header(storage, LOCAL_MAC, PEER_MAC, EthernetProtocol::Ipv4)
            .expect("benchmark Ethernet header should fit");
    offset += Ipv4Packet::encode_header(
        &mut storage[offset..],
        PEER_IP,
        LOCAL_IP,
        IpProtocol::Tcp,
        TcpPacket::MIN_HEADER_LEN + payload.len(),
        header.sequence as u16,
        64,
    )
    .expect("benchmark IPv4 header should fit");
    offset += TcpPacket::encode(
        &mut storage[offset..],
        IpAddress::Ipv4(PEER_IP),
        IpAddress::Ipv4(LOCAL_IP),
        header,
        payload,
        TransportChecksum::Software,
    )
    .expect("benchmark TCP packet should fit");
    frame.set_len(offset);
    frame
}

fn udp_ipv4_frame(source_port: u16, destination_port: u16, payload: &[u8]) -> PacketBuffer {
    udp_ipv4_frame_to(LOCAL_IP, source_port, destination_port, payload)
}

fn udp_ipv4_frame_to(
    destination: Ipv4Address,
    source_port: u16,
    destination_port: u16,
    payload: &[u8],
) -> PacketBuffer {
    let mut frame = PacketBuffer::new();
    let storage = frame.storage_mut();
    let mut offset =
        EthernetFrame::encode_header(storage, LOCAL_MAC, PEER_MAC, EthernetProtocol::Ipv4)
            .expect("benchmark Ethernet header should fit");
    offset += Ipv4Packet::encode_header(
        &mut storage[offset..],
        PEER_IP,
        destination,
        IpProtocol::Udp,
        UdpPacket::HEADER_LEN + payload.len(),
        1,
        64,
    )
    .expect("benchmark IPv4 header should fit");
    offset += UdpPacket::encode(
        &mut storage[offset..],
        IpAddress::Ipv4(PEER_IP),
        IpAddress::Ipv4(destination),
        source_port,
        destination_port,
        payload,
        TransportChecksum::Software,
    )
    .expect("benchmark UDP packet should fit");
    frame.set_len(offset);
    frame
}

fn udp_ipv4_frame_bytes(source_port: u16, destination_port: u16, payload: &[u8]) -> Bytes {
    Bytes::copy_from_slice(udp_ipv4_frame(source_port, destination_port, payload).as_slice())
}

fn udp_ipv4_frame_to_bytes(
    destination: Ipv4Address,
    source_port: u16,
    destination_port: u16,
    payload: &[u8],
) -> Bytes {
    Bytes::copy_from_slice(
        udp_ipv4_frame_to(destination, source_port, destination_port, payload).as_slice(),
    )
}

fn udp_ipv4_fragment_frame(
    fragment_offset: usize,
    more_fragments: bool,
    payload: &[u8],
) -> PacketBuffer {
    let mut frame = PacketBuffer::new();
    let storage = frame.storage_mut();
    let mut offset =
        EthernetFrame::encode_header(storage, LOCAL_MAC, PEER_MAC, EthernetProtocol::Ipv4)
            .expect("benchmark Ethernet header should fit");
    let ipv4_header = offset;
    offset += Ipv4Packet::encode_header(
        &mut storage[offset..],
        PEER_IP,
        LOCAL_IP,
        IpProtocol::Udp,
        payload.len(),
        0x1234,
        64,
    )
    .expect("benchmark IPv4 header should fit");
    let fragment_offset_blocks =
        u16::try_from(fragment_offset / 8).expect("benchmark fragment offset fits IPv4 field");
    let fragment = fragment_offset_blocks | if more_fragments { 0x2000 } else { 0 };
    storage[ipv4_header + 6..ipv4_header + 8].copy_from_slice(&fragment.to_be_bytes());
    storage[ipv4_header + 10..ipv4_header + 12].copy_from_slice(&0u16.to_be_bytes());
    let checksum = helios_netstack::ipv4_checksum(
        &storage[ipv4_header..ipv4_header + Ipv4Packet::MIN_HEADER_LEN],
    );
    storage[ipv4_header + 10..ipv4_header + 12].copy_from_slice(&checksum.to_be_bytes());
    storage[offset..offset + payload.len()].copy_from_slice(payload);
    offset += payload.len();
    frame.set_len(offset);
    frame
}

fn udp_ipv4_fragment_frames(payload: &[u8]) -> [Bytes; 2] {
    let mut datagram = [0u8; helios_netstack::ETHERNET_FRAME_BYTES];
    let datagram_len = UdpPacket::encode(
        &mut datagram,
        IpAddress::Ipv4(PEER_IP),
        IpAddress::Ipv4(LOCAL_IP),
        8080,
        49152,
        payload,
        TransportChecksum::Software,
    )
    .expect("benchmark UDP datagram should fit");
    let split_at = 16usize;
    assert!(split_at.is_multiple_of(8));
    [
        Bytes::copy_from_slice(udp_ipv4_fragment_frame(0, true, &datagram[..split_at]).as_slice()),
        Bytes::copy_from_slice(
            udp_ipv4_fragment_frame(split_at, false, &datagram[split_at..datagram_len]).as_slice(),
        ),
    ]
}

fn established_stack() -> Stack {
    let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, 1514));
    stack.add_ipv4_address(Ipv4Cidr::new(LOCAL_IP, 24));
    stack.learn_neighbor(NeighborEntry {
        ip: IpAddress::Ipv4(PEER_IP),
        mac: PEER_MAC,
        state: NeighborState::Reachable,
        updated_at: StackInstant::from_nanos(0),
    });
    stack
        .open_tcp_connect(local_tcp_endpoint(), peer_tcp_endpoint(), 7)
        .expect("benchmark TCP connect should allocate a socket");
    let syn_ack = tcp_ipv4_frame(
        TcpHeader {
            source_port: 80,
            destination_port: 49152,
            sequence: 100,
            acknowledgement: 8,
            flags: TcpFlags::SYN.union(TcpFlags::ACK),
            window_size: u16::MAX,
        },
        &[],
    );
    stack
        .receive_frame(syn_ack.as_slice(), StackInstant::from_nanos(1))
        .expect("benchmark SYN-ACK should establish the socket");
    stack
        .drive_tcp(StackInstant::from_nanos(1))
        .expect("benchmark handshake ACK should queue");
    let status = stack
        .try_submit_outbound_slices(1, |_| Ok::<Option<usize>, ()>(Some(1)))
        .expect("benchmark handshake ACK should submit");
    assert!(matches!(
        status,
        OutboundBatchStatus::Submitted {
            offered: 1,
            accepted: 1,
            ..
        }
    ));
    stack
}

#[divan::bench(args = [1usize, 16, 64])]
fn udp_receive_and_take(bencher: Bencher, packets: usize) {
    let frames: [Bytes; 64] = core::array::from_fn(|index| {
        udp_ipv4_frame_bytes(
            8080 + u16::try_from(index).expect("benchmark UDP source port fits"),
            49152,
            UDP_PAYLOAD,
        )
    });

    bencher.counter(ItemsCount::new(packets)).bench_local(|| {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, 1514));
        stack.add_ipv4_address(Ipv4Cidr::new(LOCAL_IP, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(49152))
            .expect("benchmark UDP socket should bind");
        for (index, frame) in frames.iter().take(packets).enumerate() {
            stack
                .receive_rx_frame(
                    RxFrame::new(black_box(frame.clone())),
                    StackInstant::from_nanos(
                        u64::try_from(index + 1).expect("benchmark timestamp fits"),
                    ),
                )
                .expect("benchmark UDP frame should be accepted");
        }
        for _ in 0..packets {
            let datagram = stack
                .take_udp(socket)
                .expect("benchmark UDP socket should exist")
                .expect("benchmark UDP datagram should be queued");
            black_box(datagram.bytes);
        }
    });
}

#[divan::bench(args = [8usize, 32, 64])]
fn udp_receive_take_tail_port(bencher: Bencher, packets: usize) {
    let frames: [Bytes; 64] = core::array::from_fn(|index| {
        udp_ipv4_frame_bytes(
            8080 + u16::try_from(index).expect("benchmark UDP source port fits"),
            49152 + u16::try_from(index).expect("benchmark UDP destination port fits"),
            UDP_PAYLOAD,
        )
    });

    bencher.counter(ItemsCount::new(packets)).bench_local(|| {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, 1514));
        stack.add_ipv4_address(Ipv4Cidr::new(LOCAL_IP, 24));
        let sockets: [UdpSocketId; 64] = core::array::from_fn(|index| {
            stack
                .open_udp(UdpSocketBinding::wildcard(
                    49152 + u16::try_from(index).expect("benchmark UDP port fits"),
                ))
                .expect("benchmark UDP socket should bind")
        });
        for (index, frame) in frames.iter().take(packets).enumerate() {
            stack
                .receive_rx_frame(
                    RxFrame::new(black_box(frame.clone())),
                    StackInstant::from_nanos(
                        u64::try_from(index + 1).expect("benchmark timestamp fits"),
                    ),
                )
                .expect("benchmark UDP frame should be accepted");
        }
        let datagram = stack
            .take_udp(sockets[packets - 1])
            .expect("benchmark tail UDP socket should exist")
            .expect("benchmark tail UDP datagram should be queued");
        black_box(datagram.bytes);
    });
}

#[divan::bench]
fn udp_receive_large_owned_frame(bencher: Bencher) {
    let frame = udp_ipv4_frame_bytes(8080, 49152, UDP_LARGE_PAYLOAD);

    bencher
        .counter(BytesCount::new(UDP_LARGE_PAYLOAD.len()))
        .bench_local(|| {
            let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, 1514));
            stack.add_ipv4_address(Ipv4Cidr::new(LOCAL_IP, 24));
            let socket = stack
                .open_udp(UdpSocketBinding::wildcard(49152))
                .expect("benchmark UDP socket should bind");
            stack
                .receive_rx_frame(
                    RxFrame::new(black_box(frame.clone())),
                    StackInstant::from_nanos(1),
                )
                .expect("benchmark large UDP frame should be accepted");
            let datagram = stack
                .take_udp(socket)
                .expect("benchmark UDP socket should exist")
                .expect("benchmark large UDP datagram should be queued");
            assert_eq!(datagram.bytes.as_ref(), UDP_LARGE_PAYLOAD.as_slice());
            black_box(datagram.bytes);
        });
}

#[divan::bench]
fn udp_receive_large_owned_frame_rx_checksum_offload(bencher: Bencher) {
    let frame = udp_ipv4_frame_bytes(8080, 49152, UDP_LARGE_PAYLOAD);
    let config = StackConfig::new(LOCAL_MAC, 1514)
        .with_rx_checksum_offload(RxChecksumOffload::new(true, false, true));

    bencher
        .counter(BytesCount::new(UDP_LARGE_PAYLOAD.len()))
        .bench_local(|| {
            let mut stack = Stack::new(black_box(config));
            stack.add_ipv4_address(Ipv4Cidr::new(LOCAL_IP, 24));
            let socket = stack
                .open_udp(UdpSocketBinding::wildcard(49152))
                .expect("benchmark UDP socket should bind");
            stack
                .receive_rx_frame(
                    RxFrame::with_offload(black_box(frame.clone()), RxFrameOffload::validated()),
                    StackInstant::from_nanos(1),
                )
                .expect("benchmark checksum-offloaded large UDP frame should be accepted");
            let datagram = stack
                .take_udp(socket)
                .expect("benchmark UDP socket should exist")
                .expect("benchmark large UDP datagram should be queued");
            assert_eq!(datagram.bytes.as_ref(), UDP_LARGE_PAYLOAD.as_slice());
            black_box(datagram.bytes);
        });
}

#[divan::bench]
fn udp_receive_ipv4_joined_multicast(bencher: Bencher) {
    let frame = udp_ipv4_frame_to_bytes(MULTICAST_IP, 5353, 49152, UDP_PAYLOAD);

    bencher.counter(ItemsCount::new(1usize)).bench_local(|| {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, 1514));
        stack.add_ipv4_address(Ipv4Cidr::new(LOCAL_IP, 24));
        stack
            .join_ipv4_multicast(MULTICAST_IP, LOCAL_IP)
            .expect("benchmark IPv4 multicast group should join");
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(49152))
            .expect("benchmark UDP socket should bind");
        stack
            .receive_rx_frame(
                RxFrame::new(black_box(frame.clone())),
                StackInstant::from_nanos(1),
            )
            .expect("benchmark joined multicast UDP frame should be accepted");
        let datagram = stack
            .take_udp(socket)
            .expect("benchmark UDP socket should exist")
            .expect("benchmark joined multicast datagram should be queued");
        black_box(datagram.bytes);
    });
}

#[divan::bench]
fn udp_receive_ipv4_fragment_reassembly(bencher: Bencher) {
    let fragments = udp_ipv4_fragment_frames(UDP_PAYLOAD);

    bencher.counter(ItemsCount::new(2usize)).bench_local(|| {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, 1514));
        stack.add_ipv4_address(Ipv4Cidr::new(LOCAL_IP, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(49152))
            .expect("benchmark UDP socket should bind");
        for (index, fragment) in fragments.iter().enumerate() {
            stack
                .receive_rx_frame(
                    RxFrame::new(black_box(fragment.clone())),
                    StackInstant::from_nanos(
                        u64::try_from(index + 1).expect("benchmark timestamp fits"),
                    ),
                )
                .expect("benchmark IPv4 fragment should be accepted");
        }
        let datagram = stack
            .take_udp(socket)
            .expect("benchmark UDP socket should exist")
            .expect("benchmark reassembled UDP datagram should be queued");
        black_box(datagram.bytes);
    });
}

#[divan::bench(args = [23 * 1024, 64 * 1024, 128 * 1024])]
fn tcp_receive_contiguous_read(bencher: Bencher, read_size: usize) {
    let payload = vec![0u8; TCP_PAYLOAD_BYTES];
    let target_bytes = TCP_RECEIVE_SEGMENTS * TCP_PAYLOAD_BYTES;

    bencher
        .counter(BytesCount::new(target_bytes))
        .bench_local(|| {
            let mut socket = established_tcp_socket();
            for index in 0..TCP_RECEIVE_SEGMENTS {
                let payload_bytes = Bytes::copy_from_slice(black_box(payload.as_slice()));
                let _ = socket.on_segment(
                    TcpPacket {
                        source_port: 80,
                        destination_port: 49152,
                        sequence: 101
                            + u32::try_from(index * TCP_PAYLOAD_BYTES)
                                .expect("TCP benchmark sequence fits u32"),
                        acknowledgement: 8,
                        flags: TcpFlags::ACK,
                        window_size: u16::MAX,
                        options: TcpOptions::empty(),
                        payload: black_box(payload.as_slice()),
                    },
                    payload_bytes,
                    u64::try_from(index + 2).expect("TCP benchmark timestamp fits u64"),
                );
            }

            let mut received = 0usize;
            while received < target_bytes {
                let bytes = socket
                    .receive(
                        black_box(read_size),
                        u64::try_from(received + target_bytes)
                            .expect("TCP benchmark receive timestamp fits u64"),
                    )
                    .expect("TCP benchmark receive queue should contain data");
                received = received.saturating_add(bytes.len());
            }
            assert_eq!(received, target_bytes);
        });
}

#[divan::bench]
fn tcp_receive_ack_coalesces_unsubmitted_control_frames(bencher: Bencher) {
    let payload = [0u8; TCP_PAYLOAD_BYTES];
    let frames: [PacketBuffer; TCP_ACK_COALESCE_SEGMENTS] = core::array::from_fn(|index| {
        tcp_ipv4_frame(
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 101
                    + u32::try_from(index * TCP_PAYLOAD_BYTES)
                        .expect("benchmark TCP sequence fits u32"),
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
            },
            &payload,
        )
    });

    bencher
        .counter(ItemsCount::new(TCP_ACK_COALESCE_SEGMENTS))
        .bench_local(|| {
            let mut stack = established_stack();
            for (index, frame) in frames.iter().enumerate() {
                let now = StackInstant::from_nanos(
                    u64::try_from(index + 2).expect("benchmark timestamp fits u64"),
                );
                stack
                    .receive_frame(black_box(frame.as_slice()), now)
                    .expect("benchmark TCP payload should be accepted");
                stack
                    .drive_tcp(now)
                    .expect("benchmark TCP drive should queue ACKs");
            }
            let status = stack
                .try_submit_outbound_slices(MAX_OUTBOUND_FRAMES, |frames| {
                    black_box(frames);
                    Ok::<Option<usize>, ()>(Some(frames.len()))
                })
                .expect("benchmark outbound submit should succeed");
            match status {
                OutboundBatchStatus::Submitted {
                    offered, accepted, ..
                } => {
                    assert_eq!(offered, 1);
                    assert_eq!(accepted, 1);
                }
                other => panic!("unexpected outbound status: {other:?}"),
            }
        });
}

#[divan::bench(args = [8usize, TCP_ACK_BATCH_SEGMENTS])]
fn tcp_receive_batch_drive_ack_submit(bencher: Bencher, batch: usize) {
    let payload = [0u8; TCP_PAYLOAD_BYTES];
    let frames: [PacketBuffer; TCP_ACK_BATCH_SEGMENTS] = core::array::from_fn(|index| {
        tcp_ipv4_frame(
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 101
                    + u32::try_from(index * TCP_PAYLOAD_BYTES)
                        .expect("benchmark TCP sequence fits u32"),
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
            },
            &payload,
        )
    });

    bencher.counter(ItemsCount::new(batch)).bench_local(|| {
        let mut stack = established_stack();
        for (index, frame) in frames.iter().take(batch).enumerate() {
            let now = StackInstant::from_nanos(
                u64::try_from(index + 2).expect("benchmark timestamp fits u64"),
            );
            stack
                .receive_frame(black_box(frame.as_slice()), now)
                .expect("benchmark TCP payload should be accepted");
        }
        stack
            .drive_tcp(StackInstant::from_nanos(
                u64::try_from(batch + 2).expect("benchmark timestamp fits u64"),
            ))
            .expect("benchmark TCP drive should queue one ACK");
        let status = stack
            .try_submit_outbound_slices(MAX_OUTBOUND_FRAMES, |frames| {
                black_box(frames);
                Ok::<Option<usize>, ()>(Some(frames.len()))
            })
            .expect("benchmark outbound submit should succeed");
        match status {
            OutboundBatchStatus::Submitted {
                offered, accepted, ..
            } => {
                assert_eq!(offered, 1);
                assert_eq!(accepted, 1);
            }
            other => panic!("unexpected outbound status: {other:?}"),
        }
    });
}

#[divan::bench]
fn tcp_send_bytes_segments_without_copy(bencher: Bencher) {
    let payload = Bytes::from(vec![0u8; TCP_TRANSMIT_SEGMENTS * TCP_PAYLOAD_BYTES]);

    bencher
        .counter(BytesCount::new(payload.len()))
        .bench_local(|| {
            let mut socket = established_tcp_socket();
            let mut remaining = black_box(payload.clone());
            let written = socket.queue_send_bytes(&mut remaining);
            assert_eq!(written, TCP_TRANSMIT_SEGMENTS * TCP_PAYLOAD_BYTES);
            assert!(remaining.is_empty());

            let mut transmitted = 0usize;
            for index in 0..TCP_TRANSMIT_SEGMENTS {
                let segment = socket
                    .take_transmit_segment(
                        u64::try_from(index + 2).expect("TCP benchmark timestamp fits u64"),
                        TcpSegmentBudget::wire_segments(usize::MAX),
                    )
                    .expect("TCP benchmark transmit segment should be queued");
                transmitted = transmitted.saturating_add(segment.payload.len());
            }
            assert_eq!(transmitted, TCP_TRANSMIT_SEGMENTS * TCP_PAYLOAD_BYTES);
        });
}

#[divan::bench]
fn tcp_zero_window_queue_send_bytes(bencher: Bencher) {
    let payload = Bytes::from(vec![0u8; TCP_TRANSMIT_BUFFER_BYTES]);

    bencher
        .counter(BytesCount::new(payload.len()))
        .bench_local(|| {
            let mut socket = established_tcp_socket();
            let _ = socket.on_segment(
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 101,
                    acknowledgement: 8,
                    flags: TcpFlags::ACK,
                    window_size: 0,
                    options: TcpOptions::empty(),
                    payload: &[],
                },
                Bytes::new(),
                2,
            );
            assert_eq!(socket.peer_receive_window(), 0);

            let mut remaining = black_box(payload.clone());
            let written = socket.queue_send_bytes(&mut remaining);
            assert_eq!(written, TCP_TRANSMIT_BUFFER_BYTES);
            assert!(remaining.is_empty());
            assert_eq!(
                socket.take_transmit_segment(3, TcpSegmentBudget::wire_segments(usize::MAX)),
                None
            );
        });
}

#[divan::bench]
fn tcp_zero_window_persist_probe(bencher: Bencher) {
    let payload = Bytes::from_static(b"helios-persist-probe");

    bencher.bench_local(|| {
        let mut socket = established_tcp_socket();
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: 0,
                options: TcpOptions::empty(),
                payload: &[],
            },
            Bytes::new(),
            2,
        );
        let mut remaining = black_box(payload.clone());
        let written = socket.queue_send_bytes(&mut remaining);
        assert_eq!(written, payload.len());
        assert!(
            socket
                .take_transmit_segment(3, TcpSegmentBudget::wire_segments(usize::MAX))
                .is_none()
        );
        let deadline = socket
            .next_deadline_nanos()
            .expect("persist probe deadline should be armed");
        let probe = socket
            .take_transmit_segment(deadline, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("persist deadline should emit a probe");
        assert_eq!(probe.payload.as_ref(), b"h");
        black_box(probe);
    });
}

#[divan::bench]
fn tcp_mark_syn_queued(bencher: Bencher) {
    // Isolates the first in-flight allocation for handshake control traffic.
    // Current baseline is one 2 KiB VecDeque allocation; a boxed-inline queue
    // reduced this to 168 B but regressed the multi-segment ACK path, so the
    // accepted fix needs a real pooled/slab-backed in-flight design.
    bencher.bench_local(|| {
        let mut socket = TcpSocket::connect(
            local_tcp_endpoint(),
            peer_tcp_endpoint(),
            black_box(7),
            BbrV3::new(1460),
        );
        socket.mark_syn_queued(1);
        black_box(socket.pending_retransmission(u64::MAX));
    });
}

#[divan::bench]
fn tcp_single_segment_transmit(bencher: Bencher) {
    let payload = Bytes::from_static(&[7; TCP_PAYLOAD_BYTES]);

    // Keeps the single-data-segment path visible separately from bulk transmit.
    // The right optimization target is first in-flight state allocation without
    // adding allocations or branch cost to sustained multi-segment streams.
    bencher
        .counter(BytesCount::new(TCP_PAYLOAD_BYTES))
        .bench_local(|| {
            let mut socket = established_tcp_socket();
            let mut remaining = black_box(payload.clone());
            let written = socket.queue_send_bytes(&mut remaining);
            assert_eq!(written, TCP_PAYLOAD_BYTES);
            assert!(remaining.is_empty());
            let segment = socket
                .take_transmit_segment(2, TcpSegmentBudget::wire_segments(usize::MAX))
                .expect("single queued segment should transmit");
            black_box(segment);
        });
}

#[divan::bench]
fn tcp_ack_discards_in_flight_segments(bencher: Bencher) {
    let payload = Bytes::from(vec![0u8; TCP_TRANSMIT_SEGMENTS * TCP_PAYLOAD_BYTES]);

    bencher
        .counter(ItemsCount::new(TCP_TRANSMIT_SEGMENTS))
        .bench_local(|| {
            let mut socket = established_tcp_socket();
            let mut remaining = black_box(payload.clone());
            let written = socket.queue_send_bytes(&mut remaining);
            assert_eq!(written, TCP_TRANSMIT_SEGMENTS * TCP_PAYLOAD_BYTES);
            assert!(remaining.is_empty());

            let mut acknowledgement = 0u32;
            for index in 0..TCP_TRANSMIT_SEGMENTS {
                let segment = socket
                    .take_transmit_segment(
                        u64::try_from(index + 2).expect("TCP benchmark timestamp fits u64"),
                        TcpSegmentBudget::wire_segments(usize::MAX),
                    )
                    .expect("TCP benchmark transmit segment should be queued");
                acknowledgement = segment.header.sequence + segment.sequence_len;
            }

            let _ = socket.on_segment(
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 101,
                    acknowledgement: black_box(acknowledgement),
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                    options: TcpOptions::empty(),
                    payload: &[],
                },
                Bytes::new(),
                u64::try_from(TCP_TRANSMIT_SEGMENTS + 2).expect("TCP benchmark timestamp fits u64"),
            );
            assert_eq!(socket.pending_retransmission(u64::MAX), None);
        });
}

#[divan::bench]
fn tcp_first_listen(bencher: Bencher) {
    bencher.bench_local(|| {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, 1514));
        stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(LOCAL_IP),
                    port: black_box(8080),
                },
                DEFAULT_TCP_LISTEN_BACKLOG,
            )
            .expect("benchmark TCP listen should allocate a socket")
    });
}

#[divan::bench(args = [8usize, 64])]
fn tcp_listen_many(bencher: Bencher, sockets: usize) {
    bencher.counter(ItemsCount::new(sockets)).bench_local(|| {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, 1514));
        for index in 0..sockets {
            stack
                .open_tcp_listen(
                    TcpEndpoint {
                        address: IpAddress::Ipv4(LOCAL_IP),
                        port: 8000
                            + u16::try_from(black_box(index)).expect("benchmark port fits u16"),
                    },
                    DEFAULT_TCP_LISTEN_BACKLOG,
                )
                .expect("benchmark TCP listen should allocate a socket");
        }
    });
}

#[divan::bench]
fn udp_queue_and_immediate_submit(bencher: Bencher) {
    let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, 1514));
    stack.add_ipv4_address(Ipv4Cidr::new(LOCAL_IP, 24));
    stack.learn_neighbor(NeighborEntry {
        ip: IpAddress::Ipv4(PEER_IP),
        mac: PEER_MAC,
        state: NeighborState::Reachable,
        updated_at: StackInstant::from_nanos(0),
    });
    let mut identification = 0u16;

    bencher.counter(ItemsCount::new(1usize)).bench_local(|| {
        stack
            .send_udp_ipv4_from(
                LOCAL_IP,
                PEER_IP,
                UdpEgress {
                    source_port: 49152,
                    destination_port: 8080,
                    payload: black_box(UDP_PAYLOAD),
                    hop_limit: DEFAULT_HOP_LIMIT,
                },
                identification,
                StackInstant::from_nanos(u64::from(identification)),
            )
            .expect("UDP frame should queue with a known neighbor");
        identification = identification.wrapping_add(1);
        let status = stack
            .try_submit_outbound_slices(1, |frames| {
                assert_eq!(frames.len(), 1, "benchmark submits one queued frame");
                black_box(&frames[0]);
                Ok::<Option<usize>, ()>(Some(1))
            })
            .expect("outbound immediate submit should succeed");
        match status {
            OutboundBatchStatus::Submitted {
                offered,
                accepted,
                accepted_bytes,
            } => {
                assert_eq!(offered, 1);
                assert_eq!(accepted, 1);
                assert!(accepted_bytes > UDP_PAYLOAD.len());
            }
            other => panic!("unexpected outbound status: {other:?}"),
        }
    });
}

#[divan::bench(args = [8usize, MAX_OUTBOUND_FRAMES])]
fn udp_queue_and_immediate_submit_batch(bencher: Bencher, batch: usize) {
    let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, 1514));
    stack.add_ipv4_address(Ipv4Cidr::new(LOCAL_IP, 24));
    stack.learn_neighbor(NeighborEntry {
        ip: IpAddress::Ipv4(PEER_IP),
        mac: PEER_MAC,
        state: NeighborState::Reachable,
        updated_at: StackInstant::from_nanos(0),
    });
    let mut identification = 0u16;

    bencher.counter(ItemsCount::new(batch)).bench_local(|| {
        for _ in 0..batch {
            stack
                .send_udp_ipv4_from(
                    LOCAL_IP,
                    PEER_IP,
                    UdpEgress {
                        source_port: 49152,
                        destination_port: 8080,
                        payload: black_box(UDP_PAYLOAD),
                        hop_limit: DEFAULT_HOP_LIMIT,
                    },
                    identification,
                    StackInstant::from_nanos(u64::from(identification)),
                )
                .expect("UDP frame should queue with a known neighbor");
            identification = identification.wrapping_add(1);
        }
        let status = stack
            .try_submit_outbound_slices(batch, |frames| {
                assert_eq!(frames.len(), batch, "benchmark submits the queued batch");
                black_box(frames);
                Ok::<Option<usize>, ()>(Some(frames.len()))
            })
            .expect("outbound immediate submit should succeed");
        match status {
            OutboundBatchStatus::Submitted {
                offered,
                accepted,
                accepted_bytes,
            } => {
                assert_eq!(offered, batch);
                assert_eq!(accepted, batch);
                assert!(accepted_bytes > UDP_PAYLOAD.len() * batch);
            }
            other => panic!("unexpected outbound status: {other:?}"),
        }
    });
}
