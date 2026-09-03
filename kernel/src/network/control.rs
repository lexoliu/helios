extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use thiserror::Error;

use crate::NetworkAdminCap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NetworkPortId(u32);

impl NetworkPortId {
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NetworkBridgeSecurity(u8);

impl NetworkBridgeSecurity {
    pub const UNENCRYPTED: Self = Self(1 << 0);
    pub const ANY_ENCRYPTION: Self = Self(1 << 1);
    pub const CLASSIC_ENCRYPTION: Self = Self(1 << 2);
    pub const DOUBLE_ENCRYPTION: Self = Self(1 << 3);

    pub const fn new(raw: u8) -> Result<Self, NetworkControlError> {
        match raw {
            1 | 2 | 4 | 8 => Ok(Self(raw)),
            _ => Err(NetworkControlError::InvalidBridgeRequest),
        }
    }

    pub const fn raw(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkBridgeRequest {
    network: String,
    token: String,
    security: NetworkBridgeSecurity,
}

impl NetworkBridgeRequest {
    pub const fn new(network: String, token: String, security: NetworkBridgeSecurity) -> Self {
        Self {
            network,
            token,
            security,
        }
    }

    pub fn network(&self) -> &str {
        &self.network
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub const fn security(&self) -> NetworkBridgeSecurity {
        self.security
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacAddress {
    octets: [u8; 6],
}

impl MacAddress {
    pub const fn new(octets: [u8; 6]) -> Self {
        Self { octets }
    }

    pub const fn octets(self) -> [u8; 6] {
        self.octets
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Cidr {
    address: crate::Ipv4Address,
    prefix_len: u8,
}

impl Ipv4Cidr {
    pub const fn new(address: crate::Ipv4Address, prefix_len: u8) -> Self {
        assert!(prefix_len <= 32, "IPv4 CIDR prefix length must be <= 32");
        Self {
            address,
            prefix_len,
        }
    }

    pub const fn address(self) -> crate::Ipv4Address {
        self.address
    }

    pub const fn prefix_len(self) -> u8 {
        self.prefix_len
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Route {
    destination: Ipv4Cidr,
    gateway: crate::Ipv4Address,
    preferred_until_nanos: Option<u64>,
    expires_at_nanos: Option<u64>,
}

impl Ipv4Route {
    pub const fn new(destination: Ipv4Cidr, gateway: crate::Ipv4Address) -> Self {
        Self {
            destination,
            gateway,
            preferred_until_nanos: None,
            expires_at_nanos: None,
        }
    }

    pub const fn with_lifetimes(
        destination: Ipv4Cidr,
        gateway: crate::Ipv4Address,
        preferred_until_nanos: Option<u64>,
        expires_at_nanos: Option<u64>,
    ) -> Self {
        Self {
            destination,
            gateway,
            preferred_until_nanos,
            expires_at_nanos,
        }
    }

    pub const fn destination(self) -> Ipv4Cidr {
        self.destination
    }

    pub const fn gateway(self) -> crate::Ipv4Address {
        self.gateway
    }

    pub const fn preferred_until_nanos(self) -> Option<u64> {
        self.preferred_until_nanos
    }

    pub const fn expires_at_nanos(self) -> Option<u64> {
        self.expires_at_nanos
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum NetworkControlError {
    #[error("network port is unavailable")]
    PortUnavailable,
    #[error("network bridge is unavailable")]
    BridgeUnavailable,
    #[error("network bridge request is invalid")]
    InvalidBridgeRequest,
    #[error("address is invalid for this port")]
    InvalidAddress,
    #[error("route is invalid for this port")]
    InvalidRoute,
    #[error("route timestamp is outside backend range")]
    RouteTimestampOutOfRange,
    #[error("network control backend failed")]
    BackendFault,
}

pub trait NetworkAdminBackend: Clone + Send + 'static {
    /// Per-shard frame and interrupt counters, one entry per processor.
    ///
    /// Part of the admin surface rather than the socket surface because
    /// it describes the interface, not a connection: it is what says
    /// whether the device is steering flows across processors or piling
    /// them on one.
    fn network_stats(&self) -> crate::NetworkStats;

    fn bridge_port(
        &self,
        port: NetworkPortId,
        bridge: NetworkBridgeRequest,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send;

    fn unbridge_port(
        &self,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send;

    fn acquire_dhcp(
        &self,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<Ipv4Cidr, NetworkControlError>> + Send;

    fn add_address(
        &self,
        port: NetworkPortId,
        address: Ipv4Cidr,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send;

    fn remove_address(
        &self,
        port: NetworkPortId,
        address: Ipv4Cidr,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send;

    fn clear_addresses(
        &self,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send;

    fn list_addresses(
        &self,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<Vec<Ipv4Cidr>, NetworkControlError>> + Send;

    fn mac_address(
        &self,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<MacAddress, NetworkControlError>> + Send;

    fn set_gateway(
        &self,
        port: NetworkPortId,
        gateway: crate::Ipv4Address,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send;

    fn add_route(
        &self,
        port: NetworkPortId,
        route: Ipv4Route,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send;

    fn remove_route(
        &self,
        port: NetworkPortId,
        route: Ipv4Route,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send;

    fn clear_routes(
        &self,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send;

    fn list_routes(
        &self,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<Vec<Ipv4Route>, NetworkControlError>> + Send;
}

#[derive(Clone)]
pub struct NetworkControl<Backend> {
    backend: Backend,
}

impl<Backend> NetworkControl<Backend>
where
    Backend: NetworkAdminBackend,
{
    pub const fn new(backend: Backend) -> Self {
        Self { backend }
    }

    pub fn bridge_port(
        &self,
        _: NetworkAdminCap,
        port: NetworkPortId,
        bridge: NetworkBridgeRequest,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.backend.bridge_port(port, bridge)
    }

    pub fn unbridge_port(
        &self,
        _: NetworkAdminCap,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.backend.unbridge_port(port)
    }

    pub fn acquire_dhcp(
        &self,
        _: NetworkAdminCap,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<Ipv4Cidr, NetworkControlError>> + Send {
        self.backend.acquire_dhcp(port)
    }

    pub fn add_address(
        &self,
        _: NetworkAdminCap,
        port: NetworkPortId,
        address: Ipv4Cidr,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.backend.add_address(port, address)
    }

    pub fn remove_address(
        &self,
        _: NetworkAdminCap,
        port: NetworkPortId,
        address: Ipv4Cidr,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.backend.remove_address(port, address)
    }

    pub fn clear_addresses(
        &self,
        _: NetworkAdminCap,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.backend.clear_addresses(port)
    }

    pub fn list_addresses(
        &self,
        _: NetworkAdminCap,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<Vec<Ipv4Cidr>, NetworkControlError>> + Send {
        self.backend.list_addresses(port)
    }

    pub fn mac_address(
        &self,
        _: NetworkAdminCap,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<MacAddress, NetworkControlError>> + Send {
        self.backend.mac_address(port)
    }

    pub fn set_gateway(
        &self,
        _: NetworkAdminCap,
        port: NetworkPortId,
        gateway: crate::Ipv4Address,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.backend.set_gateway(port, gateway)
    }

    pub fn add_route(
        &self,
        _: NetworkAdminCap,
        port: NetworkPortId,
        route: Ipv4Route,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.backend.add_route(port, route)
    }

    pub fn remove_route(
        &self,
        _: NetworkAdminCap,
        port: NetworkPortId,
        route: Ipv4Route,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.backend.remove_route(port, route)
    }

    pub fn clear_routes(
        &self,
        _: NetworkAdminCap,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<(), NetworkControlError>> + Send {
        self.backend.clear_routes(port)
    }

    pub fn list_routes(
        &self,
        _: NetworkAdminCap,
        port: NetworkPortId,
    ) -> impl Future<Output = Result<Vec<Ipv4Route>, NetworkControlError>> + Send {
        self.backend.list_routes(port)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;
    use futures_lite::future::block_on;

    use super::{
        Ipv4Cidr, Ipv4Route, MacAddress, NetworkAdminBackend, NetworkBridgeRequest,
        NetworkBridgeSecurity, NetworkControl, NetworkControlError, NetworkPortId,
    };
    use crate::{Ipv4Address, NetworkAuthorityRights, ProcessAuthority};

    #[derive(Clone, Copy)]
    struct TestBackend;

    impl NetworkAdminBackend for TestBackend {
        fn network_stats(&self) -> crate::NetworkStats {
            crate::NetworkStats::default()
        }

        async fn bridge_port(
            &self,
            _: NetworkPortId,
            _: NetworkBridgeRequest,
        ) -> Result<(), NetworkControlError> {
            Ok(())
        }

        async fn unbridge_port(&self, _: NetworkPortId) -> Result<(), NetworkControlError> {
            Ok(())
        }

        async fn acquire_dhcp(&self, _: NetworkPortId) -> Result<Ipv4Cidr, NetworkControlError> {
            Ok(Ipv4Cidr::new(Ipv4Address::new([10, 0, 0, 2]), 24))
        }

        async fn add_address(
            &self,
            _: NetworkPortId,
            _: Ipv4Cidr,
        ) -> Result<(), NetworkControlError> {
            Ok(())
        }

        async fn remove_address(
            &self,
            _: NetworkPortId,
            _: Ipv4Cidr,
        ) -> Result<(), NetworkControlError> {
            Ok(())
        }

        async fn clear_addresses(&self, _: NetworkPortId) -> Result<(), NetworkControlError> {
            Ok(())
        }

        async fn list_addresses(
            &self,
            _: NetworkPortId,
        ) -> Result<Vec<Ipv4Cidr>, NetworkControlError> {
            Ok(vec![Ipv4Cidr::new(Ipv4Address::new([10, 0, 0, 2]), 24)])
        }

        async fn mac_address(&self, _: NetworkPortId) -> Result<MacAddress, NetworkControlError> {
            Ok(MacAddress::new([2, 0, 0, 0, 0, 1]))
        }

        async fn set_gateway(
            &self,
            _: NetworkPortId,
            _: Ipv4Address,
        ) -> Result<(), NetworkControlError> {
            Ok(())
        }

        async fn add_route(
            &self,
            _: NetworkPortId,
            _: Ipv4Route,
        ) -> Result<(), NetworkControlError> {
            Ok(())
        }

        async fn remove_route(
            &self,
            _: NetworkPortId,
            _: Ipv4Route,
        ) -> Result<(), NetworkControlError> {
            Ok(())
        }

        async fn clear_routes(&self, _: NetworkPortId) -> Result<(), NetworkControlError> {
            Ok(())
        }

        async fn list_routes(
            &self,
            _: NetworkPortId,
        ) -> Result<Vec<Ipv4Route>, NetworkControlError> {
            Ok(vec![Ipv4Route::new(
                Ipv4Cidr::new(Ipv4Address::new([0, 0, 0, 0]), 0),
                Ipv4Address::new([10, 0, 0, 1]),
            )])
        }
    }

    #[test]
    fn network_control_operations_require_admin_capability() {
        let mut authority = ProcessAuthority::empty();
        authority.grant_network_rights(NetworkAuthorityRights::ADMIN);
        let cap = authority
            .derive_network_admin_cap()
            .expect("network admin cap must derive from ADMIN right");
        let control = NetworkControl::new(TestBackend);
        let port = NetworkPortId::new(1);
        let bridge = NetworkBridgeRequest::new(
            "edge.example".into(),
            "token".into(),
            NetworkBridgeSecurity::ANY_ENCRYPTION,
        );

        block_on(control.bridge_port(cap, port, bridge))
            .expect("bridge should succeed with admin cap");
        let route = Ipv4Route::new(
            Ipv4Cidr::new(Ipv4Address::new([0, 0, 0, 0]), 0),
            Ipv4Address::new([10, 0, 0, 1]),
        );
        block_on(control.add_route(cap, port, route)).expect("route should succeed with admin cap");
        assert_eq!(
            block_on(control.mac_address(cap, port))
                .expect("MAC query should succeed")
                .octets(),
            [2, 0, 0, 0, 0, 1]
        );
    }

    #[test]
    fn ipv4_route_preserves_lifetime_metadata() {
        let route = Ipv4Route::with_lifetimes(
            Ipv4Cidr::new(Ipv4Address::new([192, 0, 2, 0]), 24),
            Ipv4Address::new([192, 0, 2, 1]),
            Some(1_000),
            Some(2_000),
        );

        assert_eq!(route.preferred_until_nanos(), Some(1_000));
        assert_eq!(route.expires_at_nanos(), Some(2_000));
    }
}
