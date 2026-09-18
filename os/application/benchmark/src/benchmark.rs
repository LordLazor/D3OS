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
    println!("       benchmark sleep [threads_per_round=10] [rounds=10] [milliseconds=50]");
    println!("       benchmark join [workers_per_round=5] [joiners_per_worker=4] [rounds=10]");
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

fn thread_handle_sleep(ms: usize) {
    thread::sleep(ms);
}

fn sleep(threads_per_round: usize, rounds: usize, ms: usize) {
    println!("Sleep-list stress test: {} threads/round, each sleeping {} ms, for {} rounds", threads_per_round, ms, rounds);

    let start = time::systime().num_milliseconds();
    let mut total = 0;

    for round in 0..rounds {
        let mut handles = Vec::with_capacity(threads_per_round);

        for _ in 0..threads_per_round {
            if let Some(thread) = thread::create(move || thread_handle_sleep(ms)) {
                handles.push(thread);
            }
        }

        total += handles.len();

        for thread in &handles {
            let _ = thread.join();
        }

        println!("Round {}/{} done ({} threads slept {} ms and exited)", round + 1, rounds, handles.len(), ms);
    }

    let elapsed = (time::systime().num_milliseconds() - start).max(1);
    println!("{} threads slept {} ms each and exited cleanly in {} ms total", total, ms, elapsed);
}

fn thread_handle_join_worker() {
    busy_spin(2000);
}

fn thread_handle_joiner(worker: thread::Thread) {
    let _ = worker.join();
}

fn join(workers_per_round: usize, joiners_per_worker: usize, rounds: usize) {
    println!("Join-map stress test: {} workers/round, {} concurrent joiners per worker, for {} rounds", workers_per_round, joiners_per_worker, rounds);

    let start = time::systime().num_milliseconds();
    let mut total_workers = 0;
    let mut total_joins = 0;

    for round in 0..rounds {
        let mut workers = Vec::with_capacity(workers_per_round);
        for _ in 0..workers_per_round {
            if let Some(worker) = thread::create(thread_handle_join_worker) {
                workers.push(worker);
            }
        }
        total_workers += workers.len();

        let mut joiners = Vec::with_capacity(workers.len() * joiners_per_worker);
        for worker in &workers {
            let w = *worker;
            for _ in 0..joiners_per_worker {
                if let Some(joiner) = thread::create(move || thread_handle_joiner(w)) {
                    joiners.push(joiner);
                }
            }
        }
        total_joins += joiners.len();

        for joiner in &joiners {
            let _ = joiner.join();
        }

        println!("Round {}/{} done ({} workers, {} joiners each)", round + 1, rounds, workers.len(), joiners_per_worker);
    }

    let elapsed = (time::systime().num_milliseconds() - start).max(1);
    let rate = (total_joins as i64 * 1000) / elapsed;
    println!("{} workers, {} total join() calls completed in {} ms ({} joins/sec)", total_workers, total_joins, elapsed, rate);
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
        "sleep" => {
            let threads_per_round = argument_at_index(&argv, 2, 10);
            let rounds = argument_at_index(&argv, 3, 10);
            let ms = argument_at_index(&argv, 4, 50);
            sleep(threads_per_round, rounds, ms);
        }
        "join" => {
            let workers_per_round = argument_at_index(&argv, 2, 5);
            let joiners_per_worker = argument_at_index(&argv, 3, 4);
            let rounds = argument_at_index(&argv, 4, 10);
            join(workers_per_round, joiners_per_worker, rounds);
        }
        other => {
            println!("Unknown module: {}", other);
            print_usage();
        }
    }
}
