(module
  (memory (import "env" "memory") 1 1 shared)
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (import "wasix_32v1" "thread_spawn_v2" (func $thread_spawn_v2 (param i32 i32) (result i32)))
  (import "wasix_32v1" "thread_join" (func $thread_join (param i32) (result i32)))
  (import "wasix_32v1" "futex_wait" (func $futex_wait (param i32 i32 i32 i32) (result i32)))
  (import "wasix_32v1" "futex_wake" (func $futex_wake (param i32 i32) (result i32)))
  (import "wasix_32v1" "thread_exit" (func $thread_exit (param i32)))
  (data (i32.const 1024) "thread-futex:ok\n")
  (func $fail
    i32.const 1
    call $proc_exit
    unreachable)
  (func $_start (export "_start")
    i32.const 0
    i32.const 1024
    i32.store
    i32.const 4
    i32.const 16
    i32.store
    i32.const 128
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
    i32.const 128
    i32.const 0
    i32.const 0
    i32.const 132
    call $futex_wait
    i32.eqz
    if
    else
      call $fail
    end
    i32.const 132
    i32.load8_u
    i32.const 1
    i32.eq
    if
    else
      call $fail
    end
    i32.const 128
    i32.atomic.load
    i32.const 1
    i32.eq
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
    i32.const 128
    i32.const 1
    i32.atomic.store
    i32.const 128
    i32.const 136
    call $futex_wake
    i32.eqz
    if
    else
      call $fail
    end
    i32.const 136
    i32.load8_u
    i32.const 1
    i32.eq
    if
    else
      call $fail
    end
    i32.const 0
    call $thread_exit
    unreachable))
