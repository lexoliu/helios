use super::*;

pub(super) fn add_wasix_port_imports<CpuImpl, Net, StorageImpl>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_bridge",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (network, network_len, token, token_len, security): (i32, i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_reply(
                        wasix_port_bridge(
                            &mut caller,
                            network as u32,
                            network_len as u32,
                            token as u32,
                            token_len as u32,
                            security,
                        )
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_unbridge",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>, ()| {
                Box::new(async move { wasix_port_unbridge(&mut caller).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_dhcp_acquire",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>, ()| {
                Box::new(async move { wasix_port_dhcp_acquire(&mut caller).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_addr_add",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (addr,): (i32,)| {
                Box::new(
                    async move { p1_reply(wasix_port_addr_add(&mut caller, addr as u32).await) },
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_addr_remove",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (addr,): (i32,)| {
                Box::new(
                    async move { p1_reply(wasix_port_addr_remove(&mut caller, addr as u32).await) },
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_addr_clear",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>, ()| {
                Box::new(async move { wasix_port_addr_clear(&mut caller).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_mac",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (ret_mac,): (i32,)| {
                Box::new(async move { p1_reply(wasix_port_mac(&mut caller, ret_mac as u32).await) })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_addr_list",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (addrs, naddrs): (i32, i32)| {
                Box::new(async move {
                    p1_reply(wasix_port_addr_list(&mut caller, addrs as u32, naddrs as u32).await)
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_gateway_set",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (addr,): (i32,)| {
                Box::new(
                    async move { p1_reply(wasix_port_gateway_set(&mut caller, addr as u32).await) },
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_route_add",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (cidr, router, preferred, expires): (i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_reply(
                        wasix_port_route_add(
                            &mut caller,
                            cidr as u32,
                            router as u32,
                            preferred as u32,
                            expires as u32,
                        )
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_route_remove",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (cidr,): (i32,)| {
                Box::new(async move {
                    p1_reply(wasix_port_route_remove(&mut caller, cidr as u32).await)
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_route_clear",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>, ()| {
                Box::new(async move { wasix_port_route_clear(&mut caller).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "port_route_list",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (routes, nroutes): (i32, i32)| {
                Box::new(async move {
                    p1_reply(
                        wasix_port_route_list(&mut caller, routes as u32, nroutes as u32).await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

pub(super) fn add_wasix_socket_imports<CpuImpl, Net, StorageImpl>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_status",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             fd: i32,
             ret_status: i32|
             -> i32 { wasix_sock_status(&mut caller, fd, ret_status as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_addr_local",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             fd: i32,
             ret_addr: i32|
             -> i32 { wasix_sock_addr_local(&mut caller, fd, ret_addr as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_addr_peer",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             fd: i32,
             ret_addr: i32|
             -> i32 { wasix_sock_addr_peer(&mut caller, fd, ret_addr as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_open",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (fd, multiaddr, interface): (i32, i32, i32)| {
                Box::new(async move {
                    p1_reply(
                        wasix_sock_multicast_v4(
                            &mut caller,
                            fd,
                            multiaddr as u32,
                            interface as u32,
                            true,
                        )
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_leave_multicast_v4",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (fd, multiaddr, interface): (i32, i32, i32)| {
                Box::new(async move {
                    p1_reply(
                        wasix_sock_multicast_v4(
                            &mut caller,
                            fd,
                            multiaddr as u32,
                            interface as u32,
                            false,
                        )
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_join_multicast_v6",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
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
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (fd, addr): (i32, i32)| {
                Box::new(
                    async move { p1_reply(wasix_sock_bind(&mut caller, fd, addr as u32).await) },
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_listen",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (fd, backlog): (i32, i32)| {
                Box::new(async move { wasix_sock_listen(&mut caller, fd, backlog).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_accept_v2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (fd, _flags, ret_fd, ret_addr): (i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_reply(
                        wasix_sock_accept_v2(&mut caller, fd, ret_fd as u32, ret_addr as u32).await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_connect",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (fd, addr): (i32, i32)| {
                Box::new(
                    async move { p1_reply(wasix_sock_connect(&mut caller, fd, addr as u32).await) },
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_recv_from",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
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
                    p1_reply(
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
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_send_to",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (fd, iovs, iovs_len, flags, addr, ret_size): (i32, i32, i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_reply(
                        wasix_sock_send_to(
                            &mut caller,
                            fd,
                            iovs as u32,
                            iovs_len as u32,
                            flags as u16,
                            addr as u32,
                            ret_size as u32,
                        )
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_send_file",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
             (out_fd, in_fd, offset, count, ret_size): (i32, i32, i64, i64, i32)| {
                Box::new(async move {
                    p1_reply(
                        wasix_sock_send_file(
                            &mut caller,
                            out_fd,
                            in_fd,
                            offset,
                            count,
                            ret_size as u32,
                        )
                        .await,
                    )
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

pub(super) async fn wasix_resolve<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    host: u32,
    host_len: u32,
    port: i32,
    addrs: u32,
    naddrs: u32,
    ret_naddrs: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let status = caller.data().require_dns_authority();
    if status != p1::errno::SUCCESS {
        return Ok(status);
    }
    if u16::try_from(port).is_err() {
        return Ok(p1::errno::INVAL);
    }
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let host = memory
        .read_path(GuestAddress::wasm32(host), host_len)
        .await?;
    let Some(service) = caller.data().runtime_state.network_service() else {
        return Ok(p1::errno::NETDOWN);
    };
    let resolved = match service.dns_resolve(&host, u64::MAX).await {
        Ok(resolved) => resolved,
        Err(error) => return Ok(p1_errno_from_dns_error(error)),
    };
    let returned = resolved.len().min(naddrs as usize);
    let addrs = GuestAddress::wasm32(addrs);
    for (index, address) in resolved.iter().take(returned).enumerate() {
        let entry = addrs.offset(index as u64 * u64::from(WASIX_ADDR_IP_SIZE));
        write_wasix_addr_ip(&memory, entry, *address).await?;
    }
    let returned = u32::try_from(returned).map_err(|_| P1Stop::Errno(p1::errno::OVERFLOW))?;
    memory
        .write_u32(GuestAddress::wasm32(ret_naddrs), returned)
        .await?;
    Ok(p1::errno::SUCCESS)
}

pub(super) fn wasix_network_admin_service<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
) -> Result<(crate::NetworkAdminCap, Net), i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

pub(super) async fn wasix_port_bridge<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    network: u32,
    network_len: u32,
    token: u32,
    token_len: u32,
    security: i32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let security = match wasix_bridge_security(security) {
        Ok(security) => security,
        Err(status) => return Ok(status),
    };
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let network = wasix_read_exec_string(&memory, network, network_len).await?;
    let token = wasix_read_exec_string(&memory, token, token_len).await?;
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return Ok(status),
    };
    let request = crate::NetworkBridgeRequest::new(network, token, security);
    let control = crate::NetworkControl::new(service);
    Ok(
        match control
            .bridge_port(cap, crate::NetworkPortId::new(0), request)
            .await
        {
            Ok(()) => p1::errno::SUCCESS,
            Err(error) => p1_errno_from_network_control_error(error),
        },
    )
}

pub(super) async fn wasix_port_unbridge<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

pub(super) async fn wasix_port_dhcp_acquire<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

pub(super) async fn wasix_port_addr_add<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    addr: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let cidr = wasix_read_addr_cidr_ip4(&memory, GuestAddress::wasm32(addr)).await?;
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return Ok(status),
    };
    let control = crate::NetworkControl::new(service);
    Ok(
        match control
            .add_address(cap, crate::NetworkPortId::new(0), cidr)
            .await
        {
            Ok(()) => p1::errno::SUCCESS,
            Err(error) => p1_errno_from_network_control_error(error),
        },
    )
}

pub(super) async fn wasix_port_addr_remove<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    addr: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let address = wasix_read_addr_ip4(&memory, GuestAddress::wasm32(addr)).await?;
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return Ok(status),
    };
    let control = crate::NetworkControl::new(service);
    let cidr = crate::Ipv4Cidr::new(address, 32);
    Ok(
        match control
            .remove_address(cap, crate::NetworkPortId::new(0), cidr)
            .await
        {
            Ok(()) => p1::errno::SUCCESS,
            Err(error) => p1_errno_from_network_control_error(error),
        },
    )
}

pub(super) async fn wasix_port_addr_clear<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

pub(super) async fn wasix_port_mac<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    ret_mac: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return Ok(status),
    };
    let control = crate::NetworkControl::new(service);
    let mac = match control.mac_address(cap, crate::NetworkPortId::new(0)).await {
        Ok(mac) => mac,
        Err(error) => return Ok(p1_errno_from_network_control_error(error)),
    };
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    memory
        .write_bytes(GuestAddress::wasm32(ret_mac), &mac.octets())
        .await?;
    Ok(p1::errno::SUCCESS)
}

pub(super) async fn wasix_port_addr_list<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    addrs: u32,
    naddrs: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let (addrs, naddrs) = (GuestAddress::wasm32(addrs), GuestAddress::wasm32(naddrs));
    let capacity = memory.read_u32(naddrs).await?;
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return Ok(status),
    };
    let control = crate::NetworkControl::new(service);
    let addresses = match control
        .list_addresses(cap, crate::NetworkPortId::new(0))
        .await
    {
        Ok(addresses) => addresses,
        Err(error) => return Ok(p1_errno_from_network_control_error(error)),
    };
    let needed = match u32::try_from(addresses.len()) {
        Ok(needed) => needed,
        Err(_) => return Ok(p1::errno::OVERFLOW),
    };
    if needed > capacity {
        memory.write_u32(naddrs, needed).await?;
        return Ok(p1::errno::OVERFLOW);
    };
    for (index, cidr) in addresses.iter().enumerate() {
        let entry = addrs.offset(index as u64 * u64::from(WASIX_ADDR_CIDR_SIZE));
        write_wasix_addr_cidr_ip4(&memory, entry, *cidr).await?;
    }
    memory.write_u32(naddrs, needed).await?;
    Ok(p1::errno::SUCCESS)
}

pub(super) async fn wasix_port_gateway_set<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    addr: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let gateway = wasix_read_addr_ip4(&memory, GuestAddress::wasm32(addr)).await?;
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return Ok(status),
    };
    let control = crate::NetworkControl::new(service);
    Ok(
        match control
            .set_gateway(cap, crate::NetworkPortId::new(0), gateway)
            .await
        {
            Ok(()) => p1::errno::SUCCESS,
            Err(error) => p1_errno_from_network_control_error(error),
        },
    )
}

pub(super) async fn wasix_port_route_add<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    cidr: u32,
    router: u32,
    preferred: u32,
    expires: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let destination = wasix_read_addr_cidr_ip4(&memory, GuestAddress::wasm32(cidr)).await?;
    let gateway = wasix_read_addr_ip4(&memory, GuestAddress::wasm32(router)).await?;
    let preferred =
        wasix_read_optional_timestamp_async(&memory, GuestAddress::wasm32(preferred)).await?;
    let expires =
        wasix_read_optional_timestamp_async(&memory, GuestAddress::wasm32(expires)).await?;
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return Ok(status),
    };
    let control = crate::NetworkControl::new(service);
    let route = crate::Ipv4Route::with_lifetimes(destination, gateway, preferred, expires);
    Ok(
        match control
            .add_route(cap, crate::NetworkPortId::new(0), route)
            .await
        {
            Ok(()) => p1::errno::SUCCESS,
            Err(error) => p1_errno_from_network_control_error(error),
        },
    )
}

pub(super) async fn wasix_port_route_remove<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    cidr: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let destination = wasix_read_addr_ip4(&memory, GuestAddress::wasm32(cidr)).await?;
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return Ok(status),
    };
    let control = crate::NetworkControl::new(service);
    let routes = match control.list_routes(cap, crate::NetworkPortId::new(0)).await {
        Ok(routes) => routes,
        Err(error) => return Ok(p1_errno_from_network_control_error(error)),
    };
    for route in routes {
        if route.destination().address() == destination {
            match control
                .remove_route(cap, crate::NetworkPortId::new(0), route)
                .await
            {
                Ok(()) => return Ok(p1::errno::SUCCESS),
                Err(error) => return Ok(p1_errno_from_network_control_error(error)),
            }
        }
    }
    Ok(p1::errno::NOENT)
}

pub(super) async fn wasix_port_route_clear<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

pub(super) async fn wasix_port_route_list<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    routes: u32,
    nroutes: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let (routes, nroutes) = (GuestAddress::wasm32(routes), GuestAddress::wasm32(nroutes));
    let capacity = memory.read_u32(nroutes).await?;
    let (cap, service) = match wasix_network_admin_service(caller) {
        Ok(parts) => parts,
        Err(status) => return Ok(status),
    };
    let control = crate::NetworkControl::new(service);
    let route_entries = match control.list_routes(cap, crate::NetworkPortId::new(0)).await {
        Ok(route_entries) => route_entries,
        Err(error) => return Ok(p1_errno_from_network_control_error(error)),
    };
    let needed = match u32::try_from(route_entries.len()) {
        Ok(needed) => needed,
        Err(_) => return Ok(p1::errno::OVERFLOW),
    };
    if needed > capacity {
        memory.write_u32(nroutes, needed).await?;
        return Ok(p1::errno::OVERFLOW);
    }
    for (index, route) in route_entries.iter().enumerate() {
        let entry = routes.offset(index as u64 * u64::from(WASIX_ROUTE_SIZE));
        write_wasix_route_ip4(&memory, entry, *route).await?;
    }
    memory.write_u32(nroutes, needed).await?;
    Ok(p1::errno::SUCCESS)
}

pub(super) fn wasix_sock_status<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    ret_status: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let status = wasix_sock_descriptor_unavailable(caller, fd);
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1_write_u8(
        caller,
        memory,
        GuestAddress::wasm32(ret_status),
        WASIX_SOCK_STATUS_OPENED,
    )
}

pub(super) fn wasix_sock_addr_local<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    ret_addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    // A bind never names a local address in WASIX, only a port, so the
    // local address is the socket family's wildcard: `0.0.0.0` or `::`.
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
            local_port,
            family,
            ..
        }))) => {
            let (address, local_port) = (family.unspecified_address(), *local_port);
            write_wasix_addr_port(
                caller,
                memory,
                GuestAddress::wasm32(ret_addr),
                address,
                local_port,
            )
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Bound {
                local_port, family, ..
            }
            | WasixTcpSocket::Listening {
                local_port, family, ..
            },
        ))) => {
            let (address, local_port) = (family.unspecified_address(), *local_port);
            write_wasix_addr_port(
                caller,
                memory,
                GuestAddress::wasm32(ret_addr),
                address,
                local_port,
            )
        }
        Some(Preview1Descriptor::Socket(_)) => {
            write_wasix_addr_port_unspec(caller, memory, GuestAddress::wasm32(ret_addr))
        }
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

pub(super) fn wasix_sock_addr_peer<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    ret_addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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
        ))) => write_wasix_addr_port(
            caller,
            memory,
            GuestAddress::wasm32(ret_addr),
            *peer_address,
            *peer_port,
        ),
        Some(Preview1Descriptor::Socket(_)) => {
            write_wasix_addr_port_unspec(caller, memory, GuestAddress::wasm32(ret_addr))
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
        WASIX_ADDRESS_FAMILY_UNSPEC_I32
        | WASIX_ADDRESS_FAMILY_IP_INET4_I32
        | WASIX_ADDRESS_FAMILY_IP_INET6_I32 => {}
        WASIX_ADDRESS_FAMILY_UNIX_I32 => {
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

pub(super) fn wasix_sock_open<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    af: i32,
    socktype: i32,
    proto: i32,
    ret_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    if let Err(errno) = wasix_validate_network_socket_request(af, socktype, proto) {
        return errno;
    }
    // AF_UNSPEC keeps the historical IPv4 default, matching what a
    // `socket(0, SOCK_STREAM, 0)` caller gets from WASIX today.
    let family = match af {
        WASIX_ADDRESS_FAMILY_IP_INET6_I32 => WasixSocketFamily::Ipv6,
        _ => WasixSocketFamily::Ipv4,
    };
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
                family,
                options: WasixSocketOptions::default(),
            }))
        }
        WASIX_SOCK_TYPE_DGRAM => {
            Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound {
                family,
                options: WasixSocketOptions::default(),
            }))
        }
        _ => return p1::errno::INVAL,
    };
    let fd = match caller.data_mut().descriptors.insert(descriptor) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, GuestAddress::wasm32(ret_fd), fd)
}

pub(super) fn wasix_sock_pair<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    af: i32,
    socktype: i32,
    proto: i32,
    ret_fd0: u32,
    ret_fd1: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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
    let status = p1_write_u32(caller, memory, GuestAddress::wasm32(ret_fd0), fd0);
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1_write_u32(caller, memory, GuestAddress::wasm32(ret_fd1), fd1)
}

pub(super) fn wasix_sock_descriptor_unavailable<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(_)) => p1::errno::SUCCESS,
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

pub(super) fn wasix_sock_recv_authority<Net: ComponentHostNetwork>(
    descriptor: Option<&Preview1Descriptor<Net>>,
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

pub(super) fn wasix_sock_send_authority<Net: ComponentHostNetwork>(
    descriptor: Option<&Preview1Descriptor<Net>>,
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

pub(super) fn wasix_sock_bind_authority<Net: ComponentHostNetwork>(
    descriptor: Option<&Preview1Descriptor<Net>>,
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

pub(super) fn wasix_sock_listen_authority<Net: ComponentHostNetwork>(
    descriptor: Option<&Preview1Descriptor<Net>>,
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

pub(super) fn wasix_sock_set_opt_flag<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    option: i32,
    flag: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

pub(super) fn wasix_sock_get_opt_flag<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    option: i32,
    ret_flag: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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
    p1_write_wasix_bool(caller, memory, GuestAddress::wasm32(ret_flag), flag)
}

pub(super) fn wasix_sock_set_opt_time<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    option: i32,
    time: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let time = match wasix_read_optional_timestamp(caller, memory, GuestAddress::wasm32(time)) {
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

pub(super) fn wasix_sock_get_opt_time<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    option: i32,
    ret_time: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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
    p1_write_wasix_optional_timestamp(caller, memory, GuestAddress::wasm32(ret_time), time)
}

pub(super) fn wasix_sock_set_opt_size<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    option: i32,
    size: i64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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
    let status = descriptor.options_mut().set_size(option, size);
    if status != p1::errno::SUCCESS || option != WASIX_SOCK_OPTION_TTL {
        return status;
    }
    // A socket that already exists in the netstack takes the new TTL now;
    // one that has not connected, listened, or bound yet gets it applied at
    // that point, so no packet ever leaves with a stale value.
    let hop_limit = descriptor.options().hop_limit();
    let target = wasix_socket_backend_handle(descriptor);
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::SUCCESS;
    };
    match target {
        Some(WasixSocketBackendHandle::TcpStream(stream)) => service
            .tcp_set_hop_limit(stream, hop_limit)
            .map_or_else(p1_errno_from_tcp_error, |()| p1::errno::SUCCESS),
        Some(WasixSocketBackendHandle::TcpListener(listener)) => service
            .tcp_listener_set_hop_limit(listener, hop_limit)
            .map_or_else(p1_errno_from_tcp_error, |()| p1::errno::SUCCESS),
        Some(WasixSocketBackendHandle::UdpSocket(socket)) => service
            .udp_set_hop_limit(socket, hop_limit)
            .map_or_else(p1_errno_from_udp_error, |()| p1::errno::SUCCESS),
        None => p1::errno::SUCCESS,
    }
}

/// The netstack handle a WASIX socket descriptor currently owns, if any.
enum WasixSocketBackendHandle<Net: ComponentHostNetwork> {
    TcpStream(Net::TcpStream),
    TcpListener(Net::TcpListener),
    UdpSocket(Net::UdpSocket),
}

fn wasix_socket_backend_handle<Net: ComponentHostNetwork>(
    descriptor: &WasixSocketDescriptor<Net>,
) -> Option<WasixSocketBackendHandle<Net>> {
    match descriptor {
        WasixSocketDescriptor::Tcp(WasixTcpSocket::Connected { stream, .. }) => {
            Some(WasixSocketBackendHandle::TcpStream(stream.id()))
        }
        WasixSocketDescriptor::Tcp(WasixTcpSocket::Listening { listener, .. }) => {
            Some(WasixSocketBackendHandle::TcpListener(listener.id()))
        }
        WasixSocketDescriptor::Udp(WasixUdpSocket::Bound { socket, .. }) => {
            Some(WasixSocketBackendHandle::UdpSocket(socket.id()))
        }
        WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected { .. } | WasixTcpSocket::Bound { .. },
        )
        | WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound { .. })
        | WasixSocketDescriptor::Pair { .. } => None,
    }
}

pub(super) fn wasix_sock_get_opt_size<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    option: i32,
    ret_size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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
    p1_write_u64(caller, memory, GuestAddress::wasm32(ret_size), size)
}

pub(super) fn wasix_sock_multicast_v6<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    multiaddr: u32,
    interface: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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
    if let Err(errno) = wasix_validate_addr_ip6(caller, memory, GuestAddress::wasm32(multiaddr)) {
        return errno;
    }
    if let Err(errno) = wasix_validate_addr_ip6(caller, memory, GuestAddress::wasm32(interface)) {
        return errno;
    }
    p1::errno::NOTSUP
}

pub(super) async fn wasix_sock_multicast_v4<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    multiaddr: u32,
    interface: u32,
    join: bool,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let status = caller.data().require_multicast_authority();
    if status != p1::errno::SUCCESS {
        return Ok(status);
    }
    let status = caller.data().require_udp_authority();
    if status != p1::errno::SUCCESS {
        return Ok(status);
    }
    let status = wasix_udp_socket_descriptor_status(caller.data().descriptors.get(fd));
    if status != p1::errno::SUCCESS {
        return Ok(status);
    }
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let group = wasix_read_addr_ip4(&memory, GuestAddress::wasm32(multiaddr)).await?;
    let interface = wasix_read_addr_ip4(&memory, GuestAddress::wasm32(interface)).await?;
    let Some(service) = caller.data().runtime_state.network_service() else {
        return Ok(p1::errno::NETDOWN);
    };
    let result = if join {
        service.udp_join_multicast_v4(group, interface).await
    } else {
        service.udp_leave_multicast_v4(group, interface).await
    };
    Ok(match result {
        Ok(()) => p1::errno::SUCCESS,
        Err(error) => p1_errno_from_udp_error(error),
    })
}

