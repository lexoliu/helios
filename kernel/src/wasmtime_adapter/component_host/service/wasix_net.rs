use super::*;

pub(super) fn add_wasix_port_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_bridge",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (network, network_len, token, token_len, security): (i32, i32, i32, i32, i32)| {
                Box::new(async move {
                    wasix_port_bridge(
                        &mut caller,
                        network as u32,
                        network_len as u32,
                        token as u32,
                        token_len as u32,
                        security,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_unbridge",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ()| {
                Box::new(async move { wasix_port_unbridge(&mut caller).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_dhcp_acquire",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ()| {
                Box::new(async move { wasix_port_dhcp_acquire(&mut caller).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_addr_add",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, (addr,): (i32,)| {
                Box::new(async move { wasix_port_addr_add(&mut caller, addr as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_addr_remove",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, (addr,): (i32,)| {
                Box::new(async move { wasix_port_addr_remove(&mut caller, addr as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_addr_clear",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ()| {
                Box::new(async move { wasix_port_addr_clear(&mut caller).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_mac",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, (ret_mac,): (i32,)| {
                Box::new(async move { wasix_port_mac(&mut caller, ret_mac as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_addr_list",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (addrs, naddrs): (i32, i32)| {
                Box::new(async move {
                    wasix_port_addr_list(&mut caller, addrs as u32, naddrs as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_gateway_set",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, (addr,): (i32,)| {
                Box::new(async move { wasix_port_gateway_set(&mut caller, addr as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_route_add",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (cidr, router, preferred, expires): (i32, i32, i32, i32)| {
                Box::new(async move {
                    wasix_port_route_add(
                        &mut caller,
                        cidr as u32,
                        router as u32,
                        preferred as u32,
                        expires as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_route_remove",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, (cidr,): (i32,)| {
                Box::new(async move { wasix_port_route_remove(&mut caller, cidr as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_route_clear",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ()| {
                Box::new(async move { wasix_port_route_clear(&mut caller).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_route_list",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (routes, nroutes): (i32, i32)| {
                Box::new(async move {
                    wasix_port_route_list(&mut caller, routes as u32, nroutes as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

pub(super) fn add_wasix_socket_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_status",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             ret_status: i32|
             -> i32 { wasix_sock_status(&mut caller, fd, ret_status as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_addr_local",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             ret_addr: i32|
             -> i32 { wasix_sock_addr_local(&mut caller, fd, ret_addr as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_addr_peer",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             ret_addr: i32|
             -> i32 { wasix_sock_addr_peer(&mut caller, fd, ret_addr as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_open",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             af: i32,
             socktype: i32,
             proto: i32,
             ret_fd: i32|
             -> i32 {
                wasix_sock_open(&mut caller, af, socktype, proto, ret_fd as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_pair",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             af: i32,
             socktype: i32,
             proto: i32,
             ret_fd0: i32,
             ret_fd1: i32|
             -> i32 {
                wasix_sock_pair(
                    &mut caller,
                    af,
                    socktype,
                    proto,
                    ret_fd0 as u32,
                    ret_fd1 as u32,
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_set_opt_flag",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             option: i32,
             flag: i32|
             -> i32 { wasix_sock_set_opt_flag(&mut caller, fd, option, flag) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_get_opt_flag",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             option: i32,
             ret_flag: i32|
             -> i32 {
                wasix_sock_get_opt_flag(&mut caller, fd, option, ret_flag as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_set_opt_time",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             option: i32,
             time: i32|
             -> i32 { wasix_sock_set_opt_time(&mut caller, fd, option, time as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_get_opt_time",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             option: i32,
             ret_time: i32|
             -> i32 {
                wasix_sock_get_opt_time(&mut caller, fd, option, ret_time as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_set_opt_size",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             option: i32,
             size: i64|
             -> i32 { wasix_sock_set_opt_size(&mut caller, fd, option, size) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_get_opt_size",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             option: i32,
             ret_size: i32|
             -> i32 {
                wasix_sock_get_opt_size(&mut caller, fd, option, ret_size as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_join_multicast_v4",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, multiaddr, interface): (i32, i32, i32)| {
                Box::new(async move {
                    wasix_sock_multicast_v4(
                        &mut caller,
                        fd,
                        multiaddr as u32,
                        interface as u32,
                        true,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_leave_multicast_v4",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, multiaddr, interface): (i32, i32, i32)| {
                Box::new(async move {
                    wasix_sock_multicast_v4(
                        &mut caller,
                        fd,
                        multiaddr as u32,
                        interface as u32,
                        false,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_join_multicast_v6",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             multiaddr: i32,
             interface: i32|
             -> i32 {
                wasix_sock_multicast_v6(&mut caller, fd, multiaddr as u32, interface as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_leave_multicast_v6",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             multiaddr: i32,
             interface: i32|
             -> i32 {
                wasix_sock_multicast_v6(&mut caller, fd, multiaddr as u32, interface as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_bind",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, addr): (i32, i32)| {
                Box::new(async move { wasix_sock_bind(&mut caller, fd, addr as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_listen",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, backlog): (i32, i32)| {
                Box::new(async move { wasix_sock_listen(&mut caller, fd, backlog).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_accept_v2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, _flags, ret_fd, ret_addr): (i32, i32, i32, i32)| {
                Box::new(async move {
                    wasix_sock_accept_v2(&mut caller, fd, ret_fd as u32, ret_addr as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_connect",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, addr): (i32, i32)| {
                Box::new(async move { wasix_sock_connect(&mut caller, fd, addr as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_recv_from",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, iovs, iovs_len, flags, ret_size, ret_flags, ret_addr): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    wasix_sock_recv_from(
                        &mut caller,
                        fd,
                        iovs as u32,
                        iovs_len as u32,
                        flags as u16,
                        ret_size as u32,
                        ret_flags as u32,
                        ret_addr as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_send_to",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, iovs, iovs_len, flags, addr, ret_size): (i32, i32, i32, i32, i32, i32)| {
                Box::new(async move {
                    wasix_sock_send_to(
                        &mut caller,
                        fd,
                        iovs as u32,
                        iovs_len as u32,
                        flags as u16,
                        addr as u32,
                        ret_size as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_send_file",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (out_fd, in_fd, offset, count, ret_size): (i32, i32, i64, i64, i32)| {
                Box::new(async move {
                    wasix_sock_send_file(&mut caller, out_fd, in_fd, offset, count, ret_size as u32)
                        .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

pub(super) async fn wasix_resolve<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    host: u32,
    host_len: u32,
    port: i32,
    addrs: u32,
    naddrs: u32,
    ret_naddrs: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_dns_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    if u16::try_from(port).is_err() {
        return p1::errno::INVAL;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let host = match p1_read_path(caller, memory, host, host_len) {
        Ok(host) => host,
        Err(_) => return p1::errno::FAULT,
    };
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::NETDOWN;
    };
    let resolved = match service.dns_resolve(&host, u64::MAX).await {
        Ok(resolved) => resolved,
        Err(error) => return p1_errno_from_dns_error(error),
    };
    let returned = resolved.len().min(naddrs as usize);
    for (index, address) in resolved.iter().take(returned).enumerate() {
        let offset = match (index as u32).checked_mul(WASIX_ADDR_IP_SIZE) {
            Some(offset) => offset,
            None => return p1::errno::OVERFLOW,
        };
        let entry = match addrs.checked_add(offset) {
            Some(entry) => entry,
            None => return p1::errno::OVERFLOW,
        };
        let status = write_wasix_addr_ip4(caller, memory, entry, *address);
        if status != p1::errno::SUCCESS {
            return status;
        }
    }
    let returned = match u32::try_from(returned) {
        Ok(returned) => returned,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, ret_naddrs, returned)
}

pub(super) fn wasix_network_admin_service<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(crate::NetworkAdminCap, ComponentHostNetworkService), i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let cap = caller
        .data()
        .authority
        .derive_network_admin_cap()
        .map_err(|_| p1::errno::NOTCAPABLE)?;
    let service = caller
        .data()
        .runtime_state
        .network_service()
        .ok_or(p1::errno::NETDOWN)?;
    Ok((cap, service))
}

pub(super) fn p1_errno_from_network_control_error(error: crate::NetworkControlError) -> i32 {
    match error {
        crate::NetworkControlError::PortUnavailable => p1::errno::NETDOWN,
        crate::NetworkControlError::BridgeUnavailable => p1::errno::NOTSUP,
        crate::NetworkControlError::InvalidBridgeRequest => p1::errno::INVAL,
        crate::NetworkControlError::InvalidAddress
        | crate::NetworkControlError::InvalidRoute
        | crate::NetworkControlError::RouteTimestampOutOfRange => p1::errno::INVAL,
        crate::NetworkControlError::BackendFault => p1::errno::IO,
    }
}

pub(super) fn wasix_bridge_security(raw: i32) -> Result<crate::NetworkBridgeSecurity, i32> {
    let raw = u8::try_from(raw).map_err(|_| p1::errno::INVAL)?;
    match raw {
        WASIX_STREAM_SECURITY_UNENCRYPTED
        | WASIX_STREAM_SECURITY_ANY_ENCRYPTION
        | WASIX_STREAM_SECURITY_CLASSIC_ENCRYPTION
        | WASIX_STREAM_SECURITY_DOUBLE_ENCRYPTION => {
            crate::NetworkBridgeSecurity::new(raw).map_err(p1_errno_from_network_control_error)
        }
        _ => Err(p1::errno::INVAL),
    }
}

pub(super) async fn wasix_port_bridge<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    network: u32,
    network_len: u32,
    token: u32,
    token_len: u32,
    security: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let security = match wasix_bridge_security(security) {
        Ok(security) => security,
        Err(status) => return status,
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let network = match wasix_read_exec_string(caller, memory, network, network_len) {
        Ok(network) => network,
        Err(_) => return p1::errno::FAULT,
    };
    let token = match wasix_read_exec_string(caller, memory, token, token_len) {
        Ok(token) => token,
        Err(_) => return p1::errno::FAULT,
    };
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return status,
    };
    let request = crate::NetworkBridgeRequest::new(network, token, security);
    let control = crate::NetworkControl::new(service);
    match control
        .bridge_port(cap, crate::NetworkPortId::new(0), request)
        .await
    {
        Ok(()) => p1::errno::SUCCESS,
        Err(error) => p1_errno_from_network_control_error(error),
    }
}

pub(super) async fn wasix_port_unbridge<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return status,
    };
    let control = crate::NetworkControl::new(service);
    match control
        .unbridge_port(cap, crate::NetworkPortId::new(0))
        .await
    {
        Ok(()) => p1::errno::SUCCESS,
        Err(error) => p1_errno_from_network_control_error(error),
    }
}

pub(super) async fn wasix_port_dhcp_acquire<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return status,
    };
    let control = crate::NetworkControl::new(service);
    match control
        .acquire_dhcp(cap, crate::NetworkPortId::new(0))
        .await
    {
        Ok(_) => p1::errno::SUCCESS,
        Err(error) => p1_errno_from_network_control_error(error),
    }
}

pub(super) async fn wasix_port_addr_add<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let cidr = match wasix_read_addr_cidr_ip4(caller, memory, addr) {
        Ok(cidr) => cidr,
        Err(status) => return status,
    };
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return status,
    };
    let control = crate::NetworkControl::new(service);
    match control
        .add_address(cap, crate::NetworkPortId::new(0), cidr)
        .await
    {
        Ok(()) => p1::errno::SUCCESS,
        Err(error) => p1_errno_from_network_control_error(error),
    }
}

pub(super) async fn wasix_port_addr_remove<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let address = match wasix_read_addr_ip4(caller, memory, addr) {
        Ok(address) => address,
        Err(status) => return status,
    };
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return status,
    };
    let control = crate::NetworkControl::new(service);
    let cidr = crate::Ipv4Cidr::new(address, 32);
    match control
        .remove_address(cap, crate::NetworkPortId::new(0), cidr)
        .await
    {
        Ok(()) => p1::errno::SUCCESS,
        Err(error) => p1_errno_from_network_control_error(error),
    }
}

pub(super) async fn wasix_port_addr_clear<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return status,
    };
    let control = crate::NetworkControl::new(service);
    match control
        .clear_addresses(cap, crate::NetworkPortId::new(0))
        .await
    {
        Ok(()) => p1::errno::SUCCESS,
        Err(error) => p1_errno_from_network_control_error(error),
    }
}

pub(super) async fn wasix_port_mac<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_mac: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return status,
    };
    let control = crate::NetworkControl::new(service);
    let mac = match control.mac_address(cap, crate::NetworkPortId::new(0)).await {
        Ok(mac) => mac,
        Err(error) => return p1_errno_from_network_control_error(error),
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_memory(caller, memory, ret_mac, &mac.octets())
}

pub(super) async fn wasix_port_addr_list<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    addrs: u32,
    naddrs: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let capacity = match p1_try_read_u32(caller, memory, naddrs) {
        Ok(capacity) => capacity,
        Err(_) => return p1::errno::FAULT,
    };
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return status,
    };
    let control = crate::NetworkControl::new(service);
    let addresses = match control
        .list_addresses(cap, crate::NetworkPortId::new(0))
        .await
    {
        Ok(addresses) => addresses,
        Err(error) => return p1_errno_from_network_control_error(error),
    };
    let needed = match u32::try_from(addresses.len()) {
        Ok(needed) => needed,
        Err(_) => return p1::errno::OVERFLOW,
    };
    if needed > capacity {
        let status = p1_write_u32(caller, memory, naddrs, needed);
        if status != p1::errno::SUCCESS {
            return status;
        }
        return p1::errno::OVERFLOW;
    };
    for (index, cidr) in addresses.iter().enumerate() {
        let offset = match (index as u32).checked_mul(WASIX_ADDR_CIDR_SIZE) {
            Some(offset) => offset,
            None => return p1::errno::OVERFLOW,
        };
        let entry = match addrs.checked_add(offset) {
            Some(entry) => entry,
            None => return p1::errno::OVERFLOW,
        };
        let status = write_wasix_addr_cidr_ip4(caller, memory, entry, *cidr);
        if status != p1::errno::SUCCESS {
            return status;
        }
    }
    p1_write_u32(caller, memory, naddrs, needed)
}

pub(super) async fn wasix_port_gateway_set<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let gateway = match wasix_read_addr_ip4(caller, memory, addr) {
        Ok(gateway) => gateway,
        Err(status) => return status,
    };
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return status,
    };
    let control = crate::NetworkControl::new(service);
    match control
        .set_gateway(cap, crate::NetworkPortId::new(0), gateway)
        .await
    {
        Ok(()) => p1::errno::SUCCESS,
        Err(error) => p1_errno_from_network_control_error(error),
    }
}

pub(super) async fn wasix_port_route_add<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    cidr: u32,
    router: u32,
    preferred: u32,
    expires: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let destination = match wasix_read_addr_cidr_ip4(caller, memory, cidr) {
        Ok(destination) => destination,
        Err(status) => return status,
    };
    let gateway = match wasix_read_addr_ip4(caller, memory, router) {
        Ok(gateway) => gateway,
        Err(status) => return status,
    };
    let preferred = match wasix_read_optional_timestamp(caller, memory, preferred) {
        Ok(value) => value,
        Err(status) => return status,
    };
    let expires = match wasix_read_optional_timestamp(caller, memory, expires) {
        Ok(value) => value,
        Err(status) => return status,
    };
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return status,
    };
    let control = crate::NetworkControl::new(service);
    let route = crate::Ipv4Route::with_lifetimes(destination, gateway, preferred, expires);
    match control
        .add_route(cap, crate::NetworkPortId::new(0), route)
        .await
    {
        Ok(()) => p1::errno::SUCCESS,
        Err(error) => p1_errno_from_network_control_error(error),
    }
}

pub(super) async fn wasix_port_route_remove<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    cidr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let destination = match wasix_read_addr_ip4(caller, memory, cidr) {
        Ok(destination) => destination,
        Err(status) => return status,
    };
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return status,
    };
    let control = crate::NetworkControl::new(service);
    let routes = match control.list_routes(cap, crate::NetworkPortId::new(0)).await {
        Ok(routes) => routes,
        Err(error) => return p1_errno_from_network_control_error(error),
    };
    for route in routes {
        if route.destination().address() == destination {
            match control
                .remove_route(cap, crate::NetworkPortId::new(0), route)
                .await
            {
                Ok(()) => return p1::errno::SUCCESS,
                Err(error) => return p1_errno_from_network_control_error(error),
            }
        }
    }
    p1::errno::NOENT
}

pub(super) async fn wasix_port_route_clear<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return status,
    };
    let control = crate::NetworkControl::new(service);
    match control
        .clear_routes(cap, crate::NetworkPortId::new(0))
        .await
    {
        Ok(()) => p1::errno::SUCCESS,
        Err(error) => p1_errno_from_network_control_error(error),
    }
}

pub(super) async fn wasix_port_route_list<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    routes: u32,
    nroutes: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let capacity = match p1_try_read_u32(caller, memory, nroutes) {
        Ok(capacity) => capacity,
        Err(_) => return p1::errno::FAULT,
    };
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return status,
    };
    let control = crate::NetworkControl::new(service);
    let route_entries = match control.list_routes(cap, crate::NetworkPortId::new(0)).await {
        Ok(route_entries) => route_entries,
        Err(error) => return p1_errno_from_network_control_error(error),
    };
    let needed = match u32::try_from(route_entries.len()) {
        Ok(needed) => needed,
        Err(_) => return p1::errno::OVERFLOW,
    };
    if needed > capacity {
        let status = p1_write_u32(caller, memory, nroutes, needed);
        if status != p1::errno::SUCCESS {
            return status;
        }
        return p1::errno::OVERFLOW;
    }
    for (index, route) in route_entries.iter().enumerate() {
        let offset = match (index as u32).checked_mul(WASIX_ROUTE_SIZE) {
            Some(offset) => offset,
            None => return p1::errno::OVERFLOW,
        };
        let entry = match routes.checked_add(offset) {
            Some(entry) => entry,
            None => return p1::errno::OVERFLOW,
        };
        let status = write_wasix_route_ip4(caller, memory, entry, *route);
        if status != p1::errno::SUCCESS {
            return status;
        }
    }
    p1_write_u32(caller, memory, nroutes, needed)
}

pub(super) fn wasix_sock_status<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ret_status: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let status = wasix_sock_descriptor_unavailable(caller, fd);
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1_write_u8(caller, memory, ret_status, WASIX_SOCK_STATUS_OPENED)
}

pub(super) fn wasix_sock_addr_local<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ret_addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
            local_port,
            ..
        }))) => write_wasix_addr_port_ip4(
            caller,
            memory,
            ret_addr,
            crate::Ipv4Address::new([0, 0, 0, 0]),
            *local_port,
        ),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Bound { local_port, .. } | WasixTcpSocket::Listening { local_port, .. },
        ))) => write_wasix_addr_port_ip4(
            caller,
            memory,
            ret_addr,
            crate::Ipv4Address::new([0, 0, 0, 0]),
            *local_port,
        ),
        Some(Preview1Descriptor::Socket(_)) => {
            write_wasix_addr_port_unspec(caller, memory, ret_addr)
        }
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

pub(super) fn wasix_sock_addr_peer<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ret_addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected {
                peer_address,
                peer_port,
                ..
            },
        ))) => write_wasix_addr_port_ip4(caller, memory, ret_addr, *peer_address, *peer_port),
        Some(Preview1Descriptor::Socket(_)) => {
            write_wasix_addr_port_unspec(caller, memory, ret_addr)
        }
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

pub(super) fn wasix_validate_network_socket_request(
    af: i32,
    socktype: i32,
    proto: i32,
) -> Result<(), i32> {
    match af {
        WASIX_ADDRESS_FAMILY_UNSPEC_I32 | WASIX_ADDRESS_FAMILY_IP_INET4_I32 => {}
        WASIX_ADDRESS_FAMILY_IP_INET6_I32 | WASIX_ADDRESS_FAMILY_UNIX_I32 => {
            return Err(p1::errno::NOTSUP);
        }
        _ => return Err(p1::errno::INVAL),
    }
    match socktype {
        WASIX_SOCK_TYPE_STREAM if proto == 0 || proto == WASIX_IPPROTO_TCP_I32 => Ok(()),
        WASIX_SOCK_TYPE_DGRAM if proto == 0 || proto == WASIX_IPPROTO_UDP_I32 => Ok(()),
        WASIX_SOCK_TYPE_STREAM | WASIX_SOCK_TYPE_DGRAM => Err(p1::errno::INVAL),
        _ => Err(p1::errno::INVAL),
    }
}

pub(super) fn wasix_validate_socket_pair_request(
    af: i32,
    socktype: i32,
    proto: i32,
) -> Result<(), i32> {
    match af {
        WASIX_ADDRESS_FAMILY_UNSPEC_I32 | WASIX_ADDRESS_FAMILY_UNIX_I32 => {}
        WASIX_ADDRESS_FAMILY_IP_INET4_I32 | WASIX_ADDRESS_FAMILY_IP_INET6_I32 => {
            return Err(p1::errno::NOTSUP);
        }
        _ => return Err(p1::errno::INVAL),
    }
    match socktype {
        WASIX_SOCK_TYPE_STREAM | WASIX_SOCK_TYPE_DGRAM if proto == 0 => Ok(()),
        WASIX_SOCK_TYPE_STREAM | WASIX_SOCK_TYPE_DGRAM => Err(p1::errno::INVAL),
        _ => Err(p1::errno::INVAL),
    }
}

pub(super) fn wasix_sock_open<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    af: i32,
    socktype: i32,
    proto: i32,
    ret_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if let Err(errno) = wasix_validate_network_socket_request(af, socktype, proto) {
        return errno;
    }
    let status = match socktype {
        WASIX_SOCK_TYPE_STREAM => caller.data().require_tcp_authority(),
        WASIX_SOCK_TYPE_DGRAM => caller.data().require_udp_authority(),
        _ => return p1::errno::INVAL,
    };
    if status != p1::errno::SUCCESS {
        return status;
    }
    if caller.data().runtime_state.network_service().is_none() {
        return p1::errno::NETDOWN;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let descriptor = match socktype {
        WASIX_SOCK_TYPE_STREAM => {
            Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Unconnected {
                options: WasixSocketOptions::default(),
            }))
        }
        WASIX_SOCK_TYPE_DGRAM => {
            Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound {
                options: WasixSocketOptions::default(),
            }))
        }
        _ => return p1::errno::INVAL,
    };
    let fd = match caller.data_mut().descriptors.insert(descriptor) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, ret_fd, fd)
}

pub(super) fn wasix_sock_pair<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    af: i32,
    socktype: i32,
    proto: i32,
    ret_fd0: u32,
    ret_fd1: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if let Err(errno) = wasix_validate_socket_pair_request(af, socktype, proto) {
        return errno;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let (left_writer, right_reader) = crate::byte_channel();
    let (right_writer, left_reader) = crate::byte_channel();
    let first = Preview1Descriptor::Socket(WasixSocketDescriptor::Pair {
        reader: left_reader,
        writer: left_writer,
        carry: Bytes::new(),
        options: WasixSocketOptions::default(),
        socket_type: socktype,
    });
    let second = Preview1Descriptor::Socket(WasixSocketDescriptor::Pair {
        reader: right_reader,
        writer: right_writer,
        carry: Bytes::new(),
        options: WasixSocketOptions::default(),
        socket_type: socktype,
    });
    let fd0 = match caller.data_mut().descriptors.insert(first) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    let fd1 = match caller.data_mut().descriptors.insert(second) {
        Ok(fd) => fd,
        Err(errno) => {
            let _ = caller.data_mut().descriptors.close(fd0 as i32);
            return errno;
        }
    };
    let status = p1_write_u32(caller, memory, ret_fd0, fd0);
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1_write_u32(caller, memory, ret_fd1, fd1)
}

pub(super) fn wasix_sock_descriptor_unavailable<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(_)) => p1::errno::SUCCESS,
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

pub(super) fn wasix_sock_recv_authority(
    descriptor: Option<&Preview1Descriptor>,
) -> Result<WasixSocketAuthority, i32> {
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
            ..
        }))) => Ok(WasixSocketAuthority::Udp),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound {
            ..
        }))) => Err(p1::errno::INVAL),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { .. },
        ))) => Ok(WasixSocketAuthority::Tcp),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(_))) => Err(p1::errno::INVAL),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { .. })) => {
            Ok(WasixSocketAuthority::LocalOnly)
        }
        Some(_) => Err(p1::errno::NOTSOCK),
        None => Err(p1::errno::BADF),
    }
}

pub(super) fn wasix_sock_send_authority(
    descriptor: Option<&Preview1Descriptor>,
) -> Result<WasixSocketAuthority, i32> {
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(_))) => {
            Ok(WasixSocketAuthority::Udp)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { .. },
        ))) => Ok(WasixSocketAuthority::Tcp),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(_))) => Err(p1::errno::INVAL),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { .. })) => {
            Ok(WasixSocketAuthority::LocalOnly)
        }
        Some(_) => Err(p1::errno::NOTSOCK),
        None => Err(p1::errno::BADF),
    }
}

pub(super) fn wasix_sock_bind_authority(
    descriptor: Option<&Preview1Descriptor>,
) -> Result<WasixSocketAuthority, i32> {
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound {
            ..
        }))) => Ok(WasixSocketAuthority::Udp),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected { .. },
        ))) => Ok(WasixSocketAuthority::Tcp),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
            ..
        }))) => Err(p1::errno::INVAL),
        Some(Preview1Descriptor::Socket(_)) => Err(p1::errno::INVAL),
        Some(_) => Err(p1::errno::NOTSOCK),
        None => Err(p1::errno::BADF),
    }
}

pub(super) fn wasix_sock_listen_authority(
    descriptor: Option<&Preview1Descriptor>,
) -> Result<WasixSocketAuthority, i32> {
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(_))) => {
            Ok(WasixSocketAuthority::Tcp)
        }
        Some(Preview1Descriptor::Socket(_)) => Err(p1::errno::INVAL),
        Some(_) => Err(p1::errno::NOTSOCK),
        None => Err(p1::errno::BADF),
    }
}

