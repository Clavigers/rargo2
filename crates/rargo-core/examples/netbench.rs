use rargo_core::io;
use rargo_core::net::Endpoint;
use std::time::Instant;

fn main() {
    io::drive(4, || {
        let args: Vec<String> = std::env::args().collect();
        let addr = args[2].parse().unwrap();
        match args[1].as_str() {
            "serve" => {
                let ep = Endpoint::bind(addr, true).unwrap();
                loop {
                    let c = ep.accept().unwrap();
                    io::spawn(move || {
                        let mut total = 0usize;
                        while let Some(m) = c.recv() {
                            if m == b"end" {
                                c.send(total.to_string().as_bytes()).unwrap();
                                break;
                            }
                            total += m.len();
                        }
                    });
                }
            }
            "send" => {
                let mib: usize = args[3].parse().unwrap();
                let ep = Endpoint::bind("0.0.0.0:0".parse().unwrap(), false).unwrap();
                let c = ep.connect(addr).unwrap();
                let chunk = vec![7u8; 1 << 20];
                let t = Instant::now();
                for _ in 0..mib {
                    c.send(&chunk).unwrap();
                }
                c.send(b"end").unwrap();
                let got = c.recv().unwrap();
                let dt = t.elapsed().as_secs_f64();
                println!(
                    "{} MiB in {:.2} s = {:.1} MiB/s; packets sent {}, retransmits {}, rtt {:?}",
                    String::from_utf8_lossy(&got).parse::<usize>().unwrap() >> 20,
                    dt,
                    mib as f64 / dt,
                    c.stats.sent_packets.get(),
                    c.stats.retransmits.get(),
                    c.rtt()
                );
            }
            _ => panic!("usage"),
        }
    });
}