pub(super) fn wasix_udp_socket_descriptor_status<Net: ComponentHostNetwork>(
    descriptor: Option<&Preview1Descriptor<Net>>,
) -> i32 {
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(_))) => p1::errno::SUCCESS,
        Some(Preview1Descriptor::Socket(_)) => p1::errno::INVAL,
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

pub(super) async fn wasix_sock_bind<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    addr: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let (_, port) = wasix_read_addr_port(&memory, GuestAddress::wasm32(addr)).await?;
    Ok(wasix_sock_bind_port(caller, fd, port).await)
}

async fn wasix_sock_bind_port<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    port: u16,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
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
            let family = slot.family();
            // Nothing can leave a datagram socket before its bind completes,
            // so applying the descriptor's TTL here covers every packet.
            if let Err(error) = service.udp_set_hop_limit(binding.socket, options.hop_limit()) {
                return p1_errno_from_udp_error(error);
            }
            *slot = WasixUdpSocket::Bound {
                family,
                socket: WasixOwnedUdpSocket::new(service.clone(), binding.socket),
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
                family: slot.family(),
                local_port: port,
                options,
            };
            p1::errno::SUCCESS
        }
        _ => p1::errno::BADF,
    }
}

pub(super) async fn wasix_sock_listen<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    backlog: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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
    let (local_port, hop_limit, family) = match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected { options, family },
        ))) => (0, options.hop_limit(), *family),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Bound {
            local_port,
            options,
            family,
        }))) => (*local_port, options.hop_limit(), *family),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(_))) => {
            return p1::errno::INVAL;
        }
        Some(_) => return p1::errno::NOTSOCK,
        None => return p1::errno::BADF,
    };
    // A wildcard listen means `::` on an AF_INET6 socket, not `0.0.0.0`.
    let listener = match service
        .tcp_listen(family.unspecified_address(), local_port, backlog, hop_limit)
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
        family,
        listener: WasixOwnedTcpListener::new(service.clone(), listener.listener),
        local_port: listener.local_port,
        options,
    };
    p1::errno::SUCCESS
}

