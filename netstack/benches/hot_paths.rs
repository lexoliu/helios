use divan::counter::{BytesCount, ItemsCount};
use divan::{AllocProfiler, Bencher, black_box};
use helios_netstack::{
    EthernetAddress, IpAddress, Ipv4Address, Ipv4Cidr, MAX_OUTBOUND_FRAMES, NeighborEntry,
    NeighborState, OutboundBatchStatus, Stack, StackConfig, StackInstant, TcpEndpoint,
    internet_checksum,
};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

const LOCAL_MAC: EthernetAddress = [0x02, 0, 0, 0, 0, 1];
const PEER_MAC: EthernetAddress = [0x02, 0, 0, 0, 0, 2];
const LOCAL_IP: Ipv4Address = Ipv4Address::new([192, 0, 2, 10]);
const PEER_IP: Ipv4Address = Ipv4Address::new([192, 0, 2, 20]);
const UDP_PAYLOAD: &[u8] = b"helios-netstack-divan-udp-payload";

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
