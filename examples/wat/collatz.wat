;; Collatz sequence explorer.
;;
;; `steps` counts how many iterations a number takes to reach 1 and records
;; the sequence in linear memory; `longest` searches for the starting number
;; below a limit with the longest sequence.

(module
  (memory (export "memory") 1)
  (global $max_seen (export "max_seen") (mut i32) (i32.const 0))

  (func $next (param $n i32) (result i32)
    local.get $n
    i32.const 1
    i32.and
    if (result i32)
      local.get $n
      i32.const 3
      i32.mul
      i32.const 1
      i32.add
    else
      local.get $n
      i32.const 1
      i32.shr_u
    end)

  (func $steps (export "steps") (param $n i32) (result i32)
    (local $count i32)
    block $done
      loop $again
        local.get $count
        i32.const 4
        i32.mul
        local.get $n
        i32.store
        local.get $n
        global.get $max_seen
        i32.gt_u
        if
          local.get $n
          global.set $max_seen
        end
        local.get $n
        i32.const 1
        i32.le_u
        br_if $done
        local.get $n
        call $next
        local.set $n
        local.get $count
        i32.const 1
        i32.add
        local.set $count
        br $again
      end
    end
    local.get $count)

  (func $longest (export "longest") (param $limit i32) (result i32)
    (local $i i32) (local $best i32) (local $best_steps i32) (local $s i32)
    i32.const 1
    local.set $i
    block $done
      loop $search
        local.get $i
        local.get $limit
        i32.ge_u
        br_if $done
        local.get $i
        call $steps
        local.tee $s
        local.get $best_steps
        i32.gt_u
        if
          local.get $s
          local.set $best_steps
          local.get $i
          local.set $best
        end
        local.get $i
        i32.const 1
        i32.add
        local.set $i
        br $search
      end
    end
    local.get $best))