pub(super) async fn wasix_sock_accept_v2<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    ret_fd: u32,
    ret_addr: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let (accepted_fd, peer_address, peer_port) = match wasix_sock_accepted(caller, fd).await {
        Ok(accepted) => accepted,
        Err(errno) => return Ok(errno),
    };
    // The accept awaited, so the guest's pages are checked again as they
    // are written. A descriptor the guest never learned of is closed again.
    let written = async {
        let Some(memory) = p1_async_memory(caller) else {
            return Err(P1Stop::Errno(p1::errno::FAULT));
        };
        memory
            .write_u32(GuestAddress::wasm32(ret_fd), accepted_fd)
            .await?;
        if ret_addr != 0 {
            write_wasix_addr_port_async(
                &memory,
                GuestAddress::wasm32(ret_addr),
                Some(peer_address),
                peer_port,
            )
            .await?;
        }
        Ok(p1::errno::SUCCESS)
    }
    .await;
    if written.is_err() {
        let _ = caller.data_mut().descriptors.close(accepted_fd as i32);
    }
    written
}

/// Accepts a connection on the listening socket `fd` and installs it as a
/// descriptor: its number, and the address and port of its peer.
async fn wasix_sock_accepted<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
) -> Result<(u32, crate::NetworkIpAddress, u16), i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let authority = wasix_sock_listen_authority(caller.data().descriptors.get(fd))?;
    let status = caller.data().require_socket_authority(authority);
    if status != p1::errno::SUCCESS {
        return Err(status);
    }
    let (listener, accept_timeout, family) = match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Listening {
                listener,
                options,
                family,
                ..
            },
        ))) => (listener.id(), options.accept_timeout, *family),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(_))) => {
            return Err(p1::errno::INVAL);
        }
        Some(_) => return Err(p1::errno::NOTSOCK),
        None => return Err(p1::errno::BADF),
    };
    let Some(service) = caller.data().runtime_state.network_service() else {
        return Err(p1::errno::NETDOWN);
    };
    let fdflags = caller.data().descriptors.fdflags(fd)?;
    let timeout = wasix_effective_socket_timeout(accept_timeout, fdflags);
    let accepted = service
        .tcp_accept(listener, timeout)
        .await
        .map_err(|error| p1_errno_from_tcp_error_for_fdflags(error, fdflags))?;
    let peer_address = accepted.address;
    let descriptor =
        Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Connected {
            family,
            stream: WasixOwnedTcpStream::new(service.clone(), accepted.stream),
            peer_address,
            peer_port: accepted.port,
            options: WasixSocketOptions::default(),
        }));
    let accepted_fd = caller.data_mut().descriptors.insert(descriptor)?;
    Ok((accepted_fd, peer_address, accepted.port))
}

