use rargo_net::io;
use rargo_net::{Auth, Endpoint};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    io::drive(4, move || {
        let psk = [9u8; 32];
        match args[1].as_str() {
            "serve" => {
                let users: rargo_net::UserKeys = std::sync::Arc::new(move |_| Some(psk));
                let ep = Endpoint::bind_auth(args[2].parse().unwrap(), true, Auth::Users(users)).unwrap();
                while let Some(c) = ep.accept() {
                    let c2 = c.clone();
                    io::spawn(move || {
                        loop {
                            io::sleep(std::time::Duration::from_millis(500));
                            if c2.is_closed() {
                                break;
                            }
                            eprintln!("  {}", c2.sender_state());
                        }
                    });
                    io::spawn(move || {
                        while let Some(m) = c.recv() {
                            if m.len() == 8 {
                                let n = u64::from_le_bytes(m[..].try_into().unwrap());
                                let part = vec![0x5au8; 1 << 20];
                                for _ in 0..n {
                                    if c.send(&part).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    });
                }
            }
            _ => {
                let ep = Endpoint::bind_auth("0.0.0.0:0".parse().unwrap(), false, Auth::User { name: "perf".into(), psk }).unwrap();
                let t = Instant::now();
                let c = ep.connect(args[2].parse().unwrap()).unwrap();
                let connected = t.elapsed();
                let n: u64 = args[3].parse().unwrap();
                let t = Instant::now();
                c.send(&n.to_le_bytes()).unwrap();
                let mut got = 0u64;
                let mut first = None;
                while got < n << 20 {
                    got += c.recv().unwrap().len() as u64;
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
