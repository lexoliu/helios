extern crate alloc;

use bytes::Bytes;
use divan::counter::ItemsCount;
use divan::{AllocProfiler, Bencher, black_box};
use helios_hal::fs::FileRights;
use helios_hal::resource::KernelResource;
use helios_kernel::{
    ComponentHostNetworkService, ComponentNetworkService, DescriptorId, DescriptorTable, DnsError,
    DnsErrorKind, FutexKey, FutexTable, GuestAddress, Ipv4Address, Ipv4Cidr, Ipv4Route, MacAddress,
    NetworkAdminBackend, NetworkBridgeRequest, NetworkControlError, NetworkErrorDetail,
    NetworkIpAddress, NetworkPortId, PingError, PingErrorKind, PingReply, ProcessMemoryIdentity,
    TcpAccepted, TcpError, TcpErrorKind, TcpListener, TryRead, UdpBinding, UdpDatagram, UdpError,
    UdpErrorKind, byte_channel,
};
use spin::{Mutex, Once};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

#[derive(Clone)]
struct BenchFile;

#[derive(Clone)]
struct BenchNetworkService {
    payload: Bytes,
}

struct MutexNetworkServiceSlot {
    service: Mutex<Option<ComponentHostNetworkService>>,
}

struct OnceNetworkServiceSlot {
    service: Once<ComponentHostNetworkService>,
}

fn main() {
    divan::main();
}

#[divan::bench(args = [16usize, 64, 256])]
fn descriptor_insert_close_reuse(bencher: Bencher, count: usize) {
    descriptor_insert_close_reuse_with_table(bencher, count, DescriptorTable::new);
}

#[divan::bench(args = [16usize, 64, 256])]
fn descriptor_insert_close_reuse_preallocated(bencher: Bencher, count: usize) {
    descriptor_insert_close_reuse_with_table(bencher, count, || {
        DescriptorTable::with_capacity(count)
    });
}

fn descriptor_insert_close_reuse_with_table(
    bencher: Bencher,
    count: usize,
    new_table: impl Fn() -> DescriptorTable<BenchFile, FileRights>,
) {
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        let mut table = new_table();
        let mut descriptors = [DescriptorId::new(0); 256];
        for index in 0..count {
            descriptors[index] = table
                .insert(file(index as u64), false)
                .expect("descriptor insert should succeed");
        }
        for descriptor in descriptors.into_iter().take(count) {
            black_box(
                table
                    .close(descriptor)
                    .expect("descriptor close should succeed"),
            );
        }
    });
}

#[divan::bench]
fn descriptor_sparse_renumber_reuse(bencher: Bencher) {
    descriptor_sparse_renumber_reuse_with_table(bencher, DescriptorTable::new);
}

#[divan::bench]
fn descriptor_sparse_renumber_reuse_preallocated(bencher: Bencher) {
    descriptor_sparse_renumber_reuse_with_table(bencher, || DescriptorTable::with_capacity(256));
}

fn descriptor_sparse_renumber_reuse_with_table(
    bencher: Bencher,
    new_table: impl Fn() -> DescriptorTable<BenchFile, FileRights>,
) {
    bencher.bench_local(|| {
        let mut table = new_table();
        let descriptor = table
            .insert(file(0), false)
            .expect("descriptor insert should succeed");
        let target = DescriptorId::new(255);
        table
            .renumber(descriptor, target)
            .expect("sparse descriptor renumber should succeed");
        black_box(
            table
                .insert(file(1), false)
                .expect("descriptor insert should reuse sparse free slot"),
        );
    });
}

#[divan::bench(args = [16usize, 64, 256])]
fn byte_channel_write_try_read(bencher: Bencher, count: usize) {
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        let (writer, reader) = byte_channel();
        let payload = Bytes::from_static(b"helios-byte-channel");
        for _ in 0..count {
            writer
                .write(payload.clone())
                .expect("reader is alive during byte channel benchmark");
        }
        for _ in 0..count {
            match reader.try_read() {
                TryRead::Ready(bytes) => {
                    black_box(bytes);
                }
                TryRead::Pending => panic!("byte channel benchmark lost queued bytes"),
                TryRead::Eof => panic!("byte channel benchmark reached EOF before draining bytes"),
            }
        }
    });
}

