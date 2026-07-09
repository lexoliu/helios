extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use bytes::Bytes;
use triomphe::Arc;
use unsize::{CoerceUnsize, Coercion};

use crate::{
    ComponentNetworkService, DnsError, Ipv4Address, Ipv4Cidr, Ipv4Route, MacAddress,
    NetworkAdminBackend, NetworkBridgeRequest, NetworkControlError, NetworkIpAddress,
    NetworkPortId, PingError, PingReply, RegisteredTcpReadBuffer, TcpAccepted, TcpError,
    TcpListener, UdpBinding, UdpDatagram, UdpError,
};

pub trait ComponentHostTcpStreamToken: Copy + Send + 'static {
    fn into_raw(self) -> u64;

    fn from_raw(raw: u64) -> Self;
}

impl ComponentHostTcpStreamToken for u64 {
    fn into_raw(self) -> u64 {
        self
    }

    fn from_raw(raw: u64) -> Self {
        raw
    }
}

pub trait ComponentHostTcpListenerToken: Copy + Send + 'static {
    fn into_raw(self) -> u64;

    fn from_raw(raw: u64) -> Self;
}

impl ComponentHostTcpListenerToken for u64 {
    fn into_raw(self) -> u64 {
        self
    }

    fn from_raw(raw: u64) -> Self {
        raw
    }
}

pub trait ComponentHostUdpSocketToken: Copy + Send + 'static {
    fn into_raw(self) -> u64;

    fn from_raw(raw: u64) -> Self;
}

impl ComponentHostUdpSocketToken for u64 {
    fn into_raw(self) -> u64 {
        self
    }

    fn from_raw(raw: u64) -> Self {
        raw
    }
}

