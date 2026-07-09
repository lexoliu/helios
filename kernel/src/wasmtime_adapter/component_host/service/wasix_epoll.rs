use super::*;

pub(super) fn add_wasix_epoll_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            WASIX_MODULE,
            "epoll_create",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ret_fd: i32| -> i32 {
                wasix_epoll_create(&mut caller, ret_fd as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "epoll_ctl",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             epfd: i32,
             op: i32,
             fd: i32,
             event: i32|
             -> i32 { wasix_epoll_ctl(&mut caller, epfd, op, fd, event as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "epoll_wait",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (epfd, events, maxevents, timeout, ret_nevents): (i32, i32, i32, i64, i32)| {
                Box::new(async move {
                    wasix_epoll_wait(
                        &mut caller,
                        epfd,
                        events as u32,
                        maxevents,
                        timeout,
                        ret_nevents as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

pub(super) fn wasix_epoll_create<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let fd = match caller
        .data_mut()
        .descriptors
        .insert(Preview1Descriptor::Epoll(EpollDescriptor {
            interests: Vec::new(),
        })) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, ret_fd, fd)
}

pub(super) fn wasix_epoll_ctl<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    epfd: i32,
    op: i32,
    fd: i32,
    event: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match caller.data().descriptors.get(epfd) {
        Some(Preview1Descriptor::Epoll(_)) => {}
        Some(_) => return p1::errno::INVAL,
        None => return p1::errno::BADF,
    }
    if caller.data().descriptors.get(fd).is_none() {
        return p1::errno::BADF;
    }
    let interest = if op == WASIX_EPOLL_CTL_DEL {
        None
    } else {
        let Some(memory) = p1_memory(caller) else {
            return p1::errno::FAULT;
        };
        match wasix_read_epoll_event(caller, memory, event, fd) {
            Ok(event) => Some(event),
            Err(errno) => return errno,
        }
    };
    let Some(Preview1Descriptor::Epoll(epoll)) = caller.data_mut().descriptors.get_mut(epfd) else {
        return p1::errno::BADF;
    };
    let existing = epoll
        .interests
        .iter()
        .position(|registered| registered.fd == fd);
    match (op, existing, interest) {
        (WASIX_EPOLL_CTL_ADD, Some(_), _) => p1::errno::EXIST,
        (WASIX_EPOLL_CTL_ADD, None, Some(interest)) => {
            epoll.interests.push(interest);
            p1::errno::SUCCESS
        }
        (WASIX_EPOLL_CTL_MOD, Some(index), Some(interest)) => {
            epoll.interests[index] = interest;
            p1::errno::SUCCESS
        }
        (WASIX_EPOLL_CTL_MOD, None, _) | (WASIX_EPOLL_CTL_DEL, None, _) => p1::errno::NOENT,
        (WASIX_EPOLL_CTL_DEL, Some(index), None) => {
            epoll.interests.remove(index);
            p1::errno::SUCCESS
        }
        _ => p1::errno::INVAL,
    }
}

pub(super) async fn wasix_epoll_wait<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    epfd: i32,
    events: u32,
    maxevents: i32,
    timeout: i64,
    ret_nevents: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if maxevents <= 0 {
        return p1::errno::INVAL;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let maxevents = match u32::try_from(maxevents) {
        Ok(maxevents) => maxevents,
        Err(_) => return p1::errno::INVAL,
    };
    match caller.data().descriptors.get(epfd) {
        Some(Preview1Descriptor::Epoll(_)) => {}
        Some(_) => return p1::errno::INVAL,
        None => return p1::errno::BADF,
    }
    let mut ready = wasix_collect_epoll_events(caller, epfd, maxevents);
    if ready.is_empty() && timeout != 0 {
        let targets = match wasix_epoll_wait_targets(caller, epfd) {
            Ok(targets) => targets,
            Err(errno) => return errno,
        };
        if let Err(errno) =
            wasix_wait_epoll_readiness(caller.data().timer(), targets, timeout).await
        {
            return errno;
        }
        ready = wasix_collect_epoll_events(caller, epfd, maxevents);
    }
    for (index, event) in ready.iter().enumerate() {
        let offset = match (index as u32).checked_mul(WASIX_EPOLL_EVENT_SIZE) {
            Some(offset) => offset,
            None => return p1::errno::OVERFLOW,
        };
        let event_ptr = match events.checked_add(offset) {
            Some(event_ptr) => event_ptr,
            None => return p1::errno::OVERFLOW,
        };
        let status = wasix_write_epoll_event(caller, memory, event_ptr, event);
        if status != p1::errno::SUCCESS {
            return status;
        }
    }
    let returned = match u32::try_from(ready.len()) {
        Ok(returned) => returned,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, ret_nevents, returned)
}

pub(super) async fn wasix_wait_epoll_readiness<CpuImpl>(
    timer: crate::Timer<CpuImpl>,
    mut targets: Vec<EpollWaitTarget>,
    timeout: i64,
) -> Result<(), i32>
where
    CpuImpl: Cpu + Clone,
{
    if timeout < 0 {
        if targets.is_empty() {
            core::future::pending::<()>().await;
            return Ok(());
        }
        core::future::poll_fn(|cx| poll_epoll_wait_targets(&mut targets, cx)).await;
        return Ok(());
    }

    let mut timer = core::pin::pin!(timer.sleep_for(Duration::from_nanos(timeout as u64)));
    if targets.is_empty() {
        timer.await;
        return Ok(());
    }
    core::future::poll_fn(|cx| {
        if poll_epoll_wait_targets(&mut targets, cx).is_ready() {
            return Poll::Ready(());
        }
        if timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(());
        }
        Poll::Pending
    })
    .await;
    Ok(())
}

pub(super) fn poll_epoll_wait_targets(
    targets: &mut [EpollWaitTarget],
    cx: &mut Context<'_>,
) -> Poll<()> {
    for target in targets {
        if target.poll(cx).is_ready() {
            return Poll::Ready(());
        }
    }
    Poll::Pending
}

pub(super) fn wasix_epoll_wait_targets<CpuImpl, HostFs>(
    caller: &Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    epfd: i32,
) -> Result<Vec<EpollWaitTarget>, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let interests = match caller.data().descriptors.get(epfd) {
        Some(Preview1Descriptor::Epoll(epoll)) => epoll.interests.clone(),
        Some(_) => return Err(p1::errno::INVAL),
        None => return Err(p1::errno::BADF),
    };
    let mut targets = Vec::new();
    for interest in interests {
        if interest.events & WASIX_EPOLL_TYPE_EPOLLIN == 0 {
            continue;
        }
        match caller.data().descriptors.get(interest.fd) {
            Some(Preview1Descriptor::PipeRead { reader, carry }) if carry.is_empty() => {
                targets.push(EpollWaitTarget::ByteReader {
                    reader: reader.clone(),
                    wait: reader.wait_state(),
                });
            }
            Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair {
                reader, carry, ..
            })) if carry.is_empty() => {
                targets.push(EpollWaitTarget::ByteReader {
                    reader: reader.clone(),
                    wait: reader.wait_state(),
                });
            }
            Some(Preview1Descriptor::Event(event)) if !event.is_readable() => {
                targets.push(EpollWaitTarget::Event {
                    event: event.clone(),
                    wait: event.wait_state(),
                });
            }
            _ => {}
        }
    }
    Ok(targets)
}

pub(super) fn wasix_read_epoll_event<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    fd: i32,
) -> Result<EpollInterest, i32> {
    let events = p1_try_read_u32(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?;
    let user_data = EpollUserData {
        ptr: p1_try_read_u32(caller, memory, ptr + WASIX_EPOLL_EVENT_DATA_OFFSET)
            .map_err(|_| p1::errno::FAULT)?,
        fd: p1_try_read_u32(caller, memory, ptr + WASIX_EPOLL_EVENT_DATA_FD_OFFSET)
            .map_err(|_| p1::errno::FAULT)?,
        data1: p1_try_read_u32(caller, memory, ptr + WASIX_EPOLL_EVENT_DATA1_OFFSET)
            .map_err(|_| p1::errno::FAULT)?,
        data2: p1_try_read_u64(caller, memory, ptr + WASIX_EPOLL_EVENT_DATA2_OFFSET)
            .map_err(|_| p1::errno::FAULT)?,
    };
    Ok(EpollInterest {
        fd,
        events,
        data: user_data,
    })
}

pub(super) fn wasix_write_epoll_event<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    event: &EpollInterest,
) -> i32 {
    p1_write_u32(caller, memory, ptr, event.events)
        .max(p1_write_u32(
            caller,
            memory,
            ptr + WASIX_EPOLL_EVENT_PADDING_OFFSET,
            0,
        ))
        .max(p1_write_u32(
            caller,
            memory,
            ptr + WASIX_EPOLL_EVENT_DATA_OFFSET,
            event.data.ptr,
        ))
        .max(p1_write_u32(
            caller,
            memory,
            ptr + WASIX_EPOLL_EVENT_DATA_FD_OFFSET,
            event.data.fd,
        ))
        .max(p1_write_u32(
            caller,
            memory,
            ptr + WASIX_EPOLL_EVENT_DATA1_OFFSET,
            event.data.data1,
        ))
        .max(p1_write_u32(
            caller,
            memory,
            ptr + WASIX_EPOLL_EVENT_DATA_PADDING_OFFSET,
            0,
        ))
        .max(p1_write_u64(
            caller,
            memory,
            ptr + WASIX_EPOLL_EVENT_DATA2_OFFSET,
            event.data.data2,
        ))
}

