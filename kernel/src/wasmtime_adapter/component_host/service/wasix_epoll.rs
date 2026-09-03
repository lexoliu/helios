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
    // A negative timeout blocks indefinitely; otherwise anchor the deadline
    // once so the wait loop cannot restart its countdown on every re-probe.
    let deadline_nanos = if timeout < 0 {
        None
    } else {
        Some(caller.data().now_nanos().saturating_add(timeout as u64))
    };

    let ready = loop {
        let ready = wasix_collect_epoll_events(caller, epfd, maxevents).await;
        if !ready.is_empty() {
            break ready;
        }
        let remaining = match deadline_nanos {
            None => None,
            Some(deadline) => {
                let now = caller.data().now_nanos();
                if now >= deadline {
                    // Includes `timeout == 0`, which makes epoll_wait a
                    // non-blocking readiness snapshot.
                    break ready;
                }
                Some(Duration::from_nanos(deadline - now))
            }
        };
        let mut wait = match wasix_epoll_wait_set(caller, epfd) {
            Ok(wait) => wait,
            Err(errno) => return errno,
        };
        let timer = caller.data().timer();
        p1_wait_step(&timer, &mut wait, remaining).await;
    };

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

/// Collect everything the registered interests need in order to be woken.
///
/// This used to build wait targets only for pipes, socketpairs and eventfds,
/// so `epoll_wait` on a TCP/UDP socket or on stdin had nothing to wait on:
/// with an infinite timeout it parked forever, and with a timeout it always
/// timed out. `p1_add_wait_target` covers every descriptor kind, flagging the
/// poll-driven ones for re-probing.
pub(super) fn wasix_epoll_wait_set<CpuImpl, HostFs>(
    caller: &Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    epfd: i32,
) -> Result<P1WaitSet, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let interests = match caller.data().descriptors.get(epfd) {
        Some(Preview1Descriptor::Epoll(epoll)) => epoll.interests.clone(),
        Some(_) => return Err(p1::errno::INVAL),
        None => return Err(p1::errno::BADF),
    };
    let mut wait = P1WaitSet::new();
    for interest in interests {
        if interest.events & WASIX_EPOLL_TYPE_EPOLLIN != 0 {
            p1_add_wait_target(caller.data(), interest.fd, P1_EVENTTYPE_FD_READ, &mut wait);
        }
        if interest.events & WASIX_EPOLL_TYPE_EPOLLOUT != 0 {
            p1_add_wait_target(caller.data(), interest.fd, P1_EVENTTYPE_FD_WRITE, &mut wait);
        }
    }
    Ok(wait)
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

pub(super) async fn wasix_collect_epoll_events<CpuImpl, HostFs>(
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
        let events = wasix_epoll_ready_mask(caller, interest.fd, interest.events).await;
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
    if !oneshot_fds.is_empty()
        && let Some(Preview1Descriptor::Epoll(epoll)) = caller.data_mut().descriptors.get_mut(epfd)
        {
            epoll
                .interests
                .retain(|interest| !oneshot_fds.contains(&interest.fd));
        }
    ready
}

/// Compute the ready mask for one registered interest.
///
/// The old mask required a non-zero byte count for `EPOLLIN`. Sockets and
/// regular files reported `Ok(0)` from the probe whether or not they were
/// readable, so they never raised `EPOLLIN`. The tri-state readiness makes
/// "ready with nothing buffered" (end-of-stream, a drained regular file)
/// distinguishable from "would block".
pub(super) async fn wasix_epoll_ready_mask<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    interest: u32,
) -> u32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if caller.data().descriptors.get(fd).is_none() {
        return WASIX_EPOLL_TYPE_EPOLLERR | WASIX_EPOLL_TYPE_EPOLLHUP;
    }
    let mut ready = 0;
    if interest & WASIX_EPOLL_TYPE_EPOLLIN != 0 {
        ready |= wasix_epoll_mask_bit(
            p1_descriptor_readiness(caller, fd, P1_EVENTTYPE_FD_READ).await,
            P1_EVENTTYPE_FD_READ,
        );
    }
    if interest & WASIX_EPOLL_TYPE_EPOLLOUT != 0 {
        ready |= wasix_epoll_mask_bit(
            p1_descriptor_readiness(caller, fd, P1_EVENTTYPE_FD_WRITE).await,
            P1_EVENTTYPE_FD_WRITE,
        );
    }
    ready
}

