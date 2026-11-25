//! How much of one file the other already holds, in rargo's chunks: the
//! bytes of NEW in chunks that OLD also has, and the bytes left to send.
//!
//!   cargo run --example chunkshare -- OLD NEW
use rargo_core::chunk::chunk;
use std::collections::HashSet;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, old, new] = &args[..] else {
        eprintln!("usage: chunkshare OLD NEW");
        std::process::exit(2);
    };
    let read = |p: &str| std::fs::read(p).unwrap_or_else(|e| panic!("{p}: {e}"));
    let (a, b) = (chunk(&read(old)), chunk(&read(new)));
    let have: HashSet<_> = a.iter().map(|c| c.hash).collect();
    let total: u64 = b.iter().map(|c| c.len as u64).sum();
    let shared: u64 = b.iter().filter(|c| have.contains(&c.hash)).map(|c| c.len as u64).sum();
    println!(
        "{shared} of {total} bytes shared ({:.1}%), {} of {} chunks; {} bytes to send",
        100.0 * shared as f64 / total.max(1) as f64,
        b.iter().filter(|c| have.contains(&c.hash)).count(),
        b.len(),
        total - shared
    );
}