trait DynComponentHostNetworkService: Send + Sync + 'static {
    fn hardware_address(&self) -> [u8; 6];

    fn ipv4_cidr(&self) -> Pin<Box<dyn Future<Output = Option<crate::Ipv4Cidr>> + Send + '_>>;

    fn ping<'a>(
        &'a self,
        host: &'a str,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<PingReply, PingError>> + Send + 'a>>;

    fn dns_resolve<'a>(
        &'a self,
        host: &'a str,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Ipv4Address>, DnsError>> + Send + 'a>>;

    fn tcp_connect<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, TcpError>> + Send + 'a>>;

    fn tcp_connect_from<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        local_port: u16,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, TcpError>> + Send + 'a>>;

    fn tcp_connect_address<'a>(
        &'a self,
        remote_address: NetworkIpAddress,
        port: u16,
        local_port: u16,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, TcpError>> + Send + 'a>>;

    fn tcp_listen<'a>(
        &'a self,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: u16,
    ) -> Pin<Box<dyn Future<Output = Result<TcpListener<u64>, TcpError>> + Send + 'a>>;

    fn tcp_accept<'a>(
        &'a self,
        listener: u64,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<TcpAccepted<u64>, TcpError>> + Send + 'a>>;

    fn tcp_write_all<'a>(
        &'a self,
        stream: u64,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), TcpError>> + Send + 'a>>;

    fn tcp_write_all_bytes<'a>(
        &'a self,
        stream: u64,
        bytes: Bytes,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), TcpError>> + Send + 'a>>;

    fn tcp_read<'a>(
        &'a self,
        stream: u64,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Bytes>, TcpError>> + Send + 'a>>;

    fn tcp_read_into_registered<'a>(
        &'a self,
        stream: u64,
        buffer: RegisteredTcpReadBuffer<'a>,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<usize>, TcpError>> + Send + 'a>>;

    fn tcp_shutdown_send<'a>(
        &'a self,
        stream: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), TcpError>> + Send + 'a>>;

    fn tcp_close<'a>(&'a self, stream: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    fn udp_bind<'a>(
        &'a self,
        local_port: u16,
    ) -> Pin<Box<dyn Future<Output = Result<UdpBinding<u64>, UdpError>> + Send + 'a>>;

    fn udp_connect<'a>(
        &'a self,
        socket: u64,
        remote_address: NetworkIpAddress,
        port: u16,
    ) -> Result<(), UdpError>;

    fn udp_disconnect(&self, socket: u64) -> Result<(), UdpError>;

    fn udp_send<'a>(
        &'a self,
        socket: u64,
        host: &'a str,
        port: u16,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, UdpError>> + Send + 'a>>;

    fn udp_send_address<'a>(
        &'a self,
        socket: u64,
        remote_address: NetworkIpAddress,
        port: u16,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, UdpError>> + Send + 'a>>;

    fn udp_receive<'a>(
        &'a self,
        socket: u64,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<UdpDatagram>, UdpError>> + Send + 'a>>;

    fn udp_join_multicast_v4<'a>(
        &'a self,
        group: Ipv4Address,
        interface: Ipv4Address,
    ) -> Pin<Box<dyn Future<Output = Result<(), UdpError>> + Send + 'a>>;

    fn udp_leave_multicast_v4<'a>(
        &'a self,
        group: Ipv4Address,
        interface: Ipv4Address,
    ) -> Pin<Box<dyn Future<Output = Result<(), UdpError>> + Send + 'a>>;

    fn udp_close<'a>(&'a self, socket: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    fn bridge_port<'a>(
        &'a self,
        port: NetworkPortId,
        bridge: NetworkBridgeRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>>;

    fn unbridge_port<'a>(
        &'a self,
        port: NetworkPortId,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>>;

    fn acquire_dhcp<'a>(
        &'a self,
        port: NetworkPortId,
    ) -> Pin<Box<dyn Future<Output = Result<Ipv4Cidr, NetworkControlError>> + Send + 'a>>;

    fn add_address<'a>(
        &'a self,
        port: NetworkPortId,
        address: Ipv4Cidr,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>>;

    fn remove_address<'a>(
        &'a self,
        port: NetworkPortId,
        address: Ipv4Cidr,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>>;

    fn clear_addresses<'a>(
        &'a self,
        port: NetworkPortId,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>>;

    fn list_addresses<'a>(
        &'a self,
        port: NetworkPortId,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Ipv4Cidr>, NetworkControlError>> + Send + 'a>>;

    fn mac_address<'a>(
        &'a self,
        port: NetworkPortId,
    ) -> Pin<Box<dyn Future<Output = Result<MacAddress, NetworkControlError>> + Send + 'a>>;

    fn set_gateway<'a>(
        &'a self,
        port: NetworkPortId,
        gateway: Ipv4Address,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>>;

    fn add_route<'a>(
        &'a self,
        port: NetworkPortId,
        route: Ipv4Route,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>>;

    fn remove_route<'a>(
        &'a self,
        port: NetworkPortId,
        route: Ipv4Route,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>>;

    fn clear_routes<'a>(
        &'a self,
        port: NetworkPortId,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>>;

    fn list_routes<'a>(
        &'a self,
        port: NetworkPortId,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Ipv4Route>, NetworkControlError>> + Send + 'a>>;
}

#[derive(Clone)]
pub struct ComponentHostNetworkService {
    inner: Arc<dyn DynComponentHostNetworkService>,
}

impl ComponentHostNetworkService {
    pub fn from_service<Service>(service: Service) -> Self
    where
        Service: ComponentNetworkService + NetworkAdminBackend + Sync,
        Service::TcpStream: ComponentHostTcpStreamToken,
        Service::TcpListener: ComponentHostTcpListenerToken,
        Service::UdpSocket: ComponentHostUdpSocketToken,
    {
        let coercion = unsafe {
            // SAFETY: the closure performs only Rust's built-in unsize coercion
            // from the concrete adapter to the same allocation's trait-object view.
            Coercion::new(|service: *const TypedNetworkService<Service>| {
                service as *const dyn DynComponentHostNetworkService
            })
        };
        let inner = Arc::new(TypedNetworkService { service }).unsize(coercion);
        Self { inner }
    }

    pub fn tcp_read_into_registered<'a>(
        &'a self,
        stream: u64,
        buffer: RegisteredTcpReadBuffer<'a>,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Option<usize>, TcpError>> + Send + 'a {
        self.inner
            .tcp_read_into_registered(stream, buffer, timeout_nanos)
    }
}

impl ComponentNetworkService for ComponentHostNetworkService {
    type TcpStream = u64;
    type TcpListener = u64;
    type UdpSocket = u64;

    fn hardware_address(&self) -> [u8; 6] {
        self.inner.hardware_address()
    }

