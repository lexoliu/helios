extern crate alloc;

use core::future::Future;

use bytes::Bytes;

use crate::{
    ComponentNetworkService, DnsCap, MulticastCap, NetworkErrorDetail, NetworkIpAddress,
    PrivilegedBindCap, TcpAccepted, TcpCap, TcpError, TcpErrorKind, TcpListener, UdpBinding,
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
    ) -> impl Future<Output = Result<alloc::vec::Vec<NetworkIpAddress>, crate::DnsError>> + 'a
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

    pub fn tcp_connect_from<'a>(
        &'a self,
        _: TcpCap,
        host: &'a str,
        port: u16,
        local_port: u16,
        hop_limit: u8,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Service::TcpStream, crate::TcpError>> + Send + 'a {
        self.service
            .tcp_connect_from(host, port, local_port, hop_limit, timeout_nanos)
    }

    pub fn tcp_listen(
        &self,
        _: TcpCap,
        privileged_bind: Option<PrivilegedBindCap>,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: u16,
        hop_limit: u8,
    ) -> impl Future<Output = Result<TcpListener<Service::TcpListener>, TcpError>> + Send + '_ {
        async move {
            if local_port < 1024 && privileged_bind.is_none() {
                return Err(TcpError {
                    kind: TcpErrorKind::PermissionDenied,
                    detail: NetworkErrorDetail::PrivilegedBindDenied,
                });
            }
            self.service
                .tcp_listen(local_address, local_port, backlog, hop_limit)
                .await
        }
    }

    pub fn tcp_accept(
        &self,
        _: TcpCap,
        listener: Service::TcpListener,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<TcpAccepted<Service::TcpStream>, TcpError>> + Send + '_ {
        self.service.tcp_accept(listener, timeout_nanos)
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

    pub fn tcp_write_all_bytes(
        &self,
        _: TcpCap,
        stream: Service::TcpStream,
        bytes: Bytes,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<(), crate::TcpError>> + Send + '_ {
        self.service
            .tcp_write_all_bytes(stream, bytes, timeout_nanos)
    }

    pub fn tcp_read(
        &self,
        _: TcpCap,
        stream: Service::TcpStream,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Option<Bytes>, crate::TcpError>> + Send + '_ {
        self.service.tcp_read(stream, max_bytes, timeout_nanos)
    }

    pub fn tcp_shutdown_send(
        &self,
        _: TcpCap,
        stream: Service::TcpStream,
    ) -> impl Future<Output = Result<(), crate::TcpError>> + Send + '_ {
        self.service.tcp_shutdown_send(stream)
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

    pub fn udp_connect(
        &self,
        _: UdpCap,
        socket: Service::UdpSocket,
        remote_address: crate::NetworkIpAddress,
        port: u16,
    ) -> Result<(), UdpError> {
        self.service.udp_connect(socket, remote_address, port)
    }

    pub fn udp_disconnect(&self, _: UdpCap, socket: Service::UdpSocket) -> Result<(), UdpError> {
        self.service.udp_disconnect(socket)
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

    pub fn udp_join_multicast_v4(
        &self,
        _: UdpCap,
        _: MulticastCap,
        group: crate::Ipv4Address,
        interface: crate::Ipv4Address,
    ) -> impl Future<Output = Result<(), UdpError>> + Send + '_ {
        self.service.udp_join_multicast_v4(group, interface)
    }

    pub fn udp_leave_multicast_v4(
        &self,
        _: UdpCap,
        _: MulticastCap,
        group: crate::Ipv4Address,
        interface: crate::Ipv4Address,
    ) -> impl Future<Output = Result<(), UdpError>> + Send + '_ {
        self.service.udp_leave_multicast_v4(group, interface)
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
    use crate::SocketReadiness;
    use alloc::vec;
    use alloc::vec::Vec;

    use bytes::Bytes;
    use futures_lite::future::block_on;

    use super::SocketStack;
    use crate::{
        ComponentNetworkService, DnsError, Ipv4Address, NetworkAuthorityRights, NetworkErrorDetail,
        NetworkIpAddress, PingError, PingReply, ProcessAuthority, TcpAccepted, TcpError,
        TcpListener, UdpBinding, UdpDatagram, UdpError,
    };

    const TCP_ANY_V4: NetworkIpAddress = NetworkIpAddress::Ipv4(Ipv4Address::new([0, 0, 0, 0]));

    #[derive(Clone, Copy)]
    struct TestNetworkService;

    impl ComponentNetworkService for TestNetworkService {
        type TcpStream = u64;
        type TcpListener = u64;
        type UdpSocket = u64;

        // The in-memory doubles model an always-ready loopback peer.
        fn tcp_readiness(
            &self,
            _: Self::TcpStream,
        ) -> impl Future<Output = Result<SocketReadiness, TcpError>> + Send + '_ {
            core::future::ready(Ok(SocketReadiness {
                readable: true,
                writable: true,
                hangup: false,
            }))
        }

        fn tcp_listener_readiness(
            &self,
            _: Self::TcpListener,
        ) -> impl Future<Output = Result<SocketReadiness, TcpError>> + Send + '_ {
            core::future::ready(Ok(SocketReadiness {
                readable: true,
                writable: false,
                hangup: false,
            }))
        }

        fn udp_readiness(
            &self,
            _: Self::UdpSocket,
        ) -> impl Future<Output = Result<SocketReadiness, UdpError>> + Send + '_ {
            core::future::ready(Ok(SocketReadiness {
                readable: true,
                writable: true,
                hangup: false,
            }))
        }

        fn hardware_address(&self) -> [u8; 6] {
            [2, 0, 0, 0, 0, 1]
        }

        fn ipv4_cidr(
            &self,
        ) -> impl core::future::Future<Output = Option<crate::Ipv4Cidr>> + Send + '_ {
            core::future::ready(None)
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
        ) -> impl Future<Output = Result<Vec<NetworkIpAddress>, DnsError>> + Send + '_ {
            core::future::ready(Ok(vec![
                NetworkIpAddress::Ipv4(Ipv4Address::new([127, 0, 0, 1])),
                NetworkIpAddress::Ipv6(helios_netstack::Ipv6Address::LOOPBACK),
            ]))
        }

        fn tcp_connect(
            &self,
            _: &str,
            _: u16,
            _: u64,
        ) -> impl Future<Output = Result<Self::TcpStream, TcpError>> + Send + '_ {
            core::future::ready(Ok(7))
        }

        fn tcp_connect_from(
            &self,
            _: &str,
            _: u16,
            local_port: u16,
            _: u8,
            _: u64,
        ) -> impl Future<Output = Result<Self::TcpStream, TcpError>> + Send + '_ {
            core::future::ready(Ok(u64::from(local_port)))
        }

        fn tcp_connect_address(
            &self,
            _: NetworkIpAddress,
            _: u16,
            local_port: u16,
            _: u8,
            _: u64,
        ) -> impl Future<Output = Result<Self::TcpStream, TcpError>> + Send + '_ {
            core::future::ready(Ok(if local_port == 0 {
                7
            } else {
                u64::from(local_port)
            }))
        }

        fn tcp_listen(
            &self,
            _: NetworkIpAddress,
            local_port: u16,
            _: u16,
            _: u8,
        ) -> impl Future<Output = Result<TcpListener<Self::TcpListener>, TcpError>> + Send + '_
        {
            core::future::ready(Ok(TcpListener {
                listener: 8,
                local_port,
            }))
        }

        fn tcp_set_hop_limit(&self, _: Self::TcpStream, _: u8) -> Result<(), TcpError> {
            Ok(())
        }

        fn tcp_listener_set_hop_limit(&self, _: Self::TcpListener, _: u8) -> Result<(), TcpError> {
            Ok(())
        }

        fn tcp_accept(
            &self,
            listener: Self::TcpListener,
            _: u64,
        ) -> impl Future<Output = Result<TcpAccepted<Self::TcpStream>, TcpError>> + Send + '_
        {
            core::future::ready(Ok(TcpAccepted {
                stream: listener + 1,
                address: NetworkIpAddress::Ipv4(Ipv4Address::new([127, 0, 0, 1])),
                port: 4040,
            }))
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
        ) -> impl Future<Output = Result<Option<Bytes>, TcpError>> + Send + '_ {
            core::future::ready(Ok(None))
        }

        fn tcp_shutdown_send(
            &self,
            _: Self::TcpStream,
        ) -> impl Future<Output = Result<(), TcpError>> + Send + '_ {
            core::future::ready(Ok(()))
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

        fn udp_connect(
            &self,
            _: Self::UdpSocket,
            _: NetworkIpAddress,
            _: u16,
        ) -> Result<(), UdpError> {
            Ok(())
        }

        fn udp_disconnect(&self, _: Self::UdpSocket) -> Result<(), UdpError> {
            Ok(())
        }

        fn udp_set_hop_limit(&self, _: Self::UdpSocket, _: u8) -> Result<(), UdpError> {
            Ok(())
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

        fn udp_send_address(
            &self,
            _: Self::UdpSocket,
            _: NetworkIpAddress,
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

        fn udp_join_multicast_v4(
            &self,
            _: Ipv4Address,
            _: Ipv4Address,
        ) -> impl Future<Output = Result<(), UdpError>> + Send + '_ {
            core::future::ready(Ok(()))
        }

        fn udp_leave_multicast_v4(
            &self,
            _: Ipv4Address,
            _: Ipv4Address,
        ) -> impl Future<Output = Result<(), UdpError>> + Send + '_ {
            core::future::ready(Ok(()))
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
                | NetworkAuthorityRights::MULTICAST
                | NetworkAuthorityRights::PRIVILEGED_BIND,
        );
        let tcp = authority.derive_tcp_cap().unwrap();
        let udp = authority.derive_udp_cap().unwrap();
        let dns = authority.derive_dns_cap().unwrap();
        let multicast = authority.derive_multicast_cap().unwrap();
        let privileged = authority.derive_privileged_bind_cap().unwrap();
        let stack = SocketStack::new(TestNetworkService);

        // A dual-family lookup surfaces both records, in resolver order.
        assert_eq!(
            block_on(stack.dns_resolve(dns, "localhost", 1)).unwrap(),
            vec![
                NetworkIpAddress::Ipv4(Ipv4Address::new([127, 0, 0, 1])),
                NetworkIpAddress::Ipv6(helios_netstack::Ipv6Address::LOOPBACK),
            ]
        );
        assert_eq!(
            block_on(stack.tcp_connect(tcp, "localhost", 80, 1)).unwrap(),
            7
        );
        assert_eq!(
            block_on(stack.tcp_connect_from(
                tcp,
                "localhost",
                80,
                4040,
                helios_netstack::DEFAULT_HOP_LIMIT,
                1,
            )).unwrap(),
            4040
        );
        let listener =
            block_on(stack.tcp_listen(
                tcp,
                Some(privileged),
                TCP_ANY_V4,
                53,
                1,
                helios_netstack::DEFAULT_HOP_LIMIT,
            )).unwrap();
        assert_eq!(listener.local_port, 53);
        assert_eq!(
            block_on(stack.tcp_accept(tcp, listener.listener, 1))
                .unwrap()
                .stream,
            9
        );
        assert_eq!(
            block_on(stack.udp_bind(udp, Some(privileged), 53))
                .unwrap()
                .local_port,
            53
        );
        let group = Ipv4Address::new([224, 0, 0, 251]);
        let interface = Ipv4Address::new([0, 0, 0, 0]);
        block_on(stack.udp_join_multicast_v4(udp, multicast, group, interface)).unwrap();
        block_on(stack.udp_leave_multicast_v4(udp, multicast, group, interface)).unwrap();
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
    fn low_tcp_listen_requires_privileged_bind_cap() {
        let mut authority = ProcessAuthority::empty();
        authority.grant_network_rights(NetworkAuthorityRights::TCP);
        let tcp = authority.derive_tcp_cap().unwrap();
        let stack = SocketStack::new(TestNetworkService);

        let error = block_on(stack.tcp_listen(
            tcp,
            None,
            TCP_ANY_V4,
            53,
            1,
            helios_netstack::DEFAULT_HOP_LIMIT,
        )).unwrap_err();
        assert_eq!(error.kind, crate::TcpErrorKind::PermissionDenied);
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

    #[test]
    fn high_tcp_listen_does_not_require_privileged_bind_cap() {
        let mut authority = ProcessAuthority::empty();
        authority.grant_network_rights(NetworkAuthorityRights::TCP);
        let tcp = authority.derive_tcp_cap().unwrap();
        let stack = SocketStack::new(TestNetworkService);

        assert_eq!(
            block_on(stack.tcp_listen(
                tcp,
                None,
                TCP_ANY_V4,
                8080,
                1,
                helios_netstack::DEFAULT_HOP_LIMIT,
            ))
                .unwrap()
                .local_port,
            8080
        );
    }
}