#[divan::bench]
fn byte_channel_create_drop(bencher: Bencher) {
    bencher.bench_local(|| {
        black_box(byte_channel());
    });
}

#[divan::bench(args = [1usize, 16, 64])]
fn futex_prepare_complete_distinct_keys(bencher: Bencher, count: usize) {
    let memory = ProcessMemoryIdentity::new(1);
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        let mut table = FutexTable::new();
        let mut registrations = [const { None }; 64];
        for (index, registration) in registrations.iter_mut().take(count).enumerate() {
            *registration = Some(table.prepare_wait(FutexKey::new(
                memory,
                GuestAddress::new(u64::try_from(index + 1).expect("futex bench address fits")),
            )));
        }
        for registration in registrations.into_iter().take(count).flatten() {
            table.complete_wait(registration);
        }
    });
}

#[divan::bench(args = [1usize, 64, 1024])]
fn component_network_direct_tcp_read(bencher: Bencher, count: usize) {
    let service = BenchNetworkService {
        payload: Bytes::copy_from_slice(&[7; 4096]),
    };
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        for _ in 0..count {
            let bytes = futures_lite::future::block_on(service.tcp_read(1, 4096, 0))
                .expect("direct benchmark TCP read should succeed")
                .expect("direct benchmark TCP read should return data");
            black_box(bytes);
        }
    });
}

// This captures the current component-host network adapter tax: the typed
// service call is allocation-free, while the erased adapter path boxes one
// future per call. The VM TCP profile uses this path for every typed read.
#[divan::bench(args = [1usize, 64, 1024])]
fn component_network_erased_tcp_read(bencher: Bencher, count: usize) {
    let service = ComponentHostNetworkService::from_service(BenchNetworkService {
        payload: Bytes::copy_from_slice(&[7; 4096]),
    });
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        for _ in 0..count {
            let bytes = futures_lite::future::block_on(service.tcp_read(1, 4096, 0))
                .expect("erased benchmark TCP read should succeed")
                .expect("erased benchmark TCP read should return data");
            black_box(bytes);
        }
    });
}

#[divan::bench(args = [1usize, 64, 1024])]
fn runtime_network_service_mutex_lookup(bencher: Bencher, count: usize) {
    let slot = MutexNetworkServiceSlot::new(component_network_service());
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        for _ in 0..count {
            black_box(slot.get());
        }
    });
}

// This models `RuntimeState::network_service()` after the service has been
// installed. The syscall hot path only needs an immutable service handle, so a
// Once read avoids the old uncontended spin mutex around every network call.
#[divan::bench(args = [1usize, 64, 1024])]
fn runtime_network_service_once_lookup(bencher: Bencher, count: usize) {
    let slot = OnceNetworkServiceSlot::new(component_network_service());
    bencher.counter(ItemsCount::new(count)).bench_local(|| {
        for _ in 0..count {
            black_box(slot.get());
        }
    });
}

fn file(id: u64) -> KernelResource<BenchFile, FileRights> {
    black_box(id);
    let resource = KernelResource::new(BenchFile, FileRights::READ | FileRights::WRITE);
    black_box(resource)
}

fn component_network_service() -> ComponentHostNetworkService {
    ComponentHostNetworkService::from_service(BenchNetworkService {
        payload: Bytes::copy_from_slice(&[7; 4096]),
    })
}

impl MutexNetworkServiceSlot {
    fn new(service: ComponentHostNetworkService) -> Self {
        Self {
            service: Mutex::new(Some(service)),
        }
    }

    fn get(&self) -> Option<ComponentHostNetworkService> {
        self.service.lock().clone()
    }
}

impl OnceNetworkServiceSlot {
    fn new(service: ComponentHostNetworkService) -> Self {
        let slot = Self {
            service: Once::new(),
        };
        slot.service.call_once(|| service);
        slot
    }