    fn ipv4_cidr(&self) -> impl Future<Output = Option<crate::Ipv4Cidr>> + Send + '_ {
        self.inner.ipv4_cidr()
    }

    fn ping<'a>(
        &'a self,
        host: &'a str,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<PingReply, PingError>> + Send + 'a {
        self.inner.ping(host, timeout_nanos)
    }

    fn dns_resolve<'a>(
        &'a self,
        host: &'a str,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Vec<Ipv4Address>, DnsError>> + Send + 'a {
        self.inner.dns_resolve(host, timeout_nanos)
    }

    fn tcp_connect<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Self::TcpStream, TcpError>> + Send + 'a {
        self.inner.tcp_connect(host, port, timeout_nanos)
    }

    fn tcp_connect_from<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        local_port: u16,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Self::TcpStream, TcpError>> + Send + 'a {
        self.inner
            .tcp_connect_from(host, port, local_port, timeout_nanos)
    }

    fn tcp_connect_address(
        &self,
        remote_address: NetworkIpAddress,
        port: u16,
        local_port: u16,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Self::TcpStream, TcpError>> + Send + '_ {
        self.inner
            .tcp_connect_address(remote_address, port, local_port, timeout_nanos)
    }

    fn tcp_listen(
        &self,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: u16,
    ) -> impl Future<Output = Result<TcpListener<Self::TcpListener>, TcpError>> + Send + '_ {
        self.inner.tcp_listen(local_address, local_port, backlog)
    }

    fn tcp_accept(
        &self,
        listener: Self::TcpListener,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<TcpAccepted<Self::TcpStream>, TcpError>> + Send + '_ {
        self.inner.tcp_accept(listener, timeout_nanos)
    }

    fn tcp_write_all<'a>(
        &'a self,
        stream: Self::TcpStream,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<(), TcpError>> + Send + 'a {
        self.inner.tcp_write_all(stream, bytes, timeout_nanos)
    }

    fn tcp_write_all_bytes<'a>(
        &'a self,
        stream: Self::TcpStream,
        bytes: Bytes,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<(), TcpError>> + Send + 'a {
        self.inner.tcp_write_all_bytes(stream, bytes, timeout_nanos)
    }

    fn tcp_read(
        &self,
        stream: Self::TcpStream,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Option<Bytes>, TcpError>> + Send + '_ {
        self.inner.tcp_read(stream, max_bytes, timeout_nanos)
    }

    fn tcp_shutdown_send(
        &self,
        stream: Self::TcpStream,
    ) -> impl Future<Output = Result<(), TcpError>> + Send + '_ {
        self.inner.tcp_shutdown_send(stream)
    }

    fn tcp_close(&self, stream: Self::TcpStream) -> impl Future<Output = ()> + Send + '_ {
        self.inner.tcp_close(stream)
    }

    fn udp_bind(
        &self,
        local_port: u16,
    ) -> impl Future<Output = Result<UdpBinding<Self::UdpSocket>, UdpError>> + Send + '_ {
        self.inner.udp_bind(local_port)
    }

    fn udp_connect(
        &self,
        socket: Self::UdpSocket,
        remote_address: NetworkIpAddress,
        port: u16,
    ) -> Result<(), UdpError> {
        self.inner.udp_connect(socket, remote_address, port)
    }

    fn udp_disconnect(&self, socket: Self::UdpSocket) -> Result<(), UdpError> {
        self.inner.udp_disconnect(socket)
    }

    fn udp_send<'a>(
        &'a self,
        socket: Self::UdpSocket,
        host: &'a str,
        port: u16,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<u64, UdpError>> + Send + 'a {
        self.inner
            .udp_send(socket, host, port, bytes, timeout_nanos)
    }

    fn udp_send_address<'a>(
        &'a self,
        socket: Self::UdpSocket,
        remote_address: NetworkIpAddress,
        port: u16,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<u64, UdpError>> + Send + 'a {
        self.inner
            .udp_send_address(socket, remote_address, port, bytes, timeout_nanos)
    }

    fn udp_receive(
        &self,
        socket: Self::UdpSocket,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Option<UdpDatagram>, UdpError>> + Send + '_ {
        self.inner.udp_receive(socket, max_bytes, timeout_nanos)
    }

    fn udp_join_multicast_v4(
        &self,
        group: Ipv4Address,
        interface: Ipv4Address,
    ) -> impl Future<Output = Result<(), UdpError>> + Send + '_ {
        self.inner.udp_join_multicast_v4(group, interface)
    }

    fn udp_leave_multicast_v4(
        &self,
        group: Ipv4Address,
        interface: Ipv4Address,
    ) -> impl Future<Output = Result<(), UdpError>> + Send + '_ {
        self.inner.udp_leave_multicast_v4(group, interface)
    }

    fn udp_close(&self, socket: Self::UdpSocket) -> impl Future<Output = ()> + Send + '_ {
        self.inner.udp_close(socket)
    }
}

