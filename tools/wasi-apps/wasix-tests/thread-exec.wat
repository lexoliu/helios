;; `proc_exec` on a spawned thread replaces the whole process, as POSIX
;; `execve` does from any thread: main parks forever in
;; `memory.atomic.wait32`, and once it is parked the thread execs
;; `/bin/dash -c 'echo thread-exec:replaced'`. Main and the thread end,
;; the replacement runs in the process, prints its line and exits 0. A
;; kernel that drops the exec ends only the calling thread and leaves
;; main parked, so a reader of the process's output waits forever.
(module
  (memory (import "env" "memory") 1 1 shared)
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (import "wasix_32v1" "thread_spawn_v2" (func $thread_spawn_v2 (param i32 i32) (result i32)))
  (import "wasix_32v1" "proc_exec" (func $proc_exec (param i32 i32 i32 i32)))
  ;; Stage markers, so a hang names the step it stopped in.
  (data (i32.const 1024) "thread-exec:spawned\n")
  (data (i32.const 1056) "thread-exec:parking\n")
  (data (i32.const 1088) "thread-exec:exec\n")
  ;; The replacement: its path, and its argv as newline-separated
  ;; entries.
  (data (i32.const 1152) "/bin/dash")
  (data (i32.const 1184) "dash\n-c\necho thread-exec:replaced")
  ;; 50 ms, in nanoseconds: long enough for main to park after it
  ;; announced itself.
  (global $settle i64 (i64.const 50000000))
  (func $fail
    i32.const 1
    call $proc_exit
    unreachable)
  ;; Writes `len` bytes at `ptr` to stdout through the iovec at `iov`;
  ;; main and the thread each have their own.
  (func $say (param $iov i32) (param $ptr i32) (param $len i32)
    local.get $iov
    local.get $ptr
    i32.store
    local.get $iov
    i32.const 4
    i32.add
    local.get $len
    i32.store
    i32.const 1
    local.get $iov
    i32.const 1
    local.get $iov
    i32.const 8
    i32.add
    call $fd_write
    i32.eqz
    if
    else
      call $fail
    end)
  (func $_start (export "_start")
    ;; 128: the word main parks on, never notified.
    ;; 140: set to 1 by main just before it parks.
    ;; 144: a word nobody notifies, for the thread's settle wait.
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
    i32.const 0
    i32.const 1024
    i32.const 20
    call $say
    i32.const 0
    i32.const 1056
    i32.const 20
    call $say
    i32.const 140
    i32.const 1
    i32.atomic.store
    i32.const 140
    i32.const 1
    memory.atomic.notify
    drop
    ;; Park forever. Any return is a failure: nothing notifies 128, and
    ;; the thread's exec must end this call rather than wake it.
    i32.const 128
    i32.const 0
    i64.const -1
    memory.atomic.wait32
    drop
    call $fail)
  (func (export "wasi_thread_start") (param $tid i32) (param $arg i32)
    ;; Wait until main has announced it parks: 0 (woken) or 1 (it was
    ;; already there) both mean it has.
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
    ;; Let main reach its park.
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
    i32.const 32
    i32.const 1088
    i32.const 17
    call $say
    i32.const 1152
    i32.const 9
    i32.const 1184
    i32.const 33
    call $proc_exec
    ;; An exec that returns failed to replace the process.
    call $fail))
