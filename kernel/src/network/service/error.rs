use super::*;

pub(super) enum NetworkConfigurationError {
    Device(IoError),
    Control(NetworkControlError),
}

pub(super) fn map_ipv4_address(address: Ipv4Address) -> KernelIpv4Address {
    KernelIpv4Address::new(address.octets())
}

pub(super) fn map_ip_address(address: IpAddress) -> NetworkIpAddress {
    match address {
        IpAddress::Ipv4(address) => NetworkIpAddress::Ipv4(map_ipv4_address(address)),
        IpAddress::Ipv6(address) => NetworkIpAddress::Ipv6(address),
    }
}

pub(super) fn map_network_ip_address(address: NetworkIpAddress) -> IpAddress {
    match address {
        NetworkIpAddress::Ipv4(address) => IpAddress::Ipv4(map_kernel_ipv4_address(address)),
        NetworkIpAddress::Ipv6(address) => IpAddress::Ipv6(address),
    }
}

pub(super) fn map_kernel_ipv4_address(address: KernelIpv4Address) -> Ipv4Address {
    Ipv4Address::new(address.octets())
}

pub(super) fn map_kernel_ipv4_cidr(cidr: KernelIpv4Cidr) -> Ipv4Cidr {
    Ipv4Cidr::new(map_kernel_ipv4_address(cidr.address()), cidr.prefix_len())
}

pub(super) fn map_ipv4_cidr(cidr: Ipv4Cidr) -> KernelIpv4Cidr {
    KernelIpv4Cidr::new(map_ipv4_address(cidr.address()), cidr.prefix_len())
}

pub(super) fn map_tcp_connect_terminal_error(error: TcpConnectTerminalError) -> TcpError {
    match error {
        TcpConnectTerminalError::RemotePortUnreachable => TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpRemotePortUnreachable,
        },
        TcpConnectTerminalError::RemoteNetworkUnreachable => TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpRemoteNetworkUnreachable,
        },
        TcpConnectTerminalError::RemoteHostUnreachable => TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpRemoteHostUnreachable,
        },
        TcpConnectTerminalError::RemoteProtocolUnreachable => TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpRemoteProtocolUnreachable,
        },
        TcpConnectTerminalError::RemoteCommunicationProhibited => TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpRemoteCommunicationProhibited,
        },
        TcpConnectTerminalError::Closed => TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpClosedDuringConnect,
        },
    }
}

pub(super) fn map_udp_socket_error(error: UdpSocketError) -> UdpError {
    match error {
        UdpSocketError::RemotePortUnreachable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpRemotePortUnreachable,
        },
        UdpSocketError::RemoteNetworkUnreachable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpRemoteNetworkUnreachable,
        },
        UdpSocketError::RemoteHostUnreachable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpRemoteHostUnreachable,
        },
        UdpSocketError::RemoteProtocolUnreachable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpRemoteProtocolUnreachable,
        },
        UdpSocketError::RemoteCommunicationProhibited => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpRemoteCommunicationProhibited,
        },
    }
}

pub(super) fn map_udp_bind_error(error: StackError) -> UdpError {
    match error {
        StackError::AddressInUse => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpPortInUse,
        },
        StackError::UnknownSocket => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownUdpSocket,
        },
        StackError::AddressFamilyMismatch | StackError::RemoteAddressMismatch => UdpError {
            kind: UdpErrorKind::Unsupported,
            detail: NetworkErrorDetail::UnsupportedAddressFamily,
        },
        StackError::Unroutable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::NetworkServiceUnavailable,
        },
        StackError::PacketTooLarge => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpDatagramTooLarge,
        },
        _ => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpBindFailed,
        },
    }
}

pub(super) fn map_udp_connect_error(error: StackError) -> UdpError {
    match error {
        StackError::AddressInUse => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpPortInUse,
        },
        StackError::UnknownSocket => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownUdpSocket,
        },
        StackError::AddressFamilyMismatch | StackError::RemoteAddressMismatch => UdpError {
            kind: UdpErrorKind::Unsupported,
            detail: NetworkErrorDetail::UnsupportedAddressFamily,
        },
        StackError::Unroutable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::NetworkServiceUnavailable,
        },
        StackError::PacketTooLarge => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpDatagramTooLarge,
        },
        _ => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpConnectFailed,
        },
    }
}

pub(super) fn map_udp_disconnect_error(error: StackError) -> UdpError {
    match error {
        StackError::AddressInUse => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpPortInUse,
        },
        StackError::UnknownSocket => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownUdpSocket,
        },
        StackError::AddressFamilyMismatch | StackError::RemoteAddressMismatch => UdpError {
            kind: UdpErrorKind::Unsupported,
            detail: NetworkErrorDetail::UnsupportedAddressFamily,
        },
        StackError::Unroutable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::NetworkServiceUnavailable,
        },
        StackError::PacketTooLarge => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpDatagramTooLarge,
        },
        _ => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpDisconnectFailed,
        },
    }
}

pub(super) fn map_udp_send_error(error: StackError) -> UdpError {
    match error {
        StackError::UnknownSocket => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownUdpSocket,
        },
        StackError::AddressFamilyMismatch | StackError::RemoteAddressMismatch => UdpError {
            kind: UdpErrorKind::Unsupported,
            detail: NetworkErrorDetail::UnsupportedAddressFamily,
        },
        StackError::Unroutable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::NetworkServiceUnavailable,
        },
        StackError::PacketTooLarge => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpDatagramTooLarge,
        },
        _ => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpQueueFailed,
        },
    }
}