pub(super) async fn wasix_sock_connect<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    addr: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

pub(super) async fn wasix_sock_connect_inner<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    addr: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let (Some(address), port) = wasix_read_addr_port(&memory, GuestAddress::wasm32(addr)).await?
    else {
        return Ok(p1::errno::INVAL);
    };
    Ok(wasix_sock_connect_to(caller, fd, address, port).await)
}

async fn wasix_sock_connect_to<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    address: crate::NetworkIpAddress,
    port: u16,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let status = caller.data().require_tcp_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let (local_port, connect_timeout, hop_limit, family) = match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected { options, family },
        ))) => (0, options.connect_timeout, options.hop_limit(), *family),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Bound {
            local_port,
            options,
            family,
        }))) => (
            *local_port,
            options.connect_timeout,
            options.hop_limit(),
            *family,
        ),
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
    if !family.accepts(address) {
        return p1::errno::AFNOSUPPORT;
    }
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::NETDOWN;
    };
    let fdflags = match caller.data().descriptors.fdflags(fd) {
        Ok(fdflags) => fdflags,
        Err(errno) => return errno,
    };
    let timeout = wasix_effective_socket_timeout(connect_timeout, fdflags);
    // The typed address entry point keeps the family intact; formatting
    // it back into a host string would lose IPv6 outright.
    let stream = match service
        .tcp_connect_address(address, port, local_port, hop_limit, timeout)
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
        family,
        stream: WasixOwnedTcpStream::new(service.clone(), stream),
        peer_address: address,
        peer_port: port,
        options,
    };
    p1::errno::SUCCESS
}

#[expect(
    clippy::too_many_arguments,
    reason = "the parameter list is the guest ABI of this call, so grouping it would hide the contract and break the one-to-one match with the linker registration"
)]
pub(super) async fn wasix_sock_recv_from<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    flags: u16,
    ret_size: u32,
    ret_flags: u32,
    ret_addr: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

