;; A stand-in for the compiler plugin that implements its ABI and nothing
;; else, so the host's half of the contract can be tested without building
;; Cranelift to wasm. It lives only in these tests.
;;
;; Memory layout: 0 the memory-init flag, 4 the bump heap's next address,
;; 8 the thread pointer `helios_compiler_initialize` was given, 12 the flag
;; the spawned thread raises, 16 the bytes outstanding across the interface
;; (`helios_compiler_alloc` adds, `helios_compiler_free` subtracts),
;; 32/36 the environment's count and size, 40 its pointer array; the heap
;; starts at 4096.
;;
;; `helios_compiler_compile` answers a well-formed request by spawning one
;; thread, waiting for it, and returning
;; `[hint: u32][environ bytes][target][isa level][input]` as the
;; "precompiled" output,
;; so a test can read the whole request back. An input whose first byte is
;; 1 makes the thread trap; 2 answers ABI version 99 (its response header
;; uncounted, as a plugin of another layout would keep it); 3 answers
;; status 77 and 4 output kind 55, each with a counted output buffer, so
;; the host must still free it; 5 answers normally but leaves 7 bytes
;; counted as outstanding; an empty input is rejected as `CompileFailed`
;; with the diagnostic `empty`.
(module
  (import "env" "memory" (memory 2 16 shared))
  (import "wasi" "thread-spawn" (func $spawn (param i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_sizes_get"
    (func $environ_sizes_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_get"
    (func $environ_get (param i32 i32) (result i32)))

  (global $tls_base (export "__tls_base") (mut i32) (i32.const 0))

  ;; Only the instance that wins the memory-init flag publishes the TLS
  ;; block, as wasi-libc's start does.
  (func $init
    (if (i32.eqz (i32.atomic.rmw.cmpxchg (i32.const 0) (i32.const 0) (i32.const 1)))
      (then
        (i32.store (i32.const 4) (i32.const 4096))
        (global.set $tls_base (i32.const 1024)))))
  (start $init)

  (func (export "helios_compiler_pthread_self_offset") (result i32)
    (i32.const 16))

  (func (export "helios_compiler_initialize") (param $thread_pointer i32)
    (i32.store (i32.const 8) (local.get $thread_pointer)))

  (func $count (param $delta i32)
    (i32.store (i32.const 16) (i32.add (i32.load (i32.const 16)) (local.get $delta))))

  (func $alloc (export "helios_compiler_alloc") (param $len i32) (param $align i32) (result i32)
    (local $ptr i32)
    (local.set $ptr
      (i32.and
        (i32.add (i32.load (i32.const 4)) (i32.sub (local.get $align) (i32.const 1)))
        (i32.sub (i32.const 0) (local.get $align))))
    (i32.store (i32.const 4) (i32.add (local.get $ptr) (local.get $len)))
    (call $count (local.get $len))
    (local.get $ptr))

  ;; The ABI never frees a null pointer or a zero-length buffer, and every
  ;; layout has a non-zero alignment.
  (func (export "helios_compiler_free") (param $ptr i32) (param $len i32) (param $align i32)
    (if (i32.or
          (i32.or (i32.eqz (local.get $ptr)) (i32.eqz (local.get $len)))
          (i32.eqz (local.get $align)))
      (then unreachable))
    (call $count (i32.sub (i32.const 0) (local.get $len))))

  (func (export "helios_compiler_outstanding_bytes") (result i32)
    (i32.load (i32.const 16)))

  (func (export "wasi_thread_start") (param $thread_id i32) (param $arg i32)
    (if (i32.eq (local.get $arg) (i32.const 1))
      (then unreachable))
    (i32.atomic.store (i32.const 12) (i32.const 1))
    (drop (memory.atomic.notify (i32.const 12) (i32.const 1))))

  (func $respond
    (param $abi i32) (param $status i32) (param $kind i32)
    (param $output i32) (param $output_len i32)
    (param $diagnostic i32) (param $diagnostic_len i32)
    (result i32)
    (local $response i32)
    (local.set $response (call $alloc (i32.const 28) (i32.const 4)))
    (i32.store offset=0 (local.get $response) (local.get $abi))
    (i32.store offset=4 (local.get $response) (local.get $status))
    (i32.store offset=8 (local.get $response) (local.get $kind))
    (i32.store offset=12 (local.get $response) (local.get $output))
    (i32.store offset=16 (local.get $response) (local.get $output_len))
    (i32.store offset=20 (local.get $response) (local.get $diagnostic))
    (i32.store offset=24 (local.get $response) (local.get $diagnostic_len))
    (local.get $response))

  (func (export "helios_compiler_compile") (param $request i32) (param $request_len i32) (result i32)
    (local $hint i32)
    (local $wasm i32)
    (local $wasm_len i32)
    (local $target i32)
    (local $target_len i32)
    (local $isa i32)
    (local $isa_len i32)
    (local $first i32)
    (local $environ_len i32)
    (local $output i32)
    (local $output_len i32)
    (local $diagnostic i32)
    (local $response i32)
    ;; The host adopted the TLS block and initialised the thread pointer
    ;; from it, and sent a version-3 header of the declared length.
    (if (i32.or
          (i32.ne (i32.load (i32.const 8)) (i32.add (global.get $tls_base) (i32.const 16)))
          (i32.or
            (i32.ne (local.get $request_len) (i32.const 36))
            (i32.ne (i32.load (local.get $request)) (i32.const 3))))
      (then
        (return (call $respond (i32.const 3) (i32.const 1) (i32.const 0)
          (i32.const 0) (i32.const 0) (i32.const 0) (i32.const 0)))))
    (local.set $hint (i32.load offset=4 (local.get $request)))
    (local.set $wasm (i32.load offset=8 (local.get $request)))
    (local.set $wasm_len (i32.load offset=12 (local.get $request)))
    (local.set $target (i32.load offset=16 (local.get $request)))
    (local.set $target_len (i32.load offset=20 (local.get $request)))
    (local.set $isa (i32.load offset=24 (local.get $request)))
    (local.set $isa_len (i32.load offset=28 (local.get $request)))
    (if (i32.eqz (local.get $wasm_len))
      (then
        (local.set $diagnostic (call $alloc (i32.const 5) (i32.const 1)))
        ;; "empt" little-endian, then "y".
        (i32.store (local.get $diagnostic) (i32.const 0x74706d65))
        (i32.store8 offset=4 (local.get $diagnostic) (i32.const 0x79))
        (return (call $respond (i32.const 3) (i32.const 3) (i32.const 0)
          (i32.const 0) (i32.const 0) (local.get $diagnostic) (i32.const 5)))))
    (local.set $first (i32.load8_u (local.get $wasm)))
    (if (i32.eq (local.get $first) (i32.const 2))
      (then
        (local.set $response
          (call $respond (i32.const 99) (i32.const 0) (i32.const 0)
            (i32.const 0) (i32.const 0) (i32.const 0) (i32.const 0)))
        (call $count (i32.const -28))
        (return (local.get $response))))
    (if (i32.eq (local.get $first) (i32.const 3))
      (then
        (return (call $respond (i32.const 3) (i32.const 77) (i32.const 0)
          (call $alloc (i32.const 3) (i32.const 1)) (i32.const 3)
          (i32.const 0) (i32.const 0)))))
    (if (i32.eq (local.get $first) (i32.const 4))
      (then
        (return (call $respond (i32.const 3) (i32.const 0) (i32.const 55)
          (call $alloc (i32.const 3) (i32.const 1)) (i32.const 3)
          (i32.const 0) (i32.const 0)))))
    (i32.atomic.store (i32.const 12) (i32.const 0))
    (if (i32.lt_s (call $spawn (i32.eq (local.get $first) (i32.const 1))) (i32.const 0))
      (then
        (return (call $respond (i32.const 3) (i32.const 3) (i32.const 0)
          (i32.const 0) (i32.const 0) (i32.const 0) (i32.const 0)))))
    ;; A thread told to trap never raises the flag, so only a healthy
    ;; one is waited for.
    (if (i32.ne (local.get $first) (i32.const 1))
      (then
        (block $raised
          (loop $wait
            (br_if $raised (i32.eq (i32.atomic.load (i32.const 12)) (i32.const 1)))
            (drop (memory.atomic.wait32 (i32.const 12) (i32.const 0) (i64.const -1)))
            (br $wait)))))
    (drop (call $environ_sizes_get (i32.const 32) (i32.const 36)))
    (local.set $environ_len (i32.load (i32.const 36)))
    (local.set $output_len
      (i32.add
        (i32.add (i32.const 4) (local.get $environ_len))
        (i32.add
          (i32.add (local.get $target_len) (local.get $isa_len))
          (local.get $wasm_len))))
    (local.set $output (call $alloc (local.get $output_len) (i32.const 4)))
    (i32.store (local.get $output) (local.get $hint))
    (drop (call $environ_get (i32.const 40) (i32.add (local.get $output) (i32.const 4))))
    (memory.copy
      (i32.add (i32.add (local.get $output) (i32.const 4)) (local.get $environ_len))
      (local.get $target)
      (local.get $target_len))
    (memory.copy
      (i32.add
        (i32.add (i32.add (local.get $output) (i32.const 4)) (local.get $environ_len))
        (local.get $target_len))
      (local.get $isa)
      (local.get $isa_len))
    (memory.copy
      (i32.add
        (i32.add
          (i32.add (i32.add (local.get $output) (i32.const 4)) (local.get $environ_len))
          (local.get $target_len))
        (local.get $isa_len))
      (local.get $wasm)
      (local.get $wasm_len))
    (local.set $response
      (call $respond (i32.const 3) (i32.const 0) (i32.const 0)
        (local.get $output) (local.get $output_len) (i32.const 0) (i32.const 0)))
    (if (i32.eq (local.get $first) (i32.const 5))
      (then (call $count (i32.const 7))))
    (local.get $response)))