pub(super) fn wasix_sock_set_opt_flag<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    option: i32,
    flag: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let flag = match flag {
        0 => false,
        1 => true,
        _ => return p1::errno::INVAL,
    };
    let Some(Preview1Descriptor::Socket(descriptor)) = caller.data_mut().descriptors.get_mut(fd)
    else {
        return match caller.data().descriptors.get(fd) {
            Some(_) => p1::errno::NOTSOCK,
            None => p1::errno::BADF,
        };
    };
    descriptor.options_mut().set_flag(option, flag)
}

pub(super) fn wasix_sock_get_opt_flag<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    option: i32,
    ret_flag: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(Preview1Descriptor::Socket(descriptor)) = caller.data().descriptors.get(fd) else {
        return match caller.data().descriptors.get(fd) {
            Some(_) => p1::errno::NOTSOCK,
            None => p1::errno::BADF,
        };
    };
    let flag = match descriptor.options().flag(option) {
        Ok(flag) => flag,
        Err(errno) => return errno,
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_wasix_bool(caller, memory, ret_flag, flag)
}

pub(super) fn wasix_sock_set_opt_time<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    option: i32,
    time: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let time = match wasix_read_optional_timestamp(caller, memory, time) {
        Ok(time) => time,
        Err(errno) => return errno,
    };
    let Some(Preview1Descriptor::Socket(descriptor)) = caller.data_mut().descriptors.get_mut(fd)
    else {
        return match caller.data().descriptors.get(fd) {
            Some(_) => p1::errno::NOTSOCK,
            None => p1::errno::BADF,
        };
    };
    descriptor.options_mut().set_time(option, time)
}