#[expect(
    clippy::too_many_arguments,
    reason = "the parameter list is the guest ABI of this call, so grouping it would hide the contract and break the one-to-one match with the linker registration"
)]
pub(super) async fn wasix_sock_recv_from_inner<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    flags: u16,
    ret_size: u32,
    ret_flags: u32,
    ret_addr: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let (iovs, ret_size, ret_flags, ret_addr) = (
        GuestAddress::wasm32(iovs),
        GuestAddress::wasm32(ret_size),
        GuestAddress::wasm32(ret_flags),
        GuestAddress::wasm32(ret_addr),
    );
    let layout = p1_read_iovs_with_byte_len(&memory, iovs, iovs_len).await?;
    let capacity = layout.byte_len;
    let descriptor = caller.data().descriptors.get(fd).cloned();
    let authority = match wasix_sock_recv_authority(descriptor.as_ref()) {
        Ok(authority) => authority,
        Err(errno) => return Ok(errno),
    };
    let status = caller.data().require_socket_authority(authority);
    if status != p1::errno::SUCCESS {
        return Ok(status);
    }
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
            socket,
            options,
            ..
        }))) => {
            let Some(service) = caller.data().runtime_state.network_service() else {
                return Ok(p1::errno::NETDOWN);
            };
            let fdflags = caller.data().descriptors.fdflags(fd)?;
            let timeout = wasix_effective_socket_timeout(options.receive_timeout, fdflags);
            let datagram = match service.udp_receive(socket.id(), capacity, timeout).await {
                Ok(Some(datagram)) => datagram,
                Ok(None) => return Ok(p1::errno::AGAIN),
                Err(error) => return Ok(p1_errno_from_udp_error_for_fdflags(error, fdflags)),
            };
            memory
                .write_iovs_from_bytes(&layout.iovs, &datagram.bytes, ret_size)
                .await?;
            memory
                .write_u16(ret_flags, flags & WASIX_RIFLAGS_DATA_TRUNCATED)
                .await?;
            write_wasix_addr_port_async(&memory, ret_addr, Some(datagram.address), datagram.port)
                .await?;
            Ok(p1::errno::SUCCESS)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound {
            ..
        }))) => Ok(p1::errno::INVAL),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { .. })) => {
            let bytes = caller
                .data_mut()
                .read_socket_pair(fd, capacity as usize)
                .await?;
            memory
                .write_iovs_from_bytes(&layout.iovs, &bytes, ret_size)
                .await?;
            memory.write_u16(ret_flags, 0).await?;
            write_wasix_addr_port_async(&memory, ret_addr, None, 0).await?;
            Ok(p1::errno::SUCCESS)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected {
                stream, options, ..
            },
        ))) => {
            let Some(service) = caller.data().runtime_state.network_service() else {
                return Ok(p1::errno::NETDOWN);
            };
            let fdflags = caller.data().descriptors.fdflags(fd)?;
            let timeout = wasix_effective_socket_timeout(options.receive_timeout, fdflags);
            // The stack copies straight out of its receive queue into the
            // guest's buffers, on this call's own poll, each page checked
            // present in the step that writes it.
            let ranges = memory.iov_ranges(&layout.iovs)?;
            let mut buffer = memory.tcp_receive_buffer(&ranges);
            let read = service
                .tcp_read_into(stream.id(), &mut buffer, timeout)
                .await;
            let received = match p1_tcp_received(read)? {
                Ok(received) => received,
                Err(error) => return Ok(p1_errno_from_tcp_error_for_fdflags(error, fdflags)),
            };
            memory.write_u32(ret_size, received).await?;
            memory.write_u16(ret_flags, 0).await?;
            Ok(p1::errno::SUCCESS)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected { .. },
        ))) => Ok(p1::errno::INVAL),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(_))) => Ok(p1::errno::INVAL),
        Some(_) => Ok(p1::errno::NOTSOCK),
        None => Ok(p1::errno::BADF),
    }
}

pub(super) async fn wasix_sock_send_to<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    _flags: u16,
    addr: u32,
    ret_size: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
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

pub(super) async fn wasix_sock_send_to_inner<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    _flags: u16,
    addr: u32,
    ret_size: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let (iovs, ret_size) = (GuestAddress::wasm32(iovs), GuestAddress::wasm32(ret_size));
    let layout = p1_read_iovs_with_byte_len(&memory, iovs, iovs_len).await?;
    let mut gather = Preview1Gather::new(&memory, &layout)?;
    let descriptor = caller.data().descriptors.get(fd).cloned();
    let authority = match wasix_sock_send_authority(descriptor.as_ref()) {
        Ok(authority) => authority,
        Err(errno) => return Ok(errno),
    };
    let status = caller.data().require_socket_authority(authority);
    if status != p1::errno::SUCCESS {
        return Ok(status);
    }
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(socket))) => {
            let (Some(address), port) =
                wasix_read_addr_port(&memory, GuestAddress::wasm32(addr)).await?
            else {
                return Ok(p1::errno::INVAL);
            };
            let (socket, send_timeout) = match socket {
                WasixUdpSocket::Bound {
                    socket, options, ..
                } => (socket.id(), options.send_timeout),
                WasixUdpSocket::Unbound { options, family } => {
                    let Some(service) = caller.data().runtime_state.network_service() else {
                        return Ok(p1::errno::NETDOWN);
                    };
                    let binding = match service.udp_bind(0).await {
                        Ok(binding) => binding,
                        Err(error) => return Ok(p1_errno_from_udp_error(error)),
                    };
                    if let Err(error) =
                        service.udp_set_hop_limit(binding.socket, options.hop_limit())
                    {
                        return Ok(p1_errno_from_udp_error(error));
                    }
                    let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(slot))) =
                        caller.data_mut().descriptors.get_mut(fd)
                    else {
                        return Ok(p1::errno::BADF);
                    };
                    *slot = WasixUdpSocket::Bound {
                        family,
                        socket: WasixOwnedUdpSocket::new(service.clone(), binding.socket),
                        local_port: binding.local_port,
                        options,
                    };
                    let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(
                        WasixUdpSocket::Bound {
                            socket, options, ..
                        },
                    ))) = caller.data().descriptors.get(fd)
                    else {
                        return Ok(p1::errno::BADF);
                    };
                    (socket.id(), options.send_timeout)
                }
            };
            let Some(service) = caller.data().runtime_state.network_service() else {
                return Ok(p1::errno::NETDOWN);
            };
            let fdflags = caller.data().descriptors.fdflags(fd)?;
            let timeout = wasix_effective_socket_timeout(send_timeout, fdflags);
            let bytes = gather
                .datagram(
                    &memory,
                    crate::wasmtime_adapter::wasi::MAX_WASI_UDP_DATAGRAM_BYTES,
                )
                .await?;
            let sent = match service
                .udp_send_address(socket, address, port, &bytes, timeout)
                .await
            {
                Ok(sent) => sent,
                Err(error) => return Ok(p1_errno_from_udp_error_for_fdflags(error, fdflags)),
            };
            let sent = u32::try_from(sent).map_err(|_| P1Stop::Errno(p1::errno::OVERFLOW))?;
            memory.write_u32(ret_size, sent).await?;
            Ok(p1::errno::SUCCESS)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected {
                stream, options, ..
            },
        ))) => {
            let Some(service) = caller.data().runtime_state.network_service() else {
                return Ok(p1::errno::NETDOWN);
            };
            let fdflags = caller.data().descriptors.fdflags(fd)?;
            let timeout = wasix_effective_socket_timeout(options.send_timeout, fdflags);
            let written = p1_send_gathered(&memory, &mut gather, |chunk| {
                let (service, stream) = (&service, stream.id());
                async move {
                    service
                        .tcp_write_all_bytes(stream, Bytes::from(chunk), timeout)
                        .await
                        .map_err(|error| p1_errno_from_tcp_error_for_fdflags(error, fdflags))
                }
            })
            .await?;
            memory.write_u32(ret_size, written).await?;
            Ok(p1::errno::SUCCESS)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected { .. },
        ))) => Ok(p1::errno::INVAL),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(_))) => Ok(p1::errno::INVAL),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { writer, .. })) => {
            let fdflags = caller.data().descriptors.fdflags(fd)?;
            let written = p1_send_gathered(&memory, &mut gather, |chunk| {
                p1_send_to_socketpair(&writer, chunk, fdflags)
            })
            .await?;
            memory.write_u32(ret_size, written).await?;
            Ok(p1::errno::SUCCESS)
        }
        Some(_) => Ok(p1::errno::NOTSOCK),
        None => Ok(p1::errno::BADF),
    }
}

