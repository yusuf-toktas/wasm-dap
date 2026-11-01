#![no_std]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

#[derive(Clone, Copy)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Shape {
    Circle(u32),
    Rect { w: u32, h: u32 },
    Empty,
}

static mut COUNTER: u32 = 0;
static GREETING: &str = "hello wasm";

#[unsafe(no_mangle)]
pub extern "C" fn fib(n: u32) -> u32 {
    if n < 2 {
        return n; // BP: fib-base
    }
    let a = fib(n - 1);
    let b = fib(n - 2); // BP: fib-b
    a + b // BP: fib-sum
}

#[unsafe(no_mangle)]
pub extern "C" fn sum_points(count: i32) -> i32 {
    let points = [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }, Point { x: 5, y: 6 }];
    let mut total = 0;
    let mut i = 0;
    while i < count as usize && i < points.len() {
        let p = points[i];
        total += p.x * p.y; // BP: sum-loop
        i += 1;
    }
    total // BP: sum-end
}

fn area(shape: Shape) -> u32 {
    match shape {
        Shape::Circle(r) => 3 * r * r,
        Shape::Rect { w, h } => w * h, // BP: area-rect
        Shape::Empty => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn shapes() -> u32 {
    let circle = Shape::Circle(2);
    let rect = Shape::Rect { w: 3, h: 4 };
    let empty = Shape::Empty;
    let flag = true;
    let ratio = 0.5f64;
    let letter = 'w';
    let text = GREETING;
    let maybe: Option<u8> = Some(7);
    let total = area(circle) + area(rect) + area(empty); // BP: shapes-total
    if flag && ratio > 0.0 && letter == 'w' && !text.is_empty() && maybe.is_some() {
        total
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn bump() -> u32 {
    unsafe {
        COUNTER += 1; // BP: bump
        COUNTER
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn divide(a: i32, b: i32) -> i32 {
    a / b // BP: divide
}

#[unsafe(no_mangle)]
pub extern "C" fn spin(limit: u32) -> u32 {
    let mut i: u32 = 0;
    loop {
        i = i.wrapping_add(1); // BP: spin
        if limit != 0 && i == limit {
            break;
        }
    }
    i
}
