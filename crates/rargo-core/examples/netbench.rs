//! Throughput of rargo's UDP transport between two processes (for example
//! across the network namespaces of `dev/netns.sh`):
//!   netbench serve ADDR
//!   netbench send ADDR MIB
use rargo_core::net::Endpoint;
use std::time::Instant;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let addr = args[2].parse().unwrap();
    match args[1].as_str() {
        "serve" => {
            let ep = Endpoint::bind(addr, true).await.unwrap();
            loop {
                let c = ep.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut total = 0usize;
                    while let Some(m) = c.recv().await {
                        if m == b"end" {
                            c.send(total.to_string().as_bytes()).await.unwrap();
                            break;
                        }
                        total += m.len();
                    }
                });
            }
        }
        "send" => {
            let mib: usize = args[3].parse().unwrap();
            let ep = Endpoint::bind("0.0.0.0:0".parse().unwrap(), false).await.unwrap();
            let c = ep.connect(addr);
            let chunk = vec![7u8; 1 << 20];
            let t = Instant::now();
            for _ in 0..mib {
                c.send(&chunk).await.unwrap();
            }
            c.send(b"end").await.unwrap();
            let got = c.recv().await.unwrap();
            let dt = t.elapsed().as_secs_f64();
            println!(
                "{} MiB in {:.2} s = {:.1} MiB/s; packets sent {}, retransmits {}, rtt {:?}",
                String::from_utf8_lossy(&got).parse::<usize>().unwrap() >> 20,
                dt,
                mib as f64 / dt,
                c.stats.sent_packets.load(std::sync::atomic::Ordering::Relaxed),
                c.stats.retransmits.load(std::sync::atomic::Ordering::Relaxed),
                c.rtt()
            );
        }
        _ => panic!("usage"),
    }
}