/// Reads one range of a `sock_send_file` source descriptor.
async fn wasix_read_send_file_range<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    file: &crate::wasmtime_adapter::wasi::FsDescriptor,
    offset: u64,
    count: usize,
) -> Result<Bytes, i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let namespace = p1_namespace(caller);
    wasi_fs::read(&namespace, p1_grant(file)?, offset, count)
        .await
        .map_err(p1_errno_from_fs)
}

pub(super) async fn wasix_sock_send_file<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    out_fd: i32,
    in_fd: i32,
    offset: i64,
    count: i64,
    ret_size: u32,
) -> Result<i32, P1Stop>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let Some(memory) = p1_async_memory(caller) else {
        return Ok(p1::errno::FAULT);
    };
    let written = match wasix_send_file_bytes(caller, out_fd, in_fd, offset, count).await {
        Ok(written) => written,
        Err(errno) => return Ok(errno),
    };
    // The send awaited, so the guest's page is checked again as the count
    // is written.
    memory
        .write_u64(GuestAddress::wasm32(ret_size), written)
        .await?;
    Ok(p1::errno::SUCCESS)
}

/// Reads `count` bytes of the file `in_fd` from `offset` and sends them
/// to the socket `out_fd`, and answers how many went.
async fn wasix_send_file_bytes<CpuImpl, Net, StorageImpl>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, Net, StorageImpl>>,
    out_fd: i32,
    in_fd: i32,
    offset: i64,
    count: i64,
) -> Result<u64, i32>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let file = match caller.data().descriptors.get(in_fd) {
        Some(Preview1Descriptor::File { descriptor, .. }) => descriptor.clone(),
        Some(_) => return Err(p1::errno::BADF),
        None => return Err(p1::errno::BADF),
    };
    let offset = u64::try_from(offset).map_err(|_| p1::errno::INVAL)?;
    let count = u64::try_from(count).map_err(|_| p1::errno::INVAL)?;
    let count = usize::try_from(count).map_err(|_| p1::errno::OVERFLOW)?;
    // `sendfile` reads its source the same way `fd_pread` does: a host-share
    // file is pulled over 9p for the requested range, everything else comes
    // from the embedded node list.
    let bytes = wasix_read_send_file_range(caller, &file, offset, count).await?;
    let written = u64::try_from(bytes.len()).map_err(|_| p1::errno::OVERFLOW)?;
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
                return Err(status);
            }
            let Some(service) = caller.data().runtime_state.network_service() else {
                return Err(p1::errno::NETDOWN);
            };
            let fdflags = caller.data().descriptors.fdflags(out_fd)?;
            let timeout = wasix_effective_socket_timeout(options.send_timeout, fdflags);
            service
                .tcp_write_all_bytes(stream.id(), bytes, timeout)
                .await
                .map_err(|error| p1_errno_from_tcp_error_for_fdflags(error, fdflags))?;
            Ok(written)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { writer, .. })) => {
            let fdflags = caller.data().descriptors.fdflags(out_fd)?;
            if p1_fdflags_nonblocking(fdflags) {
                match writer.try_write(bytes) {
                    crate::TryWrite::Written => {}
                    crate::TryWrite::Full(_) => return Err(p1::errno::AGAIN),
                    crate::TryWrite::Closed => return Err(p1::errno::IO),
                }
            } else if writer.write(bytes).await.is_err() {
                return Err(p1::errno::IO);
            }
            Ok(written)
        }
        Some(Preview1Descriptor::Socket(_)) => Err(p1::errno::INVAL),
        Some(_) => Err(p1::errno::NOTSOCK),
        None => Err(p1::errno::BADF),
    }
}

/// Reads a `__wasi_addr_port_t`, which carries the address family in
/// its tag, so both `AF_INET` and `AF_INET6` peers arrive through the
/// same entry point.
pub(super) async fn wasix_read_addr_port(
    memory: &Preview1AsyncMemory,
    ptr: GuestAddress,
) -> Result<(Option<crate::NetworkIpAddress>, u16), P1Stop> {
    let tag = memory.read_u8(ptr).await?;
    match tag {
        WASIX_ADDRESS_FAMILY_UNSPEC => Ok((None, 0)),
        WASIX_ADDRESS_FAMILY_IP_INET4 => {
            let port = memory
                .read_u16(ptr.offset(u64::from(WASIX_ADDR_PORT_UNION_OFFSET)))
                .await?;
            let octets = memory
                .read_array(ptr.offset(u64::from(WASIX_ADDR_PORT_IP4_ADDRESS_OFFSET)))
                .await?;
            Ok((
                Some(crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new(
                    octets,
                ))),
                port,
            ))
        }
        WASIX_ADDRESS_FAMILY_IP_INET6 => {
            let port = memory
                .read_u16(ptr.offset(u64::from(WASIX_ADDR_PORT_UNION_OFFSET)))
                .await?;
            let octets = memory
                .read_array(ptr.offset(u64::from(WASIX_ADDR_PORT_IP6_ADDRESS_OFFSET)))
                .await?;
            Ok((
                Some(crate::NetworkIpAddress::Ipv6(
                    helios_netstack::Ipv6Address::new(octets),
                )),
                port,
            ))
        }
        WASIX_ADDRESS_FAMILY_UNIX => Err(P1Stop::Errno(p1::errno::NOTSUP)),
        _ => Err(P1Stop::Errno(p1::errno::INVAL)),
    }
}

pub(super) async fn wasix_read_addr_ip4(
    memory: &Preview1AsyncMemory,
    ptr: GuestAddress,
) -> Result<crate::Ipv4Address, P1Stop> {
    let tag = memory.read_u8(ptr).await?;
    match tag {
        WASIX_ADDRESS_FAMILY_IP_INET4 => {
            let octets = memory
                .read_array(ptr.offset(u64::from(WASIX_ADDR_IP_UNION_OFFSET)))
                .await?;
            Ok(crate::Ipv4Address::new(octets))
        }
        WASIX_ADDRESS_FAMILY_IP_INET6 | WASIX_ADDRESS_FAMILY_UNIX => {
            Err(P1Stop::Errno(p1::errno::NOTSUP))
        }
        WASIX_ADDRESS_FAMILY_UNSPEC => Err(P1Stop::Errno(p1::errno::INVAL)),
        _ => Err(P1Stop::Errno(p1::errno::INVAL)),
    }
}