    fn get(&self) -> Option<ComponentHostNetworkService> {
        self.service.get().cloned()
    }
}

impl ComponentNetworkService for BenchNetworkService {
    type TcpStream = u64;
    type TcpListener = u64;
    type UdpSocket = u64;

    fn hardware_address(&self) -> [u8; 6] {
        [2, 0, 0, 0, 0, 1]
    }

    async fn ipv4_cidr(&self) -> Option<Ipv4Cidr> {
        None
    }

    async fn ping(&self, _host: &str, _timeout_nanos: u64) -> Result<PingReply, PingError> {
        Err(bench_ping_error())
    }

    async fn dns_resolve(
        &self,
        _host: &str,
        _timeout_nanos: u64,
    ) -> Result<alloc::vec::Vec<Ipv4Address>, DnsError> {
        Err(bench_dns_error())
    }

    async fn tcp_connect(
        &self,
        _host: &str,
        _port: u16,
        _timeout_nanos: u64,
    ) -> Result<Self::TcpStream, TcpError> {
        Ok(1)
    }

    async fn tcp_connect_from(
        &self,
        _host: &str,
        _port: u16,
        _local_port: u16,
        _timeout_nanos: u64,
    ) -> Result<Self::TcpStream, TcpError> {
        Ok(1)
    }

    async fn tcp_connect_address(
        &self,
        _remote_address: NetworkIpAddress,
        _port: u16,
        _local_port: u16,
        _timeout_nanos: u64,
    ) -> Result<Self::TcpStream, TcpError> {
        Ok(1)
    }

    async fn tcp_listen(
        &self,
        _local_address: NetworkIpAddress,
        local_port: u16,
        _backlog: u16,
    ) -> Result<TcpListener<Self::TcpListener>, TcpError> {
        Ok(TcpListener {
            listener: 1,
            local_port,
        })
    }

    async fn tcp_accept(
        &self,
        _listener: Self::TcpListener,
        _timeout_nanos: u64,
    ) -> Result<TcpAccepted<Self::TcpStream>, TcpError> {
        Err(bench_tcp_error())
    }

    async fn tcp_write_all(
        &self,
        _stream: Self::TcpStream,
        _bytes: &[u8],
        _timeout_nanos: u64,
    ) -> Result<(), TcpError> {
        Ok(())
    }

    async fn tcp_write_all_bytes(
        &self,
        _stream: Self::TcpStream,
        _bytes: Bytes,
        _timeout_nanos: u64,
    ) -> Result<(), TcpError> {
        Ok(())
    }

    async fn tcp_read(
        &self,
        _stream: Self::TcpStream,
        _max_bytes: u32,
        _timeout_nanos: u64,
    ) -> Result<Option<Bytes>, TcpError> {
        Ok(Some(self.payload.clone()))
    }

    async fn tcp_shutdown_send(&self, _stream: Self::TcpStream) -> Result<(), TcpError> {
        Ok(())
    }

    async fn tcp_close(&self, _stream: Self::TcpStream) {}

    async fn udp_bind(&self, local_port: u16) -> Result<UdpBinding<Self::UdpSocket>, UdpError> {
        Ok(UdpBinding {
            socket: 1,
            local_port,
        })
    }

    async fn udp_send(
        &self,
        _socket: Self::UdpSocket,
        _host: &str,
        _port: u16,
        bytes: &[u8],
        _timeout_nanos: u64,
    ) -> Result<u64, UdpError> {
        Ok(bytes.len() as u64)
    }

    async fn udp_send_address(
        &self,
        _socket: Self::UdpSocket,
        _remote_address: NetworkIpAddress,
        _port: u16,
        bytes: &[u8],
        _timeout_nanos: u64,
    ) -> Result<u64, UdpError> {
        Ok(bytes.len() as u64)
    }

    async fn udp_receive(
        &self,
        _socket: Self::UdpSocket,
        _max_bytes: u32,
        _timeout_nanos: u64,
    ) -> Result<Option<UdpDatagram>, UdpError> {
        Err(bench_udp_error())
    }

