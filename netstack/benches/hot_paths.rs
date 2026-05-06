use bytes::Bytes;
use divan::counter::{BytesCount, ItemsCount};
use divan::{AllocProfiler, Bencher, black_box};
use helios_netstack::{
    BbrV3, EthernetAddress, EthernetFrame, EthernetProtocol, IpAddress, IpProtocol, Ipv4Address,
    Ipv4Cidr, Ipv4Packet, MAX_OUTBOUND_FRAMES, NeighborEntry, NeighborState, OutboundBatchStatus,
    PacketBuffer, Stack, StackConfig, StackInstant, TcpEndpoint, TcpFlags, TcpHeader, TcpOptions,
    TcpPacket, TcpSocket, TcpState, internet_checksum,
};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

const LOCAL_MAC: EthernetAddress = [0x02, 0, 0, 0, 0, 1];
const PEER_MAC: EthernetAddress = [0x02, 0, 0, 0, 0, 2];
const LOCAL_IP: Ipv4Address = Ipv4Address::new([192, 0, 2, 10]);
const PEER_IP: Ipv4Address = Ipv4Address::new([192, 0, 2, 20]);
const UDP_PAYLOAD: &[u8] = b"helios-netstack-divan-udp-payload";
const TCP_PAYLOAD_BYTES: usize = 1460;
const TCP_RECEIVE_SEGMENTS: usize = 128;
const TCP_ACK_COALESCE_SEGMENTS: usize = 16;
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
    )
    .expect("benchmark TCP packet should fit");
    frame.set_len(offset);
    frame
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
    stack.open_tcp_connect(local_tcp_endpoint(), peer_tcp_endpoint(), 7);
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

#[divan::bench(args = [23 * 1024, 64 * 1024, 128 * 1024])]
fn tcp_receive_contiguous_read(bencher: Bencher, read_size: usize) {
    let payload = vec![0u8; TCP_PAYLOAD_BYTES];
    let target_bytes = TCP_RECEIVE_SEGMENTS * TCP_PAYLOAD_BYTES;

    bencher
        .counter(BytesCount::new(target_bytes))
        .bench_local(|| {
            let mut socket = established_tcp_socket();
            for index in 0..TCP_RECEIVE_SEGMENTS {
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
                    )
                    .expect("TCP benchmark transmit segment should be queued");
                transmitted = transmitted.saturating_add(segment.payload.len());
            }
            assert_eq!(transmitted, TCP_TRANSMIT_SEGMENTS * TCP_PAYLOAD_BYTES);
        });
}

#[divan::bench]
fn tcp_first_listen(bencher: Bencher) {
    bencher.bench_local(|| {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, 1514));
        stack.open_tcp_listen(TcpEndpoint {
            address: IpAddress::Ipv4(LOCAL_IP),
            port: black_box(8080),
        })
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
                49152,
                PEER_IP,
                8080,
                black_box(UDP_PAYLOAD),
                identification,
                StackInstant::from_nanos(u64::from(identification)),
            )
            .expect("UDP frame should queue with a known neighbor");
        identification = identification.wrapping_add(1);
        let status = stack
            .try_submit_outbound_slices(1, |frames| {
                assert_eq!(frames.len(), 1, "benchmark submits one queued frame");
                black_box(frames[0]);
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
                    49152,
                    PEER_IP,
                    8080,
                    black_box(UDP_PAYLOAD),
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