impl NetworkAdminBackend for ComponentHostNetworkService {
    fn bridge_port(
        &self,
        port: NetworkPortId,
        bridge: NetworkBridgeRequest,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.inner.bridge_port(port, bridge)
    }

    fn unbridge_port(
        &self,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.inner.unbridge_port(port)
    }

    fn acquire_dhcp(
        &self,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<Ipv4Cidr, NetworkControlError>> + Send {
        self.inner.acquire_dhcp(port)
    }

    fn add_address(
        &self,
        port: NetworkPortId,
        address: Ipv4Cidr,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.inner.add_address(port, address)
    }

    fn remove_address(
        &self,
        port: NetworkPortId,
        address: Ipv4Cidr,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.inner.remove_address(port, address)
    }

    fn clear_addresses(
        &self,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.inner.clear_addresses(port)
    }

    fn list_addresses(
        &self,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<Vec<Ipv4Cidr>, NetworkControlError>> + Send {
        self.inner.list_addresses(port)
    }

    fn mac_address(
        &self,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<MacAddress, NetworkControlError>> + Send {
        self.inner.mac_address(port)
    }

    fn set_gateway(
        &self,
        port: NetworkPortId,
        gateway: Ipv4Address,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.inner.set_gateway(port, gateway)
    }

    fn add_route(
        &self,
        port: NetworkPortId,
        route: Ipv4Route,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.inner.add_route(port, route)
    }

    fn remove_route(
        &self,
        port: NetworkPortId,
        route: Ipv4Route,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.inner.remove_route(port, route)
    }

    fn clear_routes(
        &self,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.inner.clear_routes(port)
    }

    fn list_routes(
        &self,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<Vec<Ipv4Route>, NetworkControlError>> + Send {
        self.inner.list_routes(port)
    }
}

struct TypedNetworkService<Service> {
    service: Service,
}

impl<Service> DynComponentHostNetworkService for TypedNetworkService<Service>
where
    Service: ComponentNetworkService + NetworkAdminBackend + Sync,
    Service::TcpStream: ComponentHostTcpStreamToken,
    Service::TcpListener: ComponentHostTcpListenerToken,
    Service::UdpSocket: ComponentHostUdpSocketToken,
{
    fn hardware_address(&self) -> [u8; 6] {
        self.service.hardware_address()
    }

    fn ipv4_cidr(&self) -> Pin<Box<dyn Future<Output = Option<crate::Ipv4Cidr>> + Send + '_>> {
        Box::pin(self.service.ipv4_cidr())
    }

    fn ping<'a>(
        &'a self,
        host: &'a str,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<PingReply, PingError>> + Send + 'a>> {
        Box::pin(self.service.ping(host, timeout_nanos))
    }

    fn dns_resolve<'a>(
        &'a self,
        host: &'a str,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Ipv4Address>, DnsError>> + Send + 'a>> {
        Box::pin(self.service.dns_resolve(host, timeout_nanos))
    }

    fn tcp_connect<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, TcpError>> + Send + 'a>> {
        Box::pin(async move {
            let stream = self.service.tcp_connect(host, port, timeout_nanos).await?;
            Ok(stream.into_raw())
        })
    }

    fn tcp_connect_from<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        local_port: u16,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, TcpError>> + Send + 'a>> {
        Box::pin(async move {
            let stream = self
                .service
                .tcp_connect_from(host, port, local_port, timeout_nanos)
                .await?;
            Ok(stream.into_raw())
        })
    }

    fn tcp_connect_address<'a>(
        &'a self,
        remote_address: NetworkIpAddress,
        port: u16,
        local_port: u16,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, TcpError>> + Send + 'a>> {
        Box::pin(async move {
            let stream = self
                .service
                .tcp_connect_address(remote_address, port, local_port, timeout_nanos)
                .await?;
            Ok(stream.into_raw())
        })
    }