pub(super) fn wasix_collect_epoll_events<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    epfd: i32,
    maxevents: u32,
) -> Vec<EpollInterest>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let interests = match caller.data().descriptors.get(epfd) {
        Some(Preview1Descriptor::Epoll(epoll)) => epoll.interests.clone(),
        _ => return Vec::new(),
    };
    let mut ready = Vec::new();
    let mut oneshot_fds = Vec::new();
    for mut interest in interests {
        if ready.len() >= maxevents as usize {
            break;
        }
        let events =
            wasix_epoll_ready_mask(caller.data().descriptors.get(interest.fd), interest.events);
        if events == 0 {
            continue;
        }
        let oneshot = interest.events & WASIX_EPOLL_TYPE_EPOLLONESHOT != 0;
        interest.events = events;
        if oneshot {
            oneshot_fds.push(interest.fd);
        }
        ready.push(interest);
    }
    if !oneshot_fds.is_empty() {
        if let Some(Preview1Descriptor::Epoll(epoll)) = caller.data_mut().descriptors.get_mut(epfd)
        {
            epoll
                .interests
                .retain(|interest| !oneshot_fds.contains(&interest.fd));
        }
    }
    ready
}

pub(super) fn wasix_epoll_ready_mask(
    descriptor: Option<&Preview1Descriptor>,
    interest: u32,
) -> u32 {
    let Some(descriptor) = descriptor else {
        return WASIX_EPOLL_TYPE_EPOLLERR | WASIX_EPOLL_TYPE_EPOLLHUP;
    };
    let mut ready = 0;
    if interest & WASIX_EPOLL_TYPE_EPOLLIN != 0 {
        match p1_poll_descriptor(Some(descriptor), P1_EVENTTYPE_FD_READ) {
            Ok(bytes) if bytes != 0 => ready |= WASIX_EPOLL_TYPE_EPOLLIN,
            Err(_) => ready |= WASIX_EPOLL_TYPE_EPOLLERR,
            _ => {}
        }
    }
    if interest & WASIX_EPOLL_TYPE_EPOLLOUT != 0 {
        match p1_poll_descriptor(Some(descriptor), P1_EVENTTYPE_FD_WRITE) {
            Ok(_) => ready |= WASIX_EPOLL_TYPE_EPOLLOUT,
            Err(_) => ready |= WASIX_EPOLL_TYPE_EPOLLERR,
        }
    }
    ready
}
