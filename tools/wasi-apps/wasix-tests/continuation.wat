(module
  (memory (import "env" "memory") 1 1 shared)
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (import "wasix_32v1" "stack_checkpoint" (func $stack_checkpoint (param i32 i32) (result i32)))
  (import "wasix_32v1" "stack_restore" (func $stack_restore (param i32 i64)))
  (global $__data_end (export "__data_end") i32 (i32.const 2048))
  (global $__heap_base (export "__heap_base") i32 (i32.const 4096))
  (global $__stack_pointer (export "__stack_pointer") (mut i32) (i32.const 3072))
  (global $phase (mut i32) (i32.const 0))
  (data (i32.const 1024) "continuation:ok\n")
  (func $fail
    i32.const 1
    call $proc_exit
    unreachable)
  (func (export "asyncify_start_unwind") (param $data i32)
    global.get $phase
    i32.const 3
    i32.eq
    if
      i32.const 4
      global.set $phase
    else
      i32.const 1
      global.set $phase
    end)
  (func (export "asyncify_stop_unwind"))
  (func (export "asyncify_start_rewind") (param $data i32)
    i32.const 2
    global.set $phase)
  (func (export "asyncify_stop_rewind"))
  (func $_start (export "_start")
    global.get $phase
    i32.eqz
    if
      i32.const 512
      i32.const 536
      call $stack_checkpoint
      i32.eqz
      if
      else
        call $fail
      end
      global.get $phase
      i32.const 1
      i32.eq
      if
        return
      else
        call $fail
      end
    end
    global.get $phase
    i32.const 2
    i32.eq
    if
      i32.const 512
      i32.const 536
      call $stack_checkpoint
      i32.eqz
      if
      else
        call $fail
      end
      i32.const 536
      i64.load
      i64.const 0
      i64.eq
      if
        i32.const 3
        global.set $phase
        i32.const 512
        i64.const 7
        call $stack_restore
        global.get $phase
        i32.const 4
        i32.eq
        if
          return
        else
          call $fail
        end
      end
      i32.const 536
      i64.load
      i64.const 7
      i64.eq
      if
        i32.const 5
        global.set $phase
        i32.const 0
        i32.const 1024
        i32.store
        i32.const 4
        i32.const 16
        i32.store
        i32.const 1
        i32.const 0
        i32.const 1
        i32.const 8
        call $fd_write
        i32.eqz
        if
          return
        else
          call $fail
        end
      else
        call $fail
      end
    end
    global.get $phase
    i32.const 5
    i32.eq
    if
      return
    end
    call $fail))