    fn tcp_listen<'a>(
        &'a self,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: u16,
    ) -> Pin<Box<dyn Future<Output = Result<TcpListener<u64>, TcpError>> + Send + 'a>> {
        Box::pin(async move {
            let listener = self
                .service
                .tcp_listen(local_address, local_port, backlog)
                .await?;
            Ok(TcpListener {
                listener: listener.listener.into_raw(),
                local_port: listener.local_port,
            })
        })
    }

    fn tcp_accept<'a>(
        &'a self,
        listener: u64,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<TcpAccepted<u64>, TcpError>> + Send + 'a>> {
        Box::pin(async move {
            let accepted = self
                .service
                .tcp_accept(
                    <Service::TcpListener as ComponentHostTcpListenerToken>::from_raw(listener),
                    timeout_nanos,
                )
                .await?;
            Ok(TcpAccepted {
                stream: accepted.stream.into_raw(),
                address: accepted.address,
                port: accepted.port,
            })
        })
    }

    fn tcp_write_all<'a>(
        &'a self,
        stream: u64,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), TcpError>> + Send + 'a>> {
        Box::pin(self.service.tcp_write_all(
            <Service::TcpStream as ComponentHostTcpStreamToken>::from_raw(stream),
            bytes,
            timeout_nanos,
        ))
    }

    fn tcp_write_all_bytes<'a>(
        &'a self,
        stream: u64,
        bytes: Bytes,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), TcpError>> + Send + 'a>> {
        Box::pin(self.service.tcp_write_all_bytes(
            <Service::TcpStream as ComponentHostTcpStreamToken>::from_raw(stream),
            bytes,
            timeout_nanos,
        ))
    }

    fn tcp_read<'a>(
        &'a self,
        stream: u64,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Bytes>, TcpError>> + Send + 'a>> {
        Box::pin(self.service.tcp_read(
            <Service::TcpStream as ComponentHostTcpStreamToken>::from_raw(stream),
            max_bytes,
            timeout_nanos,
        ))
    }

    fn tcp_read_into_registered<'a>(
        &'a self,
        stream: u64,
        buffer: RegisteredTcpReadBuffer<'a>,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<usize>, TcpError>> + Send + 'a>> {
        Box::pin(self.service.tcp_read_into(
            <Service::TcpStream as ComponentHostTcpStreamToken>::from_raw(stream),
            buffer,
            timeout_nanos,
        ))
    }

    fn tcp_shutdown_send<'a>(
        &'a self,
        stream: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), TcpError>> + Send + 'a>> {
        Box::pin(self.service.tcp_shutdown_send(
            <Service::TcpStream as ComponentHostTcpStreamToken>::from_raw(stream),
        ))
    }

    fn tcp_close<'a>(&'a self, stream: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(
            self.service
                .tcp_close(<Service::TcpStream as ComponentHostTcpStreamToken>::from_raw(stream)),
        )
    }

    fn udp_bind<'a>(
        &'a self,
        local_port: u16,
    ) -> Pin<Box<dyn Future<Output = Result<UdpBinding<u64>, UdpError>> + Send + 'a>> {
        Box::pin(async move {
            let binding = self.service.udp_bind(local_port).await?;
            Ok(UdpBinding {
                socket: binding.socket.into_raw(),
                local_port: binding.local_port,
            })
        })
    }

    fn udp_connect<'a>(
        &'a self,
        socket: u64,
        remote_address: NetworkIpAddress,
        port: u16,
    ) -> Result<(), UdpError> {
        self.service.udp_connect(
            <Service::UdpSocket as ComponentHostUdpSocketToken>::from_raw(socket),
            remote_address,
            port,
        )
    }

    fn udp_disconnect(&self, socket: u64) -> Result<(), UdpError> {
        self.service
            .udp_disconnect(<Service::UdpSocket as ComponentHostUdpSocketToken>::from_raw(socket))
    }

    fn udp_send<'a>(
        &'a self,
        socket: u64,
        host: &'a str,
        port: u16,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, UdpError>> + Send + 'a>> {
        Box::pin(self.service.udp_send(
            <Service::UdpSocket as ComponentHostUdpSocketToken>::from_raw(socket),
            host,
            port,
            bytes,
            timeout_nanos,
        ))
    }

    fn udp_send_address<'a>(
        &'a self,
        socket: u64,
        remote_address: NetworkIpAddress,
        port: u16,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, UdpError>> + Send + 'a>> {
        Box::pin(self.service.udp_send_address(
            <Service::UdpSocket as ComponentHostUdpSocketToken>::from_raw(socket),
            remote_address,
            port,
            bytes,
            timeout_nanos,
        ))
    }

    fn udp_receive<'a>(
        &'a self,
        socket: u64,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<UdpDatagram>, UdpError>> + Send + 'a>> {
        Box::pin(self.service.udp_receive(
            <Service::UdpSocket as ComponentHostUdpSocketToken>::from_raw(socket),
            max_bytes,
            timeout_nanos,
        ))
    }

    fn udp_join_multicast_v4<'a>(
        &'a self,
        group: Ipv4Address,
        interface: Ipv4Address,
    ) -> Pin<Box<dyn Future<Output = Result<(), UdpError>> + Send + 'a>> {
        Box::pin(self.service.udp_join_multicast_v4(group, interface))
    }

    fn udp_leave_multicast_v4<'a>(
        &'a self,
        group: Ipv4Address,
        interface: Ipv4Address,
    ) -> Pin<Box<dyn Future<Output = Result<(), UdpError>> + Send + 'a>> {
        Box::pin(self.service.udp_leave_multicast_v4(group, interface))
    }

    fn udp_close<'a>(&'a self, socket: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(
            self.service
                .udp_close(<Service::UdpSocket as ComponentHostUdpSocketToken>::from_raw(socket)),
        )
    }

    fn bridge_port<'a>(
        &'a self,
        port: NetworkPortId,
        bridge: NetworkBridgeRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>> {
        Box::pin(self.service.bridge_port(port, bridge))
    }

    fn unbridge_port<'a>(
        &'a self,
        port: NetworkPortId,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>> {
        Box::pin(self.service.unbridge_port(port))
    }

    fn acquire_dhcp<'a>(
        &'a self,
        port: NetworkPortId,
    ) -> Pin<Box<dyn Future<Output = Result<Ipv4Cidr, NetworkControlError>> + Send + 'a>> {
        Box::pin(self.service.acquire_dhcp(port))
    }

    fn add_address<'a>(
        &'a self,
        port: NetworkPortId,
        address: Ipv4Cidr,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>> {
        Box::pin(self.service.add_address(port, address))
    }

    fn remove_address<'a>(
        &'a self,
        port: NetworkPortId,
        address: Ipv4Cidr,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>> {
        Box::pin(self.service.remove_address(port, address))
    }

    fn clear_addresses<'a>(
        &'a self,
        port: NetworkPortId,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>> {
        Box::pin(self.service.clear_addresses(port))
    }

    fn list_addresses<'a>(
        &'a self,
        port: NetworkPortId,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Ipv4Cidr>, NetworkControlError>> + Send + 'a>> {
        Box::pin(self.service.list_addresses(port))
    }

    fn mac_address<'a>(
        &'a self,
        port: NetworkPortId,
    ) -> Pin<Box<dyn Future<Output = Result<MacAddress, NetworkControlError>> + Send + 'a>> {
        Box::pin(self.service.mac_address(port))
    }

    fn set_gateway<'a>(
        &'a self,
        port: NetworkPortId,
        gateway: Ipv4Address,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>> {
        Box::pin(self.service.set_gateway(port, gateway))
    }

    fn add_route<'a>(
        &'a self,
        port: NetworkPortId,
        route: Ipv4Route,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>> {
        Box::pin(self.service.add_route(port, route))
    }

    fn remove_route<'a>(
        &'a self,
        port: NetworkPortId,
        route: Ipv4Route,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>> {
        Box::pin(self.service.remove_route(port, route))
    }

    fn clear_routes<'a>(
        &'a self,
        port: NetworkPortId,
    ) -> Pin<Box<dyn Future<Output = Result<(), NetworkControlError>> + Send + 'a>> {
        Box::pin(self.service.clear_routes(port))
    }

    fn list_routes<'a>(
        &'a self,
        port: NetworkPortId,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Ipv4Route>, NetworkControlError>> + Send + 'a>>
    {
        Box::pin(self.service.list_routes(port))
    }
}
