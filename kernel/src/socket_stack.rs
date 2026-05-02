extern crate alloc;

use core::future::Future;

use crate::{
    ComponentNetworkService, DnsCap, NetworkErrorDetail, PrivilegedBindCap, TcpCap, UdpBinding,
    UdpCap, UdpError, UdpErrorKind,
};

#[derive(Clone)]
pub struct SocketStack<Service> {
    service: Service,
}

impl<Service> SocketStack<Service>
where
    Service: ComponentNetworkService,
{
    pub const fn new(service: Service) -> Self {
        Self { service }
    }

    pub fn dns_resolve<'a>(
        &'a self,
        _: DnsCap,
        host: &'a str,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<alloc::vec::Vec<crate::Ipv4Address>, crate::DnsError>> + 'a
    {
        self.service.dns_resolve(host, timeout_nanos)
    }

    pub fn tcp_connect<'a>(
        &'a self,
        _: TcpCap,
        host: &'a str,
        port: u16,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Service::TcpStream, crate::TcpError>> + Send + 'a {
        self.service.tcp_connect(host, port, timeout_nanos)
    }

    pub fn tcp_write_all<'a>(
        &'a self,
        _: TcpCap,
        stream: Service::TcpStream,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<(), crate::TcpError>> + Send + 'a {
        self.service.tcp_write_all(stream, bytes, timeout_nanos)
    }

    pub fn tcp_read(
        &self,
        _: TcpCap,
        stream: Service::TcpStream,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Option<alloc::vec::Vec<u8>>, crate::TcpError>> + Send + '_
    {
        self.service.tcp_read(stream, max_bytes, timeout_nanos)
    }

    pub fn tcp_close(
        &self,
        _: TcpCap,
        stream: Service::TcpStream,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.service.tcp_close(stream)
    }

    pub fn udp_bind(
        &self,
        _: UdpCap,
        privileged_bind: Option<PrivilegedBindCap>,
        local_port: u16,
    ) -> impl Future<Output = Result<UdpBinding<Service::UdpSocket>, UdpError>> + '_ {
        async move {
            if local_port < 1024 && privileged_bind.is_none() {
                return Err(UdpError {
                    kind: UdpErrorKind::PermissionDenied,
                    detail: NetworkErrorDetail::PrivilegedBindDenied,
                });
            }
            self.service.udp_bind(local_port).await
        }
    }

    pub fn udp_send<'a>(
        &'a self,
        _: UdpCap,
        socket: Service::UdpSocket,
        host: &'a str,
        port: u16,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<u64, UdpError>> + Send + 'a {
        self.service
            .udp_send(socket, host, port, bytes, timeout_nanos)
    }

    pub fn udp_receive(
        &self,
        _: UdpCap,
        socket: Service::UdpSocket,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Option<crate::UdpDatagram>, UdpError>> + Send + '_ {
        self.service.udp_receive(socket, max_bytes, timeout_nanos)
    }

    pub fn udp_close(
        &self,
        _: UdpCap,
        socket: Service::UdpSocket,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.service.udp_close(socket)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;
    use futures_lite::future::block_on;

    use super::SocketStack;
    use crate::{
        ComponentNetworkService, DnsError, Ipv4Address, NetworkAuthorityRights, NetworkErrorDetail,
        PingError, PingReply, ProcessAuthority, TcpError, UdpBinding, UdpDatagram, UdpError,
    };

    #[derive(Clone, Copy)]
    struct TestNetworkService;

    impl ComponentNetworkService for TestNetworkService {
        type TcpStream = u64;
        type UdpSocket = u64;

        fn hardware_address(&self) -> [u8; 6] {
            [2, 0, 0, 0, 0, 1]
        }

        fn ping(
            &self,
            _: &str,
            _: u64,
        ) -> impl Future<Output = Result<PingReply, PingError>> + Send + '_ {
            core::future::ready(Ok(PingReply {
                address: Ipv4Address::new([127, 0, 0, 1]),
                round_trip_nanos: 1,
                payload_bytes: 0,
            }))
        }

        fn dns_resolve(
            &self,
            _: &str,
            _: u64,
        ) -> impl Future<Output = Result<Vec<Ipv4Address>, DnsError>> + Send + '_ {
            core::future::ready(Ok(vec![Ipv4Address::new([127, 0, 0, 1])]))
        }

        fn tcp_connect(
            &self,
            _: &str,
            _: u16,
            _: u64,
        ) -> impl Future<Output = Result<Self::TcpStream, TcpError>> + Send + '_ {
            core::future::ready(Ok(7))
        }

        fn tcp_write_all(
            &self,
            _: Self::TcpStream,
            _: &[u8],
            _: u64,
        ) -> impl Future<Output = Result<(), TcpError>> + Send + '_ {
            core::future::ready(Ok(()))
        }

        fn tcp_read(
            &self,
            _: Self::TcpStream,
            _: u32,
            _: u64,
        ) -> impl Future<Output = Result<Option<Vec<u8>>, TcpError>> + Send + '_ {
            core::future::ready(Ok(None))
        }

        fn tcp_close(&self, _: Self::TcpStream) -> impl Future<Output = ()> + Send + '_ {
            core::future::ready(())
        }

        fn udp_bind(
            &self,
            local_port: u16,
        ) -> impl Future<Output = Result<UdpBinding<Self::UdpSocket>, UdpError>> + Send + '_
        {
            core::future::ready(Ok(UdpBinding {
                socket: 9,
                local_port,
            }))
        }

        fn udp_send(
            &self,
            _: Self::UdpSocket,
            _: &str,
            _: u16,
            bytes: &[u8],
            _: u64,
        ) -> impl Future<Output = Result<u64, UdpError>> + Send + '_ {
            core::future::ready(Ok(bytes.len() as u64))
        }

        fn udp_receive(
            &self,
            _: Self::UdpSocket,
            _: u32,
            _: u64,
        ) -> impl Future<Output = Result<Option<UdpDatagram>, UdpError>> + Send + '_ {
            core::future::ready(Ok(None))
        }

        fn udp_close(&self, _: Self::UdpSocket) -> impl Future<Output = ()> + Send + '_ {
            core::future::ready(())
        }
    }

    #[test]
    fn socket_stack_consumes_typed_network_caps() {
        let mut authority = ProcessAuthority::empty();
        authority.grant_network_rights(
            NetworkAuthorityRights::TCP
                | NetworkAuthorityRights::UDP
                | NetworkAuthorityRights::DNS
                | NetworkAuthorityRights::PRIVILEGED_BIND,
        );
        let tcp = authority.derive_tcp_cap().unwrap();
        let udp = authority.derive_udp_cap().unwrap();
        let dns = authority.derive_dns_cap().unwrap();
        let privileged = authority.derive_privileged_bind_cap().unwrap();
        let stack = SocketStack::new(TestNetworkService);

        assert_eq!(
            block_on(stack.dns_resolve(dns, "localhost", 1)).unwrap(),
            vec![Ipv4Address::new([127, 0, 0, 1])]
        );
        assert_eq!(
            block_on(stack.tcp_connect(tcp, "localhost", 80, 1)).unwrap(),
            7
        );
        assert_eq!(
            block_on(stack.udp_bind(udp, Some(privileged), 53))
                .unwrap()
                .local_port,
            53
        );
    }

    #[test]
    fn low_udp_bind_requires_privileged_bind_cap() {
        let mut authority = ProcessAuthority::empty();
        authority.grant_network_rights(NetworkAuthorityRights::UDP);
        let udp = authority.derive_udp_cap().unwrap();
        let stack = SocketStack::new(TestNetworkService);

        let error = block_on(stack.udp_bind(udp, None, 53)).unwrap_err();
        assert_eq!(error.kind, crate::UdpErrorKind::PermissionDenied);
        assert_eq!(error.detail, NetworkErrorDetail::PrivilegedBindDenied);
    }

    #[test]
    fn high_udp_bind_does_not_require_privileged_bind_cap() {
        let mut authority = ProcessAuthority::empty();
        authority.grant_network_rights(NetworkAuthorityRights::UDP);
        let udp = authority.derive_udp_cap().unwrap();
        let stack = SocketStack::new(TestNetworkService);

        assert_eq!(
            block_on(stack.udp_bind(udp, None, 8080))
                .unwrap()
                .local_port,
            8080
        );
    }
}