/// Fold one direction's readiness into the epoll bits it implies.
///
/// `Ready` and `Hangup` both raise the direction's bit — an ended stream is
/// reported as readable so the guest performs the read that returns zero.
/// Only a hangup additionally raises `EPOLLHUP`.
pub(super) fn wasix_epoll_mask_bit(readiness: Result<P1Readiness, i32>, event_type: u8) -> u32 {
    let bit = if event_type == P1_EVENTTYPE_FD_WRITE {
        WASIX_EPOLL_TYPE_EPOLLOUT
    } else {
        WASIX_EPOLL_TYPE_EPOLLIN
    };
    match readiness {
        Ok(P1Readiness::Pending) => 0,
        Ok(P1Readiness::Hangup) => bit | WASIX_EPOLL_TYPE_EPOLLHUP,
        Ok(P1Readiness::Ready { .. }) => bit,
        Err(_) => WASIX_EPOLL_TYPE_EPOLLERR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket_mask(readiness: crate::SocketReadiness, event_type: u8) -> u32 {
        wasix_epoll_mask_bit(
            Ok(p1_readiness_from_socket(readiness, event_type)),
            event_type,
        )
    }

    /// A socket holding buffered bytes raises `EPOLLIN`. The old mask keyed
    /// off a non-zero byte count, and the socket probe always answered zero,
    /// so a readable socket never showed up in `epoll_wait`.
    #[test]
    fn epoll_mask_reports_a_socket_with_buffered_data() {
        assert_eq!(
            socket_mask(
                crate::SocketReadiness {
                    readable: true,
                    writable: true,
                    hangup: false,
                },
                P1_EVENTTYPE_FD_READ,
            ),
            WASIX_EPOLL_TYPE_EPOLLIN
        );
    }

    /// A peer that closed its send side is readable *and* hung up, so the
    /// guest performs the read that returns zero and sees the EOF.
    #[test]
    fn epoll_mask_reports_a_socket_at_end_of_stream() {
        assert_eq!(
            socket_mask(
                crate::SocketReadiness {
                    readable: true,
                    writable: true,
                    hangup: true,
                },
                P1_EVENTTYPE_FD_READ,
            ),
            WASIX_EPOLL_TYPE_EPOLLIN | WASIX_EPOLL_TYPE_EPOLLHUP
        );
    }

    /// A socket with an empty receive queue contributes no bits at all, so
    /// `epoll_wait` goes on to sleep instead of returning a phantom event.
    #[test]
    fn epoll_mask_ignores_a_socket_that_would_block() {
        assert_eq!(
            socket_mask(
                crate::SocketReadiness {
                    readable: false,
                    writable: true,
                    hangup: false,
                },
                P1_EVENTTYPE_FD_READ,
            ),
            0
        );
    }

    /// Writability is tracked separately: a socket that cannot send yet must
    /// not raise `EPOLLOUT`.
    #[test]
    fn epoll_mask_tracks_socket_writability() {
        assert_eq!(
            socket_mask(
                crate::SocketReadiness {
                    readable: false,
                    writable: true,
                    hangup: false,
                },
                P1_EVENTTYPE_FD_WRITE,
            ),
            WASIX_EPOLL_TYPE_EPOLLOUT
        );
        assert_eq!(
            socket_mask(
                crate::SocketReadiness {
                    readable: true,
                    writable: false,
                    hangup: false,
                },
                P1_EVENTTYPE_FD_WRITE,
            ),
            0
        );
    }

    /// A failed probe surfaces as `EPOLLERR` rather than silent readiness.
    #[test]
    fn epoll_mask_reports_probe_failures() {
        assert_eq!(
            wasix_epoll_mask_bit(Err(p1::errno::BADF), P1_EVENTTYPE_FD_READ),
            WASIX_EPOLL_TYPE_EPOLLERR
        );
    }
}
