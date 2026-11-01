(module
  (global $calls (export "calls") (mut i32) (i32.const 0))

  (func $square (param $x i32) (result i32)
    local.get $x
    local.get $x
    i32.mul)

  (func $sum_of_squares (export "sum_of_squares") (param $a i32) (param $b i32) (result i32)
    global.get $calls
    i32.const 1
    i32.add
    global.set $calls
    local.get $a
    call $square
    local.get $b
    call $square
    i32.add))
