;; A thread parked forever in `memory.atomic.wait32` must end when it is
;; signalled: the wait is cancelled, the thread unwinds, and joining it
;; returns. A wait with no cancellation exit hangs the join, or panics a
;; kernel that has none.
(module
  (memory (import "env" "memory") 1 1 shared)
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (import "wasix_32v1" "thread_spawn_v2" (func $thread_spawn_v2 (param i32 i32) (result i32)))
  (import "wasix_32v1" "thread_join" (func $thread_join (param i32) (result i32)))
  (import "wasix_32v1" "thread_signal" (func $thread_signal (param i32 i32) (result i32)))
  (data (i32.const 1024) "atomic-wait-kill:ok\n")
  ;; 50 ms, in nanoseconds: long enough for the thread to park after it
  ;; announced itself.
  (global $settle i64 (i64.const 50000000))
  (func $fail
    i32.const 1
    call $proc_exit
    unreachable)
  (func $_start (export "_start")
    ;; 128: the word the thread parks on, never notified.
    ;; 140: set to 1 by the thread just before it parks.
    ;; 144: a word nobody notifies, for the settle wait.
    i32.const 128
    i32.const 0
    i32.atomic.store
    i32.const 140
    i32.const 0
    i32.atomic.store
    i32.const 144
    i32.const 0
    i32.atomic.store
    i32.const 256
    i32.const 16
    call $thread_spawn_v2
    i32.eqz
    if
    else
      call $fail
    end
    ;; Wait for the thread's announcement: 0 (woken) or 1 (it was
    ;; already there) both mean it is on its way to park.
    i32.const 140
    i32.const 0
    i64.const -1
    memory.atomic.wait32
    i32.const 2
    i32.eq
    if
      call $fail
    end
    i32.const 140
    i32.atomic.load
    i32.const 1
    i32.eq
    if
    else
      call $fail
    end
    ;; Let the thread reach its park.
    i32.const 144
    i32.const 0
    global.get $settle
    memory.atomic.wait32
    i32.const 2
    i32.eq
    if
    else
      call $fail
    end
    ;; SIGKILL the parked thread, then join it: the join returns only once
    ;; the thread's wait was cancelled and its fiber unwound.
    i32.const 16
    i32.load
    i32.const 9
    call $thread_signal
    i32.eqz
    if
    else
      call $fail
    end
    i32.const 16
    i32.load
    call $thread_join
    i32.eqz
    if
    else
      call $fail
    end
    i32.const 0
    i32.const 1024
    i32.store
    i32.const 4
    i32.const 20
    i32.store
    i32.const 1
    i32.const 0
    i32.const 1
    i32.const 8
    call $fd_write
    i32.eqz
    if
    else
      call $fail
    end)
  (func (export "wasi_thread_start") (param $tid i32) (param $arg i32)
    i32.const 140
    i32.const 1
    i32.atomic.store
    i32.const 140
    i32.const 1
    memory.atomic.notify
    drop
    ;; Park forever. Any return is a failure: nothing notifies 128.
    i32.const 128
    i32.const 0
    i64.const -1
    memory.atomic.wait32
    drop
    call $fail))
