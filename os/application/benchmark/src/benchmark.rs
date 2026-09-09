#![no_std]

extern crate alloc;

use alloc::{string::String, vec::Vec};
use concurrent::thread;


use runtime::env::args;
#[allow(unused_imports)]
use runtime::*;
use terminal::println;

fn print_usage() {
    println!("usage: benchmark [module] [parameters]");
    println!("       benchmark help");
    println!("       benchmark spawn [threads_per_round=10] [rounds=10]");
    println!("       [parameter=default]");
}

fn busy_spin(iterations: u64) {
    let mut x: u64 = 0;

    for i in 0..iterations {
        x = x.wrapping_add(i ^ (x <<1));
    }
    core::hint::black_box(x);
}

fn thread_handle_spawn() {
    busy_spin(2000);
}

fn spawn(threads_per_round: usize, rounds: usize) {
    println!("Spawning {} threads per round for {} rounds", threads_per_round, rounds);

    let start = time::systime().num_milliseconds();

    let mut total = 0;
    
    // Perform "rounds" rounds of spawning "threads_per_round" threads
    for _ in 0..rounds {
        let mut handles = Vec::with_capacity(threads_per_round);

        for _ in 0..threads_per_round {
            if let Some(thread) = thread::create(thread_handle_spawn) {
                handles.push(thread);
            }
        }

        total += handles.len();

        for thread in &handles {
            let _ = thread.join();
        }
    }

    let elapsed = (time::systime().num_milliseconds() - start).max(1);
    let rate = (total as i64 * 1000) / elapsed;

    println!("Spawned {} threads in {} ms ({} threads/sec)", total, elapsed, rate);

}

fn argument_at_index(argv: &[String], index: usize, default: usize) -> usize {
    argv.get(index).and_then(|s| s.parse().ok()).unwrap_or(default)
}

#[unsafe(no_mangle)]
pub fn main() {
    let argv: Vec<String> = args().collect();
    let module = argv.get(1).map(|s| s.as_str()).unwrap_or("help");

    match module {
        "help" => print_usage(),
        "spawn" => {
            let threads_per_round = argument_at_index(&argv, 2, 10);
            let rounds = argument_at_index(&argv, 3, 10);
            spawn(threads_per_round, rounds);
        }
        other => {
            println!("Unknown module: {}", other);
            print_usage();
        }
    }
}