pub(super) fn multicast_join_error(error: StackError) -> UdpError {
    let detail = match error {
        StackError::MulticastInterfaceUnavailable => {
            NetworkErrorDetail::UdpMulticastInterfaceUnavailable
        }
        StackError::InvalidMulticastGroup | StackError::MulticastMembershipTableFull => {
            NetworkErrorDetail::UdpMulticastJoinFailed
        }
        _ => NetworkErrorDetail::UdpMulticastJoinFailed,
    };
    UdpError {
        kind: UdpErrorKind::Unavailable,
        detail,
    }
}

pub(super) fn multicast_leave_error(error: StackError) -> UdpError {
    let detail = match error {
        StackError::MulticastInterfaceUnavailable => {
            NetworkErrorDetail::UdpMulticastInterfaceUnavailable
        }
        StackError::InvalidMulticastGroup | StackError::MulticastMembershipNotFound => {
            NetworkErrorDetail::UdpMulticastLeaveFailed
        }
        _ => NetworkErrorDetail::UdpMulticastLeaveFailed,
    };
    UdpError {
        kind: UdpErrorKind::Unavailable,
        detail,
    }
}

pub(super) fn require_local_network_port(port: NetworkPortId) -> Result<(), NetworkControlError> {
    if port == LOCAL_NETWORK_PORT {
        Ok(())
    } else {
        Err(NetworkControlError::PortUnavailable)
    }
}

pub(super) fn ipv4_mask_prefix_len(mask: Ipv4Address) -> Result<u8, NetworkControlError> {
    let raw = u32::from_be_bytes(mask.octets());
    let prefix_len = raw.leading_ones() as u8;
    let expected = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len)
    };
    if raw == expected {
        Ok(prefix_len)
    } else {
        Err(NetworkControlError::InvalidAddress)
    }
}

pub(super) fn ping_configuration_timeout() -> PingError {
    PingError {
        kind: PingErrorKind::Timeout,
        detail: NetworkErrorDetail::NetworkConfigurationTimeout,
    }
}

pub(super) fn ping_configuration_error(error: NetworkConfigurationError) -> PingError {
    match error {
        NetworkConfigurationError::Device(error) => {
            PingError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
        }
        NetworkConfigurationError::Control(error) => PingError {
            kind: PingErrorKind::Unavailable,
            detail: network_configuration_control_detail(error),
        },
    }
}

pub(super) fn dns_configuration_timeout() -> DnsError {
    DnsError {
        kind: DnsErrorKind::Timeout,
        detail: NetworkErrorDetail::NetworkConfigurationTimeout,
    }
}

pub(super) fn dns_configuration_error(error: NetworkConfigurationError) -> DnsError {
    match error {
        NetworkConfigurationError::Device(error) => {
            DnsError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
        }
        NetworkConfigurationError::Control(error) => DnsError {
            kind: DnsErrorKind::Unavailable,
            detail: network_configuration_control_detail(error),
        },
    }
}

pub(super) fn tcp_configuration_timeout() -> TcpError {
    TcpError {
        kind: TcpErrorKind::Timeout,
        detail: NetworkErrorDetail::NetworkConfigurationTimeout,
    }
}

pub(super) fn tcp_configuration_error(error: NetworkConfigurationError) -> TcpError {
    match error {
        NetworkConfigurationError::Device(error) => {
            TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
        }
        NetworkConfigurationError::Control(error) => TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: network_configuration_control_detail(error),
        },
    }
}

pub(super) fn udp_configuration_timeout() -> UdpError {
    UdpError {
        kind: UdpErrorKind::Timeout,
        detail: NetworkErrorDetail::NetworkConfigurationTimeout,
    }
}

pub(super) fn udp_configuration_error(error: NetworkConfigurationError) -> UdpError {
    match error {
        NetworkConfigurationError::Device(error) => {
            UdpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
        }
        NetworkConfigurationError::Control(error) => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: network_configuration_control_detail(error),
        },
    }
}

pub(super) fn network_configuration_control_detail(
    error: NetworkControlError,
) -> NetworkErrorDetail {
    match error {
        NetworkControlError::PortUnavailable
        | NetworkControlError::BridgeUnavailable
        | NetworkControlError::InvalidBridgeRequest
        | NetworkControlError::InvalidAddress
        | NetworkControlError::InvalidRoute
        | NetworkControlError::RouteTimestampOutOfRange
        | NetworkControlError::BackendFault => NetworkErrorDetail::NetworkServiceUnavailable,
    }
}

pub(super) fn parse_ipv4(input: &str) -> Option<Ipv4Address> {
    let mut octets = [0u8; 4];
    let mut parts = input.split('.');
    for octet in &mut octets {
        let part = parts.next()?;
        *octet = part.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(Ipv4Address::new(octets))
}

pub(super) fn parse_ipv6(input: &str) -> Option<Ipv6Address> {
    input
        .parse::<core::net::Ipv6Addr>()
        .ok()
        .map(|address| Ipv6Address::new(address.octets()))
}
