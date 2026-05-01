extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;

use crate::{
    ComponentNetworkService, DnsError, Ipv4Address, PingError, PingReply, TcpError, UdpBinding,
    UdpDatagram, UdpError,
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

pub trait DynComponentHostNetworkService: Send + Sync + 'static {
    fn ping<'a>(
        &'a self,
        host: String,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<PingReply, PingError>> + Send + 'a>>;

    fn dns_resolve<'a>(
        &'a self,
        host: String,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Ipv4Address>, DnsError>> + Send + 'a>>;

    fn tcp_connect<'a>(
        &'a self,
        host: String,
        port: u16,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, TcpError>> + Send + 'a>>;

    fn tcp_write_all<'a>(
        &'a self,
        stream: u64,
        bytes: Vec<u8>,
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
        host: String,
        port: u16,
        bytes: Vec<u8>,
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
        Service::UdpSocket: ComponentHostUdpSocketToken,
    {
        Self {
            inner: Arc::new(TypedNetworkService { service }),
        }
    }
}

impl ComponentNetworkService for ComponentHostNetworkService {
    type TcpStream = u64;
    type UdpSocket = u64;

    fn ping(
        &self,
        host: &str,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<PingReply, PingError>> + Send + '_ {
        self.inner.ping(String::from(host), timeout_nanos)
    }

    fn dns_resolve(
        &self,
        host: &str,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Vec<Ipv4Address>, DnsError>> + Send + '_ {
        self.inner.dns_resolve(String::from(host), timeout_nanos)
    }

    fn tcp_connect(
        &self,
        host: &str,
        port: u16,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Self::TcpStream, TcpError>> + Send + '_ {
        self.inner
            .tcp_connect(String::from(host), port, timeout_nanos)
    }

    fn tcp_write_all(
        &self,
        stream: Self::TcpStream,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<(), TcpError>> + Send + '_ {
        self.inner
            .tcp_write_all(stream, bytes.to_vec(), timeout_nanos)
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

    fn udp_send(
        &self,
        socket: Self::UdpSocket,
        host: &str,
        port: u16,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<u64, UdpError>> + Send + '_ {
        self.inner.udp_send(
            socket,
            String::from(host),
            port,
            bytes.to_vec(),
            timeout_nanos,
        )
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
    Service::UdpSocket: ComponentHostUdpSocketToken,
{
    fn ping<'a>(
        &'a self,
        host: String,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<PingReply, PingError>> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move { service.ping(&host, timeout_nanos).await })
    }

    fn dns_resolve<'a>(
        &'a self,
        host: String,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Ipv4Address>, DnsError>> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move { service.dns_resolve(&host, timeout_nanos).await })
    }

    fn tcp_connect<'a>(
        &'a self,
        host: String,
        port: u16,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, TcpError>> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move {
            let stream = service.tcp_connect(&host, port, timeout_nanos).await?;
            Ok(stream.into_raw())
        })
    }

    fn tcp_write_all<'a>(
        &'a self,
        stream: u64,
        bytes: Vec<u8>,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), TcpError>> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move {
            service
                .tcp_write_all(
                    <Service::TcpStream as ComponentHostTcpStreamToken>::from_raw(stream),
                    &bytes,
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
        let service = self.service.clone();
        Box::pin(async move {
            service
                .tcp_read(
                    <Service::TcpStream as ComponentHostTcpStreamToken>::from_raw(stream),
                    max_bytes,
                    timeout_nanos,
                )
                .await
        })
    }

    fn tcp_close<'a>(&'a self, stream: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move {
            service
                .tcp_close(<Service::TcpStream as ComponentHostTcpStreamToken>::from_raw(stream))
                .await
        })
    }

    fn udp_bind<'a>(
        &'a self,
        local_port: u16,
    ) -> Pin<Box<dyn Future<Output = Result<UdpBinding<u64>, UdpError>> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move {
            let binding = service.udp_bind(local_port).await?;
            Ok(UdpBinding {
                socket: binding.socket.into_raw(),
                local_port: binding.local_port,
            })
        })
    }

    fn udp_send<'a>(
        &'a self,
        socket: u64,
        host: String,
        port: u16,
        bytes: Vec<u8>,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, UdpError>> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move {
            service
                .udp_send(
                    <Service::UdpSocket as ComponentHostUdpSocketToken>::from_raw(socket),
                    &host,
                    port,
                    &bytes,
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
        let service = self.service.clone();
        Box::pin(async move {
            service
                .udp_receive(
                    <Service::UdpSocket as ComponentHostUdpSocketToken>::from_raw(socket),
                    max_bytes,
                    timeout_nanos,
                )
                .await
        })
    }

    fn udp_close<'a>(&'a self, socket: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move {
            service
                .udp_close(<Service::UdpSocket as ComponentHostUdpSocketToken>::from_raw(socket))
                .await
        })
    }
}
