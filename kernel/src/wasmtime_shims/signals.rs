use core::{mem, ptr};

use super::std::sync::OnceLock;
use libc::{SA_NODEFER, SA_SIGINFO, SIGBUS, SIGFPE, SIGILL, SIGSEGV, c_int, c_void, sigaction};

type TrapHandler = extern "C" fn(usize, usize, bool, usize);

static TRAP_HANDLER: OnceLock<TrapHandler> = OnceLock::new();

fn last_errno() -> i32 {
    super::std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EINVAL)
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_init_traps(handler: TrapHandler) -> i32 {
    match TRAP_HANDLER.set(handler) {
        Ok(()) => {}
        Err(previous) if (previous as usize) == handler as usize => {}
        Err(_) => panic!("Wasmtime trap handler was initialized twice with different callbacks"),
    }

    for signo in [SIGSEGV, SIGBUS, SIGILL, SIGFPE] {
        let rc = install_signal(signo);
        if rc != 0 {
            return rc;
        }
    }

    0
}

fn install_signal(signo: c_int) -> i32 {
    let mut action: sigaction = unsafe { mem::zeroed() };
    action.sa_sigaction = handle_signal as *const () as usize;
    action.sa_flags = SA_SIGINFO | SA_NODEFER;

    let rc = unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signo, &action, ptr::null_mut())
    };
    if rc == 0 {
        return 0;
    }

    last_errno()
}

extern "C" fn handle_signal(signo: c_int, info: *mut libc::siginfo_t, context: *mut c_void) {
    let handler = *TRAP_HANDLER
        .get()
        .expect("signal arrived before Wasmtime trap handler initialization");
    let (ip, fp) = unsafe { registers_from_context(context) };
    let (has_faulting_addr, faulting_addr) = match signo {
        SIGSEGV | SIGBUS => (true, unsafe { (*info).si_addr() as usize }),
        _ => (false, 0),
    };

    handler(ip, fp, has_faulting_addr, faulting_addr);

    unsafe {
        libc::signal(signo, libc::SIG_DFL);
    }
}

#[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
unsafe fn registers_from_context(context: *mut c_void) -> (usize, usize) {
    let context = unsafe { &*(context.cast::<libc::ucontext_t>()) };
    let mcontext = unsafe { &*context.uc_mcontext };
    (mcontext.__ss.__pc as usize, mcontext.__ss.__fp as usize)
}

#[cfg(all(target_vendor = "apple", target_arch = "x86_64"))]
unsafe fn registers_from_context(context: *mut c_void) -> (usize, usize) {
    let context = unsafe { &*(context.cast::<libc::ucontext_t>()) };
    let mcontext = unsafe { &*context.uc_mcontext };
    (mcontext.__ss.__rip as usize, mcontext.__ss.__rbp as usize)
}
