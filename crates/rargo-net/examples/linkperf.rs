//! Measures the transport over a real (or shaped) link.
//!   linkperf serve ADDR            echoes nothing, counts what arrives
//!   linkperf send ADDR MIB         sends MIB mebibytes, then asks for them back
use rargo_net::{Auth, Endpoint};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    rt.block_on(async move {
        let psk = [9u8; 32];
        match args[1].as_str() {
            "serve" => {
                let users: rargo_net::UserKeys = std::sync::Arc::new(move |_| Some(psk));
                let ep = Endpoint::bind_auth(args[2].parse().unwrap(), true, Auth::Users(users)).await.unwrap();
                while let Some(c) = ep.accept().await {
                    let c2 = c.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            if c2.is_closed() {
                                break;
                            }
                            eprintln!("  {}", c2.sender_state());
                        }
                    });
                    tokio::spawn(async move {
                        // Whatever arrives is counted; a message of 8 bytes asks for that many mebibytes back.
                        while let Some(m) = c.recv().await {
                            if m.len() == 8 {
                                let n = u64::from_le_bytes(m[..].try_into().unwrap());
                                let part = vec![0x5au8; 1 << 20];
                                for _ in 0..n {
                                    if c.send(&part).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    });
                }
            }
            _ => {
                let ep = Endpoint::bind_auth("0.0.0.0:0".parse().unwrap(), false, Auth::User { name: "perf".into(), psk }).await.unwrap();
                let t = Instant::now();
                let c = ep.connect(args[2].parse().unwrap()).await.unwrap();
                let connected = t.elapsed();
                let n: u64 = args[3].parse().unwrap();
                let t = Instant::now();
                c.send(&n.to_le_bytes()).await.unwrap();
                let mut got = 0u64;
                let mut first = None;
                while got < n << 20 {
                    got += c.recv().await.unwrap().len() as u64;
                    first.get_or_insert_with(|| t.elapsed());
                }
                let down = t.elapsed().as_secs_f64();
                println!(
                    "connect {:.0} ms; {n} MiB down in {down:.2} s = {:.1} MiB/s (first MiB after {:.0} ms); rtt {:?}",
                    connected.as_secs_f64() * 1e3,
                    n as f64 / down,
                    first.unwrap().as_secs_f64() * 1e3,
                    c.rtt()
                );
            }
        }
    });
}
