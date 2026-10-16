;; A process whose main returns while its threads still run must end:
;; one thread is parked forever in `memory.atomic.wait32` and one spins
;; in pure wasm, and neither is joined. The process completes with
;; main's status (0) and its output ends. A kernel that leaves the
;; threads running keeps their clones of the process's stdout open, so
;; a reader of that output waits forever.
(module
  (memory (import "env" "memory") 1 1 shared)
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (import "wasix_32v1" "thread_spawn_v2" (func $thread_spawn_v2 (param i32 i32) (result i32)))
  (data (i32.const 1024) "orphan-threads:ok\n")
  ;; Stage markers, so a hang names the step it stopped in.
  (data (i32.const 1056) "orphan-threads:spawned\n")
  (data (i32.const 1088) "orphan-threads:parked\n")
  (data (i32.const 1120) "orphan-threads:spinning\n")
  ;; 50 ms, in nanoseconds: long enough for the parker to park after it
  ;; announced itself.
  (global $settle i64 (i64.const 50000000))
  (func $fail
    i32.const 1
    call $proc_exit
    unreachable)
  ;; Writes `len` bytes at `ptr` to stdout through the iovec at 0.
  (func $say (param $ptr i32) (param $len i32)
    i32.const 0
    local.get $ptr
    i32.store
    i32.const 4
    local.get $len
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
  ;; Waits until the thread that announces itself at `addr` has stored 1
  ;; there: 0 (woken) or 1 (it was already there) both mean it has.
  (func $await_announcement (param $addr i32)
    local.get $addr
    i32.const 0
    i64.const -1
    memory.atomic.wait32
    i32.const 2
    i32.eq
    if
      call $fail
    end
    local.get $addr
    i32.atomic.load
    i32.const 1
    i32.eq
    if
    else
      call $fail
    end)
  (func $_start (export "_start")
    ;; 128: the word the parker parks on, never notified.
    ;; 140: set to 1 by the parker just before it parks.
    ;; 144: a word nobody notifies, for the settle wait.
    ;; 148: set to 1 by the spinner just before it spins.
    i32.const 128
    i32.const 0
    i32.atomic.store
    i32.const 140
    i32.const 0
    i32.atomic.store
    i32.const 144
    i32.const 0
    i32.atomic.store
    i32.const 148
    i32.const 0
    i32.atomic.store
    ;; The thread argument names the role: 256 parks, 320 spins.
    i32.const 256
    i32.const 16
    call $thread_spawn_v2
    i32.eqz
    if
    else
      call $fail
    end
    i32.const 320
    i32.const 20
    call $thread_spawn_v2
    i32.eqz
    if
    else
      call $fail
    end
    i32.const 1056
    i32.const 23
    call $say
    i32.const 140
    call $await_announcement
    ;; Let the parker reach its park.
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
    i32.const 1088
    i32.const 22
    call $say
    i32.const 148
    call $await_announcement
    i32.const 1120
    i32.const 24
    call $say
    ;; Return with both threads still live and unjoined.
    i32.const 1024
    i32.const 18
    call $say)
  (func (export "wasi_thread_start") (param $tid i32) (param $arg i32)
    local.get $arg
    i32.const 256
    i32.eq
    if
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
      call $fail
    else
      i32.const 148
      i32.const 1
      i32.atomic.store
      i32.const 148
      i32.const 1
      memory.atomic.notify
      drop
      ;; Spin forever in pure wasm: only an interrupt check ends it.
      (loop $spin
        br $spin)
    end))