pub(super) fn wasix_sock_get_opt_time<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    option: i32,
    ret_time: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(Preview1Descriptor::Socket(descriptor)) = caller.data().descriptors.get(fd) else {
        return match caller.data().descriptors.get(fd) {
            Some(_) => p1::errno::NOTSOCK,
            None => p1::errno::BADF,
        };
    };
    let time = match descriptor.options().time(option) {
        Ok(time) => time,
        Err(errno) => return errno,
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_wasix_optional_timestamp(caller, memory, ret_time, time)
}

pub(super) fn wasix_sock_set_opt_size<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    option: i32,
    size: i64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let size = match u64::try_from(size) {
        Ok(size) => size,
        Err(_) => return p1::errno::INVAL,
    };
    let Some(Preview1Descriptor::Socket(descriptor)) = caller.data_mut().descriptors.get_mut(fd)
    else {
        return match caller.data().descriptors.get(fd) {
            Some(_) => p1::errno::NOTSOCK,
            None => p1::errno::BADF,
        };
    };
    descriptor.options_mut().set_size(option, size)
}

pub(super) fn wasix_sock_get_opt_size<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    option: i32,
    ret_size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(Preview1Descriptor::Socket(descriptor)) = caller.data().descriptors.get(fd) else {
        return match caller.data().descriptors.get(fd) {
            Some(_) => p1::errno::NOTSOCK,
            None => p1::errno::BADF,
        };
    };
    let size = match option {
        WASIX_SOCK_OPTION_TYPE => descriptor.socket_type() as u64,
        WASIX_SOCK_OPTION_PROTO => descriptor.protocol(),
        _ => match descriptor.options().size(option) {
            Ok(size) => size,
            Err(errno) => return errno,
        },
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u64(caller, memory, ret_size, size)
}

