(module
  (memory 1)
  (func (export "_start")
    i32.const 0
    i32.const 7
    i32.store
    i32.const 0
    i32.load
    drop))
