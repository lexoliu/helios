extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use crate::{
    ComponentNetworkService, DnsError, Ipv4Address, PingError, PingReply, TcpAccepted, TcpError,
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

    fn tcp_listen<'a>(
        &'a self,
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

    fn tcp_read<'a>(
        &'a self,
        stream: u64,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, TcpError>> + Send + 'a>>;

    fn tcp_close<'a>(&'a self, stream: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    fn udp_bind<'a>(
        &'a self,
        local_port: u16,
    ) -> Pin<Box<dyn Future<Output = Result<UdpBinding<u64>, UdpError>> + Send + 'a>>;

    fn udp_send<'a>(
        &'a self,
        socket: u64,
        host: &'a str,
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

    fn udp_close<'a>(&'a self, socket: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

#[derive(Clone)]
pub struct ComponentHostNetworkService {
    inner: Arc<dyn DynComponentHostNetworkService>,
}

impl ComponentHostNetworkService {
    pub fn from_service<Service>(service: Service) -> Self
    where
        Service: ComponentNetworkService + Sync,
        Service::TcpStream: ComponentHostTcpStreamToken,
        Service::TcpListener: ComponentHostTcpListenerToken,
        Service::UdpSocket: ComponentHostUdpSocketToken,
    {
        Self {
            inner: Arc::new(TypedNetworkService { service }),
        }
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

    fn tcp_listen(
        &self,
        local_port: u16,
        backlog: u16,
    ) -> impl Future<Output = Result<TcpListener<Self::TcpListener>, TcpError>> + Send + '_ {
        self.inner.tcp_listen(local_port, backlog)
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

    fn tcp_read(
        &self,
        stream: Self::TcpStream,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, TcpError>> + Send + '_ {
        self.inner.tcp_read(stream, max_bytes, timeout_nanos)
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

    fn udp_receive(
        &self,
        socket: Self::UdpSocket,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Option<UdpDatagram>, UdpError>> + Send + '_ {
        self.inner.udp_receive(socket, max_bytes, timeout_nanos)
    }

    fn udp_close(&self, socket: Self::UdpSocket) -> impl Future<Output = ()> + Send + '_ {
        self.inner.udp_close(socket)
    }
}

struct TypedNetworkService<Service> {
    service: Service,
}

impl<Service> DynComponentHostNetworkService for TypedNetworkService<Service>
where
    Service: ComponentNetworkService + Sync,
    Service::TcpStream: ComponentHostTcpStreamToken,
    Service::TcpListener: ComponentHostTcpListenerToken,
    Service::UdpSocket: ComponentHostUdpSocketToken,
{
    fn hardware_address(&self) -> [u8; 6] {
        self.service.hardware_address()
    }

    fn ipv4_cidr(&self) -> Pin<Box<dyn Future<Output = Option<crate::Ipv4Cidr>> + Send + '_>> {
        Box::pin(async move { self.service.ipv4_cidr().await })
    }

    fn ping<'a>(
        &'a self,
        host: &'a str,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<PingReply, PingError>> + Send + 'a>> {
        Box::pin(async move { self.service.ping(host, timeout_nanos).await })
    }

    fn dns_resolve<'a>(
        &'a self,
        host: &'a str,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Ipv4Address>, DnsError>> + Send + 'a>> {
        Box::pin(async move { self.service.dns_resolve(host, timeout_nanos).await })
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

    fn tcp_listen<'a>(
        &'a self,
        local_port: u16,
        backlog: u16,
    ) -> Pin<Box<dyn Future<Output = Result<TcpListener<u64>, TcpError>> + Send + 'a>> {
        Box::pin(async move {
            let listener = self.service.tcp_listen(local_port, backlog).await?;
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
        Box::pin(async move {
            self.service
                .tcp_write_all(
                    <Service::TcpStream as ComponentHostTcpStreamToken>::from_raw(stream),
                    bytes,
                    timeout_nanos,
                )
                .await
        })
    }

    fn tcp_read<'a>(
        &'a self,
        stream: u64,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, TcpError>> + Send + 'a>> {
        Box::pin(async move {
            self.service
                .tcp_read(
                    <Service::TcpStream as ComponentHostTcpStreamToken>::from_raw(stream),
                    max_bytes,
                    timeout_nanos,
                )
                .await
        })
    }

    fn tcp_close<'a>(&'a self, stream: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.service
                .tcp_close(<Service::TcpStream as ComponentHostTcpStreamToken>::from_raw(stream))
                .await
        })
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

    fn udp_send<'a>(
        &'a self,
        socket: u64,
        host: &'a str,
        port: u16,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, UdpError>> + Send + 'a>> {
        Box::pin(async move {
            self.service
                .udp_send(
                    <Service::UdpSocket as ComponentHostUdpSocketToken>::from_raw(socket),
                    host,
                    port,
                    bytes,
                    timeout_nanos,
                )
                .await
        })
    }

    fn udp_receive<'a>(
        &'a self,
        socket: u64,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<UdpDatagram>, UdpError>> + Send + 'a>> {
        Box::pin(async move {
            self.service
                .udp_receive(
                    <Service::UdpSocket as ComponentHostUdpSocketToken>::from_raw(socket),
                    max_bytes,
                    timeout_nanos,
                )
                .await
        })
    }

    fn udp_close<'a>(&'a self, socket: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.service
                .udp_close(<Service::UdpSocket as ComponentHostUdpSocketToken>::from_raw(socket))
                .await
        })
    }
}