pub(super) fn wasix_sock_multicast_v6<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    multiaddr: u32,
    interface: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_multicast_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let status = caller.data().require_udp_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let status = wasix_udp_socket_descriptor_status(caller.data().descriptors.get(fd));
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    if let Err(errno) = wasix_validate_addr_ip6(caller, memory, multiaddr) {
        return errno;
    }
    if let Err(errno) = wasix_validate_addr_ip6(caller, memory, interface) {
        return errno;
    }
    p1::errno::NOTSUP
}

pub(super) async fn wasix_sock_multicast_v4<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    multiaddr: u32,
    interface: u32,
    join: bool,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_multicast_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let status = caller.data().require_udp_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let status = wasix_udp_socket_descriptor_status(caller.data().descriptors.get(fd));
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let group = match wasix_read_addr_ip4(caller, memory, multiaddr) {
        Ok(group) => group,
        Err(errno) => return errno,
    };
    let interface = match wasix_read_addr_ip4(caller, memory, interface) {
        Ok(interface) => interface,
        Err(errno) => return errno,
    };
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::NETDOWN;
    };
    let result = if join {
        service.udp_join_multicast_v4(group, interface).await
    } else {
        service.udp_leave_multicast_v4(group, interface).await
    };
    match result {
        Ok(()) => p1::errno::SUCCESS,
        Err(error) => p1_errno_from_udp_error(error),
    }
}

