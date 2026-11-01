use std::env;

struct Stats {
    count: usize,
    total: u64,
}

fn factorial(n: u64) -> u64 {
    if n <= 1 {
        1
    } else {
        n * factorial(n - 1) // BP: factorial
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let name = env::var("GREETING_NAME").unwrap_or_else(|_| "world".to_string());
    println!("hello, {name}!"); // BP: hello
    let mut stats = Stats { count: 0, total: 0 };
    for i in 1..=5u64 {
        let f = factorial(i);
        stats.count += 1;
        stats.total += f; // BP: loop
    }
    println!("args={} count={} total={}", args.len(), stats.count, stats.total);
    if args.iter().any(|a| a == "--fail") {
        eprintln!("failing on request");
        std::process::exit(3);
    }
}