pub(super) fn wasix_validate_addr_ip6<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
) -> Result<(), i32> {
    let tag = p1_try_read_u8(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?;
    wasix_addr_ip6_family_status(tag)?;
    let mut octets = [0_u8; 16];
    p1_read_memory_into(
        caller,
        memory,
        ptr.offset(u64::from(WASIX_ADDR_IP_UNION_OFFSET)),
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

pub(super) async fn wasix_read_addr_cidr_ip4(
    memory: &Preview1AsyncMemory,
    ptr: GuestAddress,
) -> Result<crate::Ipv4Cidr, P1Stop> {
    let tag = memory.read_u8(ptr).await?;
    match tag {
        WASIX_ADDRESS_FAMILY_IP_INET4 => {
            let octets = memory
                .read_array(ptr.offset(u64::from(WASIX_ADDR_CIDR_IP4_ADDRESS_OFFSET)))
                .await?;
            let prefix = memory
                .read_u8(ptr.offset(u64::from(WASIX_ADDR_CIDR_IP4_PREFIX_OFFSET)))
                .await?;
            if prefix > 32 {
                return Err(P1Stop::Errno(p1::errno::INVAL));
            }
            Ok(crate::Ipv4Cidr::new(
                crate::Ipv4Address::new(octets),
                prefix,
            ))
        }
        WASIX_ADDRESS_FAMILY_IP_INET6 | WASIX_ADDRESS_FAMILY_UNIX => {
            Err(P1Stop::Errno(p1::errno::NOTSUP))
        }
        WASIX_ADDRESS_FAMILY_UNSPEC => Err(P1Stop::Errno(p1::errno::INVAL)),
        _ => Err(P1Stop::Errno(p1::errno::INVAL)),
    }
}

pub(super) async fn wasix_read_option_pid(
    memory: &Preview1AsyncMemory,
    ptr: GuestAddress,
) -> Result<Option<u32>, P1Stop> {
    let tag = memory.read_u8(ptr).await?;
    match tag {
        WASIX_OPTION_NONE => Ok(None),
        WASIX_OPTION_SOME => Ok(Some(
            memory
                .read_u32(ptr.offset(u64::from(WASIX_OPTION_UNION_U32_OFFSET)))
                .await?,
        )),
        _ => Err(P1Stop::Errno(p1::errno::INVAL)),
    }
}

pub(super) fn wasix_read_optional_timestamp<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
) -> Result<Option<u64>, i32> {
    if ptr == GuestAddress::wasm32(0) {
        return Ok(None);
    }
    let tag = p1_try_read_u8(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?;
    match tag {
        0 => Ok(None),
        1 => p1_try_read_u64(caller, memory, ptr.offset(8))
            .map(Some)
            .map_err(|_| p1::errno::FAULT),
        _ => Err(p1::errno::INVAL),
    }
}

/// An optional-`u64` record as the guest's `__wasi_option_timestamp_t`
/// lays it out: the tag, then the value at its own offset.
pub(super) fn wasix_optional_timestamp_record(value: Option<u64>) -> [u8; 16] {
    let mut record = [0_u8; 16];
    record[0] = if value.is_some() {
        WASIX_OPTION_SOME
    } else {
        WASIX_OPTION_NONE
    };
    let offset = WASIX_OPTION_UNION_U64_OFFSET as usize;
    record[offset..offset + 8].copy_from_slice(&value.unwrap_or(0).to_le_bytes());
    record
}

pub(super) fn p1_write_wasix_optional_timestamp<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
    value: Option<u64>,
) -> i32 {
    p1_write_memory(caller, memory, ptr, &wasix_optional_timestamp_record(value))
}

/// [`wasix_read_optional_timestamp`] for a host call that awaits.
pub(super) async fn wasix_read_optional_timestamp_async(
    memory: &Preview1AsyncMemory,
    ptr: GuestAddress,
) -> Result<Option<u64>, P1Stop> {
    if ptr == GuestAddress::wasm32(0) {
        return Ok(None);
    }
    match memory.read_u8(ptr).await? {
        0 => Ok(None),
        1 => Ok(Some(memory.read_u64(ptr.offset(8)).await?)),
        _ => Err(P1Stop::Errno(p1::errno::INVAL)),
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
    ptr: GuestAddress,
    value: bool,
) -> i32 {
    p1_write_u8(caller, memory, ptr, u8::from(value))
}

pub(super) fn p1_read_wasix_bool<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
) -> Result<bool, i32> {
    match p1_try_read_u8(caller, memory, ptr).map_err(|_| p1::errno::FAULT)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(p1::errno::INVAL),
    }
}

/// Writes a `__wasi_addr_t` in the address's own family. The union is
/// 16 bytes wide, so an IPv6 address fills it exactly and an IPv4 one
/// occupies its first four bytes.
pub(super) async fn write_wasix_addr_ip(
    memory: &Preview1AsyncMemory,
    ptr: GuestAddress,
    address: crate::NetworkIpAddress,
) -> Result<(), GuestAccessError> {
    match address {
        crate::NetworkIpAddress::Ipv4(address) => write_wasix_addr_ip4(memory, ptr, address).await,
        crate::NetworkIpAddress::Ipv6(address) => {
            memory
                .write_bytes(ptr, &[WASIX_ADDRESS_FAMILY_IP_INET6])
                .await?;
            memory
                .write_bytes(
                    ptr.offset(u64::from(WASIX_ADDR_IP_UNION_OFFSET)),
                    &address.octets(),
                )
                .await
        }
    }
}

pub(super) async fn write_wasix_addr_ip4(
    memory: &Preview1AsyncMemory,
    ptr: GuestAddress,
    address: crate::Ipv4Address,
) -> Result<(), GuestAccessError> {
    memory
        .write_bytes(ptr, &[WASIX_ADDRESS_FAMILY_IP_INET4])
        .await?;
    memory
        .write_bytes(
            ptr.offset(u64::from(WASIX_ADDR_IP_UNION_OFFSET)),
            &address.octets(),
        )
        .await
}

pub(super) async fn write_wasix_addr_cidr_ip4(
    memory: &Preview1AsyncMemory,
    ptr: GuestAddress,
    cidr: crate::Ipv4Cidr,
) -> Result<(), GuestAccessError> {
    let mut bytes = [0_u8; WASIX_ADDR_CIDR_SIZE as usize];
    bytes[0] = WASIX_ADDRESS_FAMILY_IP_INET4;
    bytes[WASIX_ADDR_CIDR_IP4_ADDRESS_OFFSET as usize
        ..WASIX_ADDR_CIDR_IP4_ADDRESS_OFFSET as usize + 4]
        .copy_from_slice(&cidr.address().octets());
    bytes[WASIX_ADDR_CIDR_IP4_PREFIX_OFFSET as usize] = cidr.prefix_len();
    memory.write_bytes(ptr, &bytes).await
}

pub(super) async fn write_wasix_route_ip4(
    memory: &Preview1AsyncMemory,
    ptr: GuestAddress,
    route: crate::Ipv4Route,
) -> Result<(), GuestAccessError> {
    write_wasix_addr_cidr_ip4(
        memory,
        ptr.offset(u64::from(WASIX_ROUTE_CIDR_OFFSET)),
        route.destination(),
    )
    .await?;
    write_wasix_addr_ip4(
        memory,
        ptr.offset(u64::from(WASIX_ROUTE_ROUTER_OFFSET)),
        route.gateway(),
    )
    .await?;
    memory
        .write_bytes(
            ptr.offset(u64::from(WASIX_ROUTE_PREFERRED_UNTIL_OFFSET)),
            &wasix_optional_timestamp_record(route.preferred_until_nanos()),
        )
        .await?;
    memory
        .write_bytes(
            ptr.offset(u64::from(WASIX_ROUTE_EXPIRES_AT_OFFSET)),
            &wasix_optional_timestamp_record(route.expires_at_nanos()),
        )
        .await
}

/// Writes a `__wasi_addr_port_t` in the address's own family, so an
/// `AF_INET6` peer is reported as one rather than being flattened.
pub(super) fn write_wasix_addr_port<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
    address: crate::NetworkIpAddress,
    port: u16,
) -> i32 {
    let (record, len) = wasix_addr_port_record(Some(address), port);
    p1_write_memory(caller, memory, ptr, &record[..len])
}

