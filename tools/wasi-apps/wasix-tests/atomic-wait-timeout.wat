;; A timed `memory.atomic.wait32` with no notifier must return 2
;; (timed out) once its timeout has elapsed, and not before.
(module
  (memory (import "env" "memory") 1 1 shared)
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "clock_time_get" (func $clock_time_get (param i32 i64 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (data (i32.const 1024) "atomic-wait-timeout:ok\n")
  ;; 20 ms, in nanoseconds.
  (global $timeout i64 (i64.const 20000000))
  (func $fail
    i32.const 1
    call $proc_exit
    unreachable)
  ;; Reads the monotonic clock into `$out`.
  (func $monotonic (param $out i32)
    i32.const 1
    i64.const 1
    local.get $out
    call $clock_time_get
    i32.eqz
    if
    else
      call $fail
    end)
  (func $_start (export "_start")
    i32.const 128
    i32.const 0
    i32.atomic.store
    i32.const 200
    call $monotonic
    i32.const 128
    i32.const 0
    global.get $timeout
    memory.atomic.wait32
    i32.const 2
    i32.eq
    if
    else
      call $fail
    end
    i32.const 208
    call $monotonic
    i32.const 208
    i64.load
    i32.const 200
    i64.load
    i64.sub
    global.get $timeout
    i64.ge_u
    if
    else
      call $fail
    end
    i32.const 0
    i32.const 1024
    i32.store
    i32.const 4
    i32.const 23
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
    end))