pub(super) fn wasix_udp_socket_descriptor_status(descriptor: Option<&Preview1Descriptor>) -> i32 {
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(_))) => p1::errno::SUCCESS,
        Some(Preview1Descriptor::Socket(_)) => p1::errno::INVAL,
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

pub(super) async fn wasix_sock_bind<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let (_, port) = match wasix_read_addr_port(caller, memory, addr) {
        Ok(addr) => addr,
        Err(errno) => return errno,
    };
    let descriptor = caller.data().descriptors.get(fd).cloned();
    let authority = match wasix_sock_bind_authority(descriptor.as_ref()) {
        Ok(authority) => authority,
        Err(errno) => return errno,
    };
    let status = caller.data().require_socket_authority(authority);
    if status != p1::errno::SUCCESS {
        return status;
    }
    if port < 1024 {
        let status = caller.data().require_privileged_bind_authority();
        if status != p1::errno::SUCCESS {
            return status;
        }
    }
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(_))) => {
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            let binding = match service.udp_bind(port).await {
                Ok(binding) => binding,
                Err(error) => return p1_errno_from_udp_error(error),
            };
            let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(slot))) =
                caller.data_mut().descriptors.get_mut(fd)
            else {
                return p1::errno::BADF;
            };
            let options = *slot.options();
            *slot = WasixUdpSocket::Bound {
                socket: binding.socket,
                local_port: binding.local_port,
                options,
            };
            p1::errno::SUCCESS
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(_))) => {
            if caller.data().runtime_state.network_service().is_none() {
                return p1::errno::NETDOWN;
            }
            let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(slot))) =
                caller.data_mut().descriptors.get_mut(fd)
            else {
                return p1::errno::BADF;
            };
            let options = *slot.options();
            *slot = WasixTcpSocket::Bound {
                local_port: port,
                options,
            };
            p1::errno::SUCCESS
        }
        _ => p1::errno::BADF,
    }
}

pub(super) async fn wasix_sock_listen<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    backlog: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if backlog < 0 {
        return p1::errno::INVAL;
    }
    let authority = match wasix_sock_listen_authority(caller.data().descriptors.get(fd)) {
        Ok(authority) => authority,
        Err(errno) => return errno,
    };
    let status = caller.data().require_socket_authority(authority);
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::NETDOWN;
    };
    let backlog = match u16::try_from(backlog) {
        Ok(backlog) => backlog,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let local_port = match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected { .. },
        ))) => 0,
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Bound {
            local_port,
            ..
        }))) => *local_port,
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(_))) => {
            return p1::errno::INVAL;
        }
        Some(_) => return p1::errno::NOTSOCK,
        None => return p1::errno::BADF,
    };
    let listener = match service
        .tcp_listen(
            crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([0, 0, 0, 0])),
            local_port,
            backlog,
        )
        .await
    {
        Ok(listener) => listener,
        Err(error) => return p1_errno_from_tcp_error(error),
    };
    let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(slot))) =
        caller.data_mut().descriptors.get_mut(fd)
    else {
        return p1::errno::BADF;
    };
    let options = *slot.options();
    *slot = WasixTcpSocket::Listening {
        listener: listener.listener,
        local_port: listener.local_port,
        options,
    };
    p1::errno::SUCCESS
}

pub(super) async fn wasix_sock_accept_v2<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ret_fd: u32,
    ret_addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let authority = match wasix_sock_listen_authority(caller.data().descriptors.get(fd)) {
        Ok(authority) => authority,
        Err(errno) => return errno,
    };
    let status = caller.data().require_socket_authority(authority);
    if status != p1::errno::SUCCESS {
        return status;
    }
    let (listener, accept_timeout) = match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Listening {
                listener, options, ..
            },
        ))) => (*listener, options.accept_timeout),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(_))) => {
            return p1::errno::INVAL;
        }
        Some(_) => return p1::errno::NOTSOCK,
        None => return p1::errno::BADF,
    };
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::NETDOWN;
    };
    let fdflags = match caller.data().descriptors.fdflags(fd) {
        Ok(fdflags) => fdflags,
        Err(errno) => return errno,
    };
    let timeout = wasix_effective_socket_timeout(accept_timeout, fdflags);
    let accepted = match service.tcp_accept(listener, timeout).await {
        Ok(accepted) => accepted,
        Err(error) => return p1_errno_from_tcp_error_for_fdflags(error, fdflags),
    };
    let peer_address = match accepted.address {
        crate::NetworkIpAddress::Ipv4(address) => address,
        crate::NetworkIpAddress::Ipv6(_) => return p1::errno::NOTSUP,
    };
    let descriptor =
        Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Connected {
            stream: accepted.stream,
            peer_address,
            peer_port: accepted.port,
            options: WasixSocketOptions::default(),
        }));
    let accepted_fd = match caller.data_mut().descriptors.insert(descriptor) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    let Some(memory) = p1_memory(caller) else {
        let _ = caller.data_mut().descriptors.close(accepted_fd as i32);
        return p1::errno::FAULT;
    };
    let status = p1_write_u32(caller, memory, ret_fd, accepted_fd);
    if status != p1::errno::SUCCESS {
        let _ = caller.data_mut().descriptors.close(accepted_fd as i32);
        return status;
    }
    if ret_addr != 0 {
        return write_wasix_addr_port_ip4(caller, memory, ret_addr, peer_address, accepted.port);
    }
    p1::errno::SUCCESS
}

pub(super) async fn wasix_sock_connect<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let started = caller
        .data()
        .runtime_state
        .profiling_enabled()
        .then(|| caller.data().cpu.now().ticks());
    let result = wasix_sock_connect_inner(caller, fd, addr).await;
    if let Some(started) = started {
        p1_record_kernel_profile(caller.data(), "wasix_sock_connect", started);
    }
    result
}