pub(super) fn write_wasix_addr_port_unspec<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: GuestAddress,
) -> i32 {
    let (record, len) = wasix_addr_port_record(None, 0);
    p1_write_memory(caller, memory, ptr, &record[..len])
}

/// [`write_wasix_addr_port`] and [`write_wasix_addr_port_unspec`] for a
/// host call that awaits: `None` is `AF_UNSPEC`.
pub(super) async fn write_wasix_addr_port_async(
    memory: &Preview1AsyncMemory,
    ptr: GuestAddress,
    address: Option<crate::NetworkIpAddress>,
    port: u16,
) -> Result<(), GuestAccessError> {
    let (record, len) = wasix_addr_port_record(address, port);
    memory.write_bytes(ptr, &record[..len]).await
}

/// A `__wasi_addr_port_t` in the address's own family, and how many of its
/// bytes the family uses: an `AF_UNSPEC` or `AF_INET` record fills the tag,
/// the port and four address bytes, an `AF_INET6` one sixteen.
fn wasix_addr_port_record(
    address: Option<crate::NetworkIpAddress>,
    port: u16,
) -> ([u8; 20], usize) {
    let mut record = [0_u8; 20];
    let port_offset = WASIX_ADDR_PORT_UNION_OFFSET as usize;
    record[port_offset..port_offset + 2].copy_from_slice(&port.to_le_bytes());
    let address_offset = WASIX_ADDR_PORT_IP4_ADDRESS_OFFSET as usize;
    match address {
        None => {
            record[0] = WASIX_ADDRESS_FAMILY_UNSPEC;
            (record, address_offset + 4)
        }
        Some(crate::NetworkIpAddress::Ipv4(address)) => {
            record[0] = WASIX_ADDRESS_FAMILY_IP_INET4;
            record[address_offset..address_offset + 4].copy_from_slice(&address.octets());
            (record, address_offset + 4)
        }
        Some(crate::NetworkIpAddress::Ipv6(address)) => {
            record[0] = WASIX_ADDRESS_FAMILY_IP_INET6;
            let address_offset = WASIX_ADDR_PORT_IP6_ADDRESS_OFFSET as usize;
            record[address_offset..address_offset + 16].copy_from_slice(&address.octets());
            (record, address_offset + 16)
        }
    }
}

pub(super) fn wasix_effective_socket_timeout(option_timeout: Option<u64>, fdflags: u16) -> u64 {
    if p1_fdflags_nonblocking(fdflags) {
        0
    } else {
        option_timeout.unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_over(bytes: &mut [u8]) -> Preview1AsyncMemory {
        Preview1AsyncMemory::new(Preview1Memory {
            base: bytes.as_mut_ptr() as usize,
            len: bytes.len(),
        })
    }

    fn run<F: Future>(future: F) -> F::Output {
        futures_lite::future::block_on(future)
    }

    /// An optional pid decodes as the guest's record has it: a tag, then
    /// the value at its own offset; a tag that is neither is `EINVAL` and
    /// a record outside memory is `EFAULT`.
    #[test]
    fn an_optional_pid_decodes_its_tag_and_value() {
        let mut guest = vec![0_u8; 32];
        guest[8] = WASIX_OPTION_SOME;
        guest[8 + WASIX_OPTION_UNION_U32_OFFSET as usize..][..4]
            .copy_from_slice(&4242_u32.to_le_bytes());
        guest[16] = WASIX_OPTION_NONE;
        guest[24] = 9;
        guest[30] = WASIX_OPTION_SOME;
        let memory = memory_over(&mut guest);
        let read = |ptr| run(wasix_read_option_pid(&memory, GuestAddress::wasm32(ptr)));
        assert_eq!(read(8), Ok(Some(4242)));
        assert_eq!(read(16), Ok(None));
        assert_eq!(read(24), Err(P1Stop::Errno(p1::errno::INVAL)));
        assert_eq!(read(32), Err(P1Stop::Errno(p1::errno::FAULT)));
        assert_eq!(
            read(30),
            Err(P1Stop::Errno(p1::errno::FAULT)),
            "the value of a record that starts inside memory but ends outside it"
        );
    }

    /// An IPv4 address record that straddles a present page and one swap
    /// has taken, at the end of a two-page memory.
    fn ip4_record_over_an_evicted_page() -> (crate::memory::GuestSwap, GuestAddress) {
        let mut swap = crate::memory::GuestSwap::new(2);
        // The tag on page zero, the octets on page one.
        let at = TEST_PAGE - WASIX_ADDR_IP_UNION_OFFSET as usize;
        swap.bytes_mut()[at] = WASIX_ADDRESS_FAMILY_IP_INET4;
        swap.bytes_mut()[TEST_PAGE..TEST_PAGE + 4].copy_from_slice(&[198, 51, 100, 7]);
        swap.evict(1);
        (swap, GuestAddress::wasm32(at as u32))
    }

    /// Reading a socket address whose octets are on a page that is out
    /// waits for the page, asks for it, and once it is back decodes the
    /// address the guest wrote, not the page's absence.
    #[test]
    fn an_address_on_an_evicted_page_reads_once_the_page_is_back() {
        let (mut swap, record) = ip4_record_over_an_evicted_page();
        let memory = Preview1AsyncMemory::over_swap(&swap);
        let mut read = core::pin::pin!(wasix_read_addr_ip4(&memory, record));
        assert!(poll_once(read.as_mut()).is_none(), "the read waits");
        assert_eq!(swap.queued_faults(), 1, "the page was asked for");

        swap.serve();
        assert_eq!(
            poll_once(read.as_mut()),
            Some(Ok(crate::Ipv4Address::new([198, 51, 100, 7])))
        );
    }

    /// The address's page cannot come back, so the call traps the guest.
    #[test]
    fn an_address_on_a_lost_page_traps_the_guest() {
        let (mut swap, record) = ip4_record_over_an_evicted_page();
        swap.fail_reads(1);
        let memory = Preview1AsyncMemory::over_swap(&swap);
        let mut read = core::pin::pin!(wasix_read_addr_ip4(&memory, record));
        assert!(poll_once(read.as_mut()).is_none());
        swap.serve();
        assert_eq!(
            poll_once(read.as_mut()),
            Some(Err(P1Stop::Trap(crate::memory::SwapFaultError::Backend)))
        );
    }

    /// A receive's source address lands on a page that was out once the
    /// page is back, whole.
    #[test]
    fn a_peer_address_lands_on_an_evicted_page_once_it_is_back() {
        let mut swap = crate::memory::GuestSwap::new(2);
        swap.evict(1);
        let memory = Preview1AsyncMemory::over_swap(&swap);
        let at = GuestAddress::wasm32((TEST_PAGE - 4) as u32);
        let peer = crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([203, 0, 113, 9]));
        let mut write = core::pin::pin!(write_wasix_addr_port_async(&memory, at, Some(peer), 8080));
        assert!(poll_once(write.as_mut()).is_none());
        swap.serve();
        assert_eq!(poll_once(write.as_mut()), Some(Ok(())));

        let mut plain = vec![0_u8; 2 * TEST_PAGE];
        run(write_wasix_addr_port_async(
            &memory_over(&mut plain),
            at,
            Some(peer),
            8080,
        ))
        .expect("in bounds");
        assert!(
            swap.bytes()[TEST_PAGE - 4..TEST_PAGE + 16] == plain[TEST_PAGE - 4..TEST_PAGE + 16],
            "the record is the one a present page gets"
        );
    }
}