    async fn udp_join_multicast_v4(
        &self,
        _group: Ipv4Address,
        _interface: Ipv4Address,
    ) -> Result<(), UdpError> {
        Ok(())
    }

    async fn udp_leave_multicast_v4(
        &self,
        _group: Ipv4Address,
        _interface: Ipv4Address,
    ) -> Result<(), UdpError> {
        Ok(())
    }

    async fn udp_close(&self, _socket: Self::UdpSocket) {}
}

impl NetworkAdminBackend for BenchNetworkService {
    async fn bridge_port(
        &self,
        _port: NetworkPortId,
        _bridge: NetworkBridgeRequest,
    ) -> Result<(), NetworkControlError> {
        Err(NetworkControlError::BackendFault)
    }

    async fn unbridge_port(&self, _port: NetworkPortId) -> Result<(), NetworkControlError> {
        Err(NetworkControlError::BackendFault)
    }

    async fn acquire_dhcp(&self, _port: NetworkPortId) -> Result<Ipv4Cidr, NetworkControlError> {
        Err(NetworkControlError::BackendFault)
    }

    async fn add_address(
        &self,
        _port: NetworkPortId,
        _address: Ipv4Cidr,
    ) -> Result<(), NetworkControlError> {
        Err(NetworkControlError::BackendFault)
    }

    async fn remove_address(
        &self,
        _port: NetworkPortId,
        _address: Ipv4Cidr,
    ) -> Result<(), NetworkControlError> {
        Err(NetworkControlError::BackendFault)
    }

    async fn clear_addresses(&self, _port: NetworkPortId) -> Result<(), NetworkControlError> {
        Err(NetworkControlError::BackendFault)
    }

    async fn list_addresses(
        &self,
        _port: NetworkPortId,
    ) -> Result<alloc::vec::Vec<Ipv4Cidr>, NetworkControlError> {
        Err(NetworkControlError::BackendFault)
    }

    async fn mac_address(&self, _port: NetworkPortId) -> Result<MacAddress, NetworkControlError> {
        Err(NetworkControlError::BackendFault)
    }

    async fn set_gateway(
        &self,
        _port: NetworkPortId,
        _gateway: Ipv4Address,
    ) -> Result<(), NetworkControlError> {
        Err(NetworkControlError::BackendFault)
    }

    async fn add_route(
        &self,
        _port: NetworkPortId,
        _route: Ipv4Route,
    ) -> Result<(), NetworkControlError> {
        Err(NetworkControlError::BackendFault)
    }

    async fn remove_route(
        &self,
        _port: NetworkPortId,
        _route: Ipv4Route,
    ) -> Result<(), NetworkControlError> {
        Err(NetworkControlError::BackendFault)
    }

    async fn clear_routes(&self, _port: NetworkPortId) -> Result<(), NetworkControlError> {
        Err(NetworkControlError::BackendFault)
    }

    async fn list_routes(
        &self,
        _port: NetworkPortId,
    ) -> Result<alloc::vec::Vec<Ipv4Route>, NetworkControlError> {
        Err(NetworkControlError::BackendFault)
    }
}

fn bench_ping_error() -> PingError {
    PingError {
        kind: PingErrorKind::Unavailable,
        detail: NetworkErrorDetail::NetworkServiceUnavailable,
    }
}

fn bench_dns_error() -> DnsError {
    DnsError {
        kind: DnsErrorKind::Unavailable,
        detail: NetworkErrorDetail::NetworkServiceUnavailable,
    }
}

fn bench_tcp_error() -> TcpError {
    TcpError {
        kind: TcpErrorKind::Unavailable,
        detail: NetworkErrorDetail::NetworkServiceUnavailable,
    }
}

fn bench_udp_error() -> UdpError {
    UdpError {
        kind: UdpErrorKind::Unavailable,
        detail: NetworkErrorDetail::NetworkServiceUnavailable,
    }
}