pub(super) async fn wasix_sock_connect_inner<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let (Some(address), port) = (match wasix_read_addr_port(caller, memory, addr) {
        Ok(addr) => addr,
        Err(errno) => return errno,
    }) else {
        return p1::errno::INVAL;
    };
    let status = caller.data().require_tcp_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let (local_port, connect_timeout) = match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected { options },
        ))) => (0, options.connect_timeout),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Bound {
            local_port,
            options,
        }))) => (*local_port, options.connect_timeout),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { .. },
        ))) => return p1::errno::INVAL,
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Listening { .. },
        ))) => return p1::errno::INVAL,
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(_))) => {
            return p1::errno::NOTSOCK;
        }
        Some(_) => return p1::errno::NOTSOCK,
        None => return p1::errno::BADF,
    };
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::NETDOWN;
    };
    let mut host_buffer = [0; 15];
    let host = address.write_dotted_decimal(&mut host_buffer);
    let fdflags = match caller.data().descriptors.fdflags(fd) {
        Ok(fdflags) => fdflags,
        Err(errno) => return errno,
    };
    let timeout = wasix_effective_socket_timeout(connect_timeout, fdflags);
    let stream = match service
        .tcp_connect_from(host, port, local_port, timeout)
        .await
    {
        Ok(stream) => stream,
        Err(error) => return p1_errno_from_tcp_error_for_fdflags(error, fdflags),
    };
    let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(slot))) =
        caller.data_mut().descriptors.get_mut(fd)
    else {
        return p1::errno::BADF;
    };
    let options = *slot.options();
    *slot = WasixTcpSocket::Connected {
        stream,
        peer_address: address,
        peer_port: port,
        options,
    };
    p1::errno::SUCCESS
}

pub(super) async fn wasix_sock_recv_from<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    flags: u16,
    ret_size: u32,
    ret_flags: u32,
    ret_addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let started = caller
        .data()
        .runtime_state
        .profiling_enabled()
        .then(|| caller.data().cpu.now().ticks());
    let result = wasix_sock_recv_from_inner(
        caller, fd, iovs, iovs_len, flags, ret_size, ret_flags, ret_addr,
    )
    .await;
    if let Some(started) = started {
        p1_record_kernel_profile(caller.data(), "wasix_sock_recv_from", started);
    }
    result
}

pub(super) async fn wasix_sock_recv_from_inner<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    flags: u16,
    ret_size: u32,
    ret_flags: u32,
    ret_addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let layout = match p1_read_iovs_with_byte_len(caller, memory, iovs, iovs_len) {
        Ok(layout) => layout,
        Err(errno) => return errno,
    };
    let capacity = match layout.byte_len_u32() {
        Ok(capacity) => capacity,
        Err(errno) => return errno,
    };
    let descriptor = caller.data().descriptors.get(fd).cloned();
    let authority = match wasix_sock_recv_authority(descriptor.as_ref()) {
        Ok(authority) => authority,
        Err(errno) => return errno,
    };
    let status = caller.data().require_socket_authority(authority);
    if status != p1::errno::SUCCESS {
        return status;
    }
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
            socket,
            options,
            ..
        }))) => {
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            let fdflags = match caller.data().descriptors.fdflags(fd) {
                Ok(fdflags) => fdflags,
                Err(errno) => return errno,
            };
            let timeout = wasix_effective_socket_timeout(options.receive_timeout, fdflags);
            let datagram = match service.udp_receive(socket, capacity, timeout).await {
                Ok(Some(datagram)) => datagram,
                Ok(None) => return p1::errno::AGAIN,
                Err(error) => return p1_errno_from_udp_error_for_fdflags(error, fdflags),
            };
            let status =
                p1_write_iovs_from_bytes(caller, memory, layout.iovs, &datagram.bytes, ret_size);
            if status != p1::errno::SUCCESS {
                return status;
            }
            let status = p1_write_u16(
                caller,
                memory,
                ret_flags,
                flags & WASIX_RIFLAGS_DATA_TRUNCATED,
            );
            if status != p1::errno::SUCCESS {
                return status;
            }
            let peer_address = match datagram.address {
                crate::NetworkIpAddress::Ipv4(address) => address,
                crate::NetworkIpAddress::Ipv6(_) => return p1::errno::NOTSUP,
            };
            write_wasix_addr_port_ip4(caller, memory, ret_addr, peer_address, datagram.port)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound {
            ..
        }))) => p1::errno::INVAL,
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { .. })) => {
            let bytes = match caller
                .data_mut()
                .read_socket_pair(fd, capacity as usize)
                .await
            {
                Ok(bytes) => bytes,
                Err(errno) => return errno,
            };
            let status = p1_write_iovs_from_bytes(caller, memory, layout.iovs, &bytes, ret_size);
            if status != p1::errno::SUCCESS {
                return status;
            }
            let status = p1_write_u16(caller, memory, ret_flags, 0);
            if status != p1::errno::SUCCESS {
                return status;
            }
            write_wasix_addr_port_unspec(caller, memory, ret_addr)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected {
                stream, options, ..
            },
        ))) => {
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            let fdflags = match caller.data().descriptors.fdflags(fd) {
                Ok(fdflags) => fdflags,
                Err(errno) => return errno,
            };
            let timeout = wasix_effective_socket_timeout(options.receive_timeout, fdflags);
            let ranges = match p1_iovs_memory_ranges(memory, &layout.iovs) {
                Ok(ranges) => ranges,
                Err(errno) => return errno,
            };
            let buffer = crate::RegisteredTcpReadBuffer::new(memory.base, &ranges);
            let bytes = match service
                .tcp_read_into_registered(stream, buffer, timeout)
                .await
            {
                Ok(Some(bytes)) => bytes,
                Ok(None) => 0,
                Err(error) => return p1_errno_from_tcp_error_for_fdflags(error, fdflags),
            };
            let status = p1_write_u32(
                caller,
                memory,
                ret_size,
                u32::try_from(bytes)
                    .unwrap_or_else(|_| panic!("TCP receive byte count exceeds u32")),
            );
            if status != p1::errno::SUCCESS {
                return status;
            }
            p1_write_u16(caller, memory, ret_flags, 0)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected { .. },
        ))) => p1::errno::INVAL,
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(_))) => p1::errno::INVAL,
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

pub(super) async fn wasix_sock_send_to<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    _flags: u16,
    addr: u32,
    ret_size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let started = caller
        .data()
        .runtime_state
        .profiling_enabled()
        .then(|| caller.data().cpu.now().ticks());
    let result = wasix_sock_send_to_inner(caller, fd, iovs, iovs_len, _flags, addr, ret_size).await;
    if let Some(started) = started {
        p1_record_kernel_profile(caller.data(), "wasix_sock_send_to", started);
    }
    result
}

