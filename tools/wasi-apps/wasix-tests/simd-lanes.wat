(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))

  (memory (export "memory") 1)

  (data (i32.const 16) "simd-lanes:00\n")

  (func $_start (export "_start")
    (local $value i32)

    i32.const 0
    i32.const 16
    i32.store

    i32.const 4
    i32.const 14
    i32.store

    (local.set $value
      (i32x4.extract_lane 0
        (i32x4.add
          (v128.const i32x4 10 20 30 40)
          (v128.const i32x4 7 0 0 0))))

    i32.const 27
    local.get $value
    i32.const 10
    i32.div_u
    i32.const 48
    i32.add
    i32.store8

    i32.const 28
    local.get $value
    i32.const 10
    i32.rem_u
    i32.const 48
    i32.add
    i32.store8

    i32.const 1
    i32.const 0
    i32.const 1
    i32.const 8
    call $fd_write
    drop)
)