pub(super) async fn wasix_sock_send_to_inner<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    _flags: u16,
    addr: u32,
    ret_size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let bytes = match p1_read_iovs_to_bytes(caller, memory, iovs, iovs_len) {
        Ok(bytes) => bytes,
        Err(errno) => return errno,
    };
    let descriptor = caller.data().descriptors.get(fd).cloned();
    let authority = match wasix_sock_send_authority(descriptor.as_ref()) {
        Ok(authority) => authority,
        Err(errno) => return errno,
    };
    let status = caller.data().require_socket_authority(authority);
    if status != p1::errno::SUCCESS {
        return status;
    }
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(socket))) => {
            let (Some(address), port) = (match wasix_read_addr_port(caller, memory, addr) {
                Ok(addr) => addr,
                Err(errno) => return errno,
            }) else {
                return p1::errno::INVAL;
            };
            let (socket, send_timeout) = match socket {
                WasixUdpSocket::Bound {
                    socket, options, ..
                } => (socket, options.send_timeout),
                WasixUdpSocket::Unbound { options } => {
                    let Some(service) = caller.data().runtime_state.network_service() else {
                        return p1::errno::NETDOWN;
                    };
                    let binding = match service.udp_bind(0).await {
                        Ok(binding) => binding,
                        Err(error) => return p1_errno_from_udp_error(error),
                    };
                    let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(slot))) =
                        caller.data_mut().descriptors.get_mut(fd)
                    else {
                        return p1::errno::BADF;
                    };
                    *slot = WasixUdpSocket::Bound {
                        socket: binding.socket,
                        local_port: binding.local_port,
                        options,
                    };
                    let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(
                        WasixUdpSocket::Bound {
                            socket, options, ..
                        },
                    ))) = caller.data().descriptors.get(fd)
                    else {
                        return p1::errno::BADF;
                    };
                    (*socket, options.send_timeout)
                }
            };
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            let mut host_buffer = [0; 15];
            let host = address.write_dotted_decimal(&mut host_buffer);
            let fdflags = match caller.data().descriptors.fdflags(fd) {
                Ok(fdflags) => fdflags,
                Err(errno) => return errno,
            };
            let timeout = wasix_effective_socket_timeout(send_timeout, fdflags);
            let sent = match service.udp_send(socket, host, port, &bytes, timeout).await {
                Ok(sent) => sent,
                Err(error) => return p1_errno_from_udp_error_for_fdflags(error, fdflags),
            };
            let sent = match u32::try_from(sent) {
                Ok(sent) => sent,
                Err(_) => return p1::errno::OVERFLOW,
            };
            p1_write_u32(caller, memory, ret_size, sent)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected {
                stream, options, ..
            },
        ))) => {
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            let fdflags = match caller.data().descriptors.fdflags(fd) {
                Ok(fdflags) => fdflags,
                Err(errno) => return errno,
            };
            let timeout = wasix_effective_socket_timeout(options.send_timeout, fdflags);
            let written = match u32::try_from(bytes.len()) {
                Ok(written) => written,
                Err(_) => return p1::errno::OVERFLOW,
            };
            if let Err(error) = service
                .tcp_write_all_bytes(stream, Bytes::from(bytes), timeout)
                .await
            {
                return p1_errno_from_tcp_error_for_fdflags(error, fdflags);
            }
            p1_write_u32(caller, memory, ret_size, written)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected { .. },
        ))) => p1::errno::INVAL,
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(_))) => p1::errno::INVAL,
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { writer, .. })) => {
            let written = match u32::try_from(bytes.len()) {
                Ok(written) => written,
                Err(_) => return p1::errno::OVERFLOW,
            };
            if writer.write(bytes).is_err() {
                return p1::errno::IO;
            }
            p1_write_u32(caller, memory, ret_size, written)
        }
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

pub(super) async fn wasix_sock_send_file<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    out_fd: i32,
    in_fd: i32,
    offset: i64,
    count: i64,
    ret_size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let file = match caller.data().descriptors.get(in_fd) {
        Some(Preview1Descriptor::File { descriptor, .. }) => descriptor.clone(),
        Some(_) => return p1::errno::BADF,
        None => return p1::errno::BADF,
    };
    let offset = match u64::try_from(offset) {
        Ok(offset) => offset,
        Err(_) => return p1::errno::INVAL,
    };
    let count = match u64::try_from(count) {
        Ok(count) => count,
        Err(_) => return p1::errno::INVAL,
    };
    let count = match usize::try_from(count) {
        Ok(count) => count,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let bytes = match caller
        .data()
        .filesystem
        .read_file_chunk(&file, offset, count)
        .map_err(p1_errno_from_fs)
    {
        Ok(bytes) => bytes,
        Err(errno) => return errno,
    };
    let written = match u64::try_from(bytes.len()) {
        Ok(written) => written,
        Err(_) => return p1::errno::OVERFLOW,
    };
    match caller.data().descriptors.get(out_fd).cloned() {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected {
                stream, options, ..
            },
        ))) => {
            let status = caller
                .data()
                .require_socket_authority(WasixSocketAuthority::Tcp);
            if status != p1::errno::SUCCESS {
                return status;
            }
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            let fdflags = match caller.data().descriptors.fdflags(out_fd) {
                Ok(fdflags) => fdflags,
                Err(errno) => return errno,
            };
            let timeout = wasix_effective_socket_timeout(options.send_timeout, fdflags);
            if let Err(error) = service.tcp_write_all_bytes(stream, bytes, timeout).await {
                return p1_errno_from_tcp_error_for_fdflags(error, fdflags);
            }
            p1_write_u64(caller, memory, ret_size, written)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { writer, .. })) => {
            if writer.write(bytes).is_err() {
                return p1::errno::IO;
            }
            p1_write_u64(caller, memory, ret_size, written)
        }
        Some(Preview1Descriptor::Socket(_)) => p1::errno::INVAL,
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

pub(super) fn wasix_read_addr_port<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<(Option<crate::Ipv4Address>, u16), i32> {
    let tag = p1_try_read_u8(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?;
    match tag {
        WASIX_ADDRESS_FAMILY_UNSPEC => Ok((None, 0)),
        WASIX_ADDRESS_FAMILY_IP_INET4 => {
            let port = p1_try_read_u16(caller, memory, ptr + WASIX_ADDR_PORT_UNION_OFFSET)
                .map_err(|_| p1::errno::FAULT)?;
            let mut octets = [0_u8; 4];
            p1_read_memory_into(
                caller,
                memory,
                ptr + WASIX_ADDR_PORT_IP4_ADDRESS_OFFSET,
                &mut octets,
            )
            .map_err(|_| p1::errno::FAULT)?;
            Ok((Some(crate::Ipv4Address::new(octets)), port))
        }
        WASIX_ADDRESS_FAMILY_IP_INET6 => Err(p1::errno::NOTSUP),
        WASIX_ADDRESS_FAMILY_UNIX => Err(p1::errno::NOTSUP),
        _ => Err(p1::errno::INVAL),
    }
}

pub(super) fn wasix_read_addr_ip4<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<crate::Ipv4Address, i32> {
    let tag = p1_try_read_u8(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?;
    match tag {
        WASIX_ADDRESS_FAMILY_IP_INET4 => {
            let mut octets = [0_u8; 4];
            p1_read_memory_into(
                caller,
                memory,
                ptr + WASIX_ADDR_IP_UNION_OFFSET,
                &mut octets,
            )
            .map_err(|_| p1::errno::FAULT)?;
            Ok(crate::Ipv4Address::new(octets))
        }
        WASIX_ADDRESS_FAMILY_IP_INET6 | WASIX_ADDRESS_FAMILY_UNIX => Err(p1::errno::NOTSUP),
        WASIX_ADDRESS_FAMILY_UNSPEC => Err(p1::errno::INVAL),
        _ => Err(p1::errno::INVAL),
    }
}

pub(super) fn wasix_validate_addr_ip6<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<(), i32> {
    let tag = p1_try_read_u8(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?;
    wasix_addr_ip6_family_status(tag)?;
    let mut octets = [0_u8; 16];
    p1_read_memory_into(
        caller,
        memory,
        ptr + WASIX_ADDR_IP_UNION_OFFSET,
        &mut octets,
    )
    .map_err(|_| p1::errno::FAULT)
}

pub(super) fn wasix_addr_ip6_family_status(tag: u8) -> Result<(), i32> {
    match tag {
        WASIX_ADDRESS_FAMILY_IP_INET6 => Ok(()),
        WASIX_ADDRESS_FAMILY_UNIX => Err(p1::errno::NOTSUP),
        WASIX_ADDRESS_FAMILY_UNSPEC | WASIX_ADDRESS_FAMILY_IP_INET4 => Err(p1::errno::INVAL),
        _ => Err(p1::errno::INVAL),
    }
}

pub(super) fn wasix_read_addr_cidr_ip4<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<crate::Ipv4Cidr, i32> {
    let tag = p1_try_read_u8(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?;
    match tag {
        WASIX_ADDRESS_FAMILY_IP_INET4 => {
            let mut octets = [0_u8; 4];
            p1_read_memory_into(
                caller,
                memory,
                ptr + WASIX_ADDR_CIDR_IP4_ADDRESS_OFFSET,
                &mut octets,
            )
            .map_err(|_| p1::errno::FAULT)?;
            let prefix = p1_try_read_u8(caller, memory, ptr + WASIX_ADDR_CIDR_IP4_PREFIX_OFFSET)
                .map_err(|_| p1::errno::FAULT)?;
            if prefix > 32 {
                return Err(p1::errno::INVAL);
            }
            Ok(crate::Ipv4Cidr::new(
                crate::Ipv4Address::new(octets),
                prefix,
            ))
        }
        WASIX_ADDRESS_FAMILY_IP_INET6 | WASIX_ADDRESS_FAMILY_UNIX => Err(p1::errno::NOTSUP),
        WASIX_ADDRESS_FAMILY_UNSPEC => Err(p1::errno::INVAL),
        _ => Err(p1::errno::INVAL),
    }
}

pub(super) fn wasix_read_option_pid<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<Option<u32>, i32> {
    let tag = p1_try_read_u8(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?;
    match tag {
        WASIX_OPTION_NONE => Ok(None),
        WASIX_OPTION_SOME => p1_try_read_u32(caller, memory, ptr + WASIX_OPTION_UNION_U32_OFFSET)
            .map(Some)
            .map_err(|_| p1::errno::FAULT),
        _ => Err(p1::errno::INVAL),
    }
}

pub(super) fn wasix_read_optional_timestamp<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<Option<u64>, i32> {
    if ptr == 0 {
        return Ok(None);
    }
    let tag = p1_try_read_u8(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?;
    match tag {
        0 => Ok(None),
        1 => {
            let value = ptr.checked_add(8).ok_or(p1::errno::OVERFLOW)?;
            p1_try_read_u64(caller, memory, value)
                .map(Some)
                .map_err(|_| p1::errno::FAULT)
        }
        _ => Err(p1::errno::INVAL),
    }
}

pub(super) fn p1_write_wasix_optional_timestamp<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    value: Option<u64>,
) -> i32 {
    match value {
        Some(value) => p1_write_u8(caller, memory, ptr, WASIX_OPTION_SOME).max(p1_write_u64(
            caller,
            memory,
            ptr + WASIX_OPTION_UNION_U64_OFFSET,
            value,
        )),
        None => p1_write_u8(caller, memory, ptr, WASIX_OPTION_NONE).max(p1_write_u64(
            caller,
            memory,
            ptr + WASIX_OPTION_UNION_U64_OFFSET,
            0,
        )),
    }
}

pub(super) fn wasix_socket_flag_bit(option: i32) -> Result<u32, i32> {
    match option {
        WASIX_SOCK_OPTION_REUSE_PORT
        | WASIX_SOCK_OPTION_REUSE_ADDR
        | WASIX_SOCK_OPTION_NO_DELAY
        | WASIX_SOCK_OPTION_DONT_ROUTE
        | WASIX_SOCK_OPTION_ONLY_V6
        | WASIX_SOCK_OPTION_BROADCAST
        | WASIX_SOCK_OPTION_MULTICAST_LOOP_V4
        | WASIX_SOCK_OPTION_MULTICAST_LOOP_V6
        | WASIX_SOCK_OPTION_PROMISCUOUS
        | WASIX_SOCK_OPTION_LISTENING
        | WASIX_SOCK_OPTION_KEEP_ALIVE
        | WASIX_SOCK_OPTION_OOB_INLINE => Ok(1_u32 << option),
        _ => Err(p1::errno::INVAL),
    }
}

pub(super) fn p1_write_wasix_bool<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    value: bool,
) -> i32 {
    p1_write_u8(caller, memory, ptr, u8::from(value))
}

pub(super) fn p1_read_wasix_bool<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<bool, i32> {
    match p1_try_read_u8(caller, memory, ptr).map_err(|_| p1::errno::FAULT)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(p1::errno::INVAL),
    }
}

pub(super) fn write_wasix_addr_ip4<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    address: crate::Ipv4Address,
) -> i32 {
    let octets = address.octets();
    p1_write_u8(caller, memory, ptr, WASIX_ADDRESS_FAMILY_IP_INET4).max(p1_write_memory(
        caller,
        memory,
        ptr + WASIX_ADDR_IP_UNION_OFFSET,
        &octets,
    ))
}

pub(super) fn write_wasix_addr_cidr_ip4<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    cidr: crate::Ipv4Cidr,
) -> i32 {
    let mut bytes = [0_u8; WASIX_ADDR_CIDR_SIZE as usize];
    bytes[0] = WASIX_ADDRESS_FAMILY_IP_INET4;
    bytes[WASIX_ADDR_CIDR_IP4_ADDRESS_OFFSET as usize
        ..WASIX_ADDR_CIDR_IP4_ADDRESS_OFFSET as usize + 4]
        .copy_from_slice(&cidr.address().octets());
    bytes[WASIX_ADDR_CIDR_IP4_PREFIX_OFFSET as usize] = cidr.prefix_len();
    p1_write_memory(caller, memory, ptr, &bytes)
}

pub(super) fn write_wasix_route_ip4<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    route: crate::Ipv4Route,
) -> i32 {
    let status = write_wasix_addr_cidr_ip4(
        caller,
        memory,
        ptr + WASIX_ROUTE_CIDR_OFFSET,
        route.destination(),
    );
    if status != p1::errno::SUCCESS {
        return status;
    }
    let status = write_wasix_addr_ip4(
        caller,
        memory,
        ptr + WASIX_ROUTE_ROUTER_OFFSET,
        route.gateway(),
    );
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1_write_wasix_optional_timestamp(
        caller,
        memory,
        ptr + WASIX_ROUTE_PREFERRED_UNTIL_OFFSET,
        route.preferred_until_nanos(),
    )
    .max(p1_write_wasix_optional_timestamp(
        caller,
        memory,
        ptr + WASIX_ROUTE_EXPIRES_AT_OFFSET,
        route.expires_at_nanos(),
    ))
}

pub(super) fn write_wasix_addr_port_ip4<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    address: crate::Ipv4Address,
    port: u16,
) -> i32 {
    let octets = address.octets();
    p1_write_u8(caller, memory, ptr, WASIX_ADDRESS_FAMILY_IP_INET4)
        .max(p1_write_u16(
            caller,
            memory,
            ptr + WASIX_ADDR_PORT_UNION_OFFSET,
            port,
        ))
        .max(p1_write_memory(
            caller,
            memory,
            ptr + WASIX_ADDR_PORT_IP4_ADDRESS_OFFSET,
            &octets,
        ))
}

pub(super) fn write_wasix_addr_port_unspec<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> i32 {
    p1_write_u8(caller, memory, ptr, WASIX_ADDRESS_FAMILY_UNSPEC)
        .max(p1_write_u16(
            caller,
            memory,
            ptr + WASIX_ADDR_PORT_UNION_OFFSET,
            0,
        ))
        .max(p1_write_memory(
            caller,
            memory,
            ptr + WASIX_ADDR_PORT_IP4_ADDRESS_OFFSET,
            &[0, 0, 0, 0],
        ))
}

pub(super) fn wasix_effective_socket_timeout(option_timeout: Option<u64>, fdflags: u16) -> u64 {
    if p1_fdflags_nonblocking(fdflags) {
        0
    } else {
        option_timeout.unwrap_or(u64::MAX)
    }
}
