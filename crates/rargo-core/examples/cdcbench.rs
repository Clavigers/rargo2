use dashmap::DashMap;
use rargo_core::chunk::{MAX_CHUNK, MIN_CHUNK, AVG_CHUNK};
use rargo_core::hash::Hash;
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

fn gear() -> &'static [u64; 256] {
    static T: std::sync::OnceLock<[u64; 256]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u64; 256];
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for v in t.iter_mut() {
            x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            *v = z ^ (z >> 31);
        }
        t
    })
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Rule {
    Mask,
    RepMax { min: usize, horizon: usize },
}

fn mask(data: &[u8], cuts: &mut Vec<usize>) {
    let g = gear();
    const MASK_S: u64 = (1 << 18) - 1;
    const MASK_L: u64 = (1 << 14) - 1;
    let mut start = 0;
    while data.len() - start > MIN_CHUNK {
        let end = (start + MAX_CHUNK).min(data.len());
        let mid = (start + AVG_CHUNK).min(end);
        let mut h: u64 = 0;
        let mut cut = end;
        let mut i = start + MIN_CHUNK;
        while i < mid {
            h = (h << 1).wrapping_add(g[data[i] as usize]);
            if h & MASK_S == 0 {
                cut = i + 1;
                break;
            }
            i += 1;
        }
        if cut == end {
            while i < end {
                h = (h << 1).wrapping_add(g[data[i] as usize]);
                if h & MASK_L == 0 {
                    cut = i + 1;
                    break;
                }
                i += 1;
            }
        }
        cuts.push(cut - start);
        start = cut;
    }
    if start < data.len() {
        cuts.push(data.len() - start);
    }
}

const WINDOW: usize = 64;

fn repmax(data: &[u8], min: usize, horizon: usize, cuts: &mut Vec<usize>) {
    let g = gear();
    let peek = 2 * min + horizon;
    let mut pos = 0;
    while pos < data.len() {
        let mut d = &data[pos..(pos + peek).min(data.len())];
        if d.len() < 2 * min {
            cuts.push(d.len());
            return;
        }
        d = &d[..d.len() - min];
        let mut initial: u64 = 0;
        for &b in &d[min - WINDOW.min(min)..min] {
            initial = (initial << 1).wrapping_add(g[b as usize]);
        }
        loop {
            let mut hash = initial;
            let mut best = hash;
            let mut best_off = 0;
            for (i, &b) in d[min..].iter().enumerate() {
                if i >= best_off + min {
                    break;
                }
                hash = (hash << 1).wrapping_add(g[b as usize]);
                if best < hash {
                    best = hash;
                    best_off = i + 1;
                }
            }
            if best_off < min {
                cuts.push(min + best_off);
                pos += min + best_off;
                break;
            }
            d = &d[..best_off];
        }
    }
}

fn repmax_fast(data: &[u8], min: usize, horizon: usize, cuts: &mut Vec<usize>, records: &mut Vec<usize>) {
    let g = gear();
    let peek = 2 * min + horizon;
    let mut pos = 0;
    while pos < data.len() {
        let avail = (data.len() - pos).min(peek);
        if avail < 2 * min {
            cuts.push(avail);
            return;
        }
        let limit = avail - 2 * min;
        let d = &data[pos..];
        let mut hash: u64 = 0;
        for &b in &d[min - WINDOW.min(min)..min] {
            hash = (hash << 1).wrapping_add(g[b as usize]);
        }
        let mut best = hash;
        records.clear();
        records.push(0);
        let mut i = 0;
        loop {
            let stop = limit.min(records[records.len() - 1] + min);
            if i + 8 > stop {
                break;
            }
            let b: [u8; 8] = d[min + i..min + i + 8].try_into().unwrap();
            let mut s = 0u64;
            for (k, &x) in b.iter().enumerate() {
                s = (s << 1).wrapping_add(g[x as usize]);
                let h = (hash << (k + 1)).wrapping_add(s);
                if best < h {
                    best = h;
                    records.push(i + k + 1);
                }
            }
            hash = (hash << 8).wrapping_add(s);
            i += 8;
        }
        while i < limit && i < records[records.len() - 1] + min {
            hash = (hash << 1).wrapping_add(g[d[min + i] as usize]);
            if best < hash {
                best = hash;
                records.push(i + 1);
            }
            i += 1;
        }
        let mut lim = limit;
        let off = loop {
            let mut j = 0;
            while j + 1 < records.len() && records[j + 1] <= lim && records[j + 1] <= records[j] + min {
                j += 1;
            }
            let o = records[j];
            if o < min {
                break o;
            }
            lim = o - min;
        };
        cuts.push(min + off);
        pos += min + off;
    }
}

fn elf_boundaries(data: &[u8]) -> Option<Vec<usize>> {
    if data.len() < 64 || &data[..4] != b"\x7fELF" || data[4] != 2 || data[5] != 1 {
        return None;
    }
    let u64_at = |o: usize| data.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()) as usize);
    let u16_at = |o: usize| data.get(o..o + 2).map(|b| u16::from_le_bytes(b.try_into().unwrap()) as usize);
    let shoff = u64_at(0x28)?;
    let shentsize = u16_at(0x3a)?;
    let shnum = u16_at(0x3c)?;
    if shentsize < 64 || shoff == 0 {
        return None;
    }
    let mut b = vec![shoff];
    for i in 0..shnum {
        let sh = shoff.checked_add(i * shentsize)?;
        let typ = data.get(sh + 4..sh + 8).map(|x| u32::from_le_bytes(x.try_into().unwrap()))?;
        let off = u64_at(sh + 0x18)?;
        if typ != 8 && off > 0 && off < data.len() {
            b.push(off);
        }
    }
    b.sort_unstable();
    b.dedup();
    Some(b)
}

fn structural(data: &[u8]) -> Vec<usize> {
    if let Some(members) = rargo_core::ar::members(data) {
        let mut b = Vec::new();
        for m in &members {
            b.push(m.header_offset);
            let start = m.data.as_ptr() as usize - data.as_ptr() as usize;
            b.push(start);
            if let Some(inner) = elf_boundaries(m.data) {
                b.extend(inner.into_iter().map(|o| start + o));
            }
        }
        b.sort_unstable();
        b.dedup();
        return b;
    }
    elf_boundaries(data).unwrap_or_default()
}

fn cuts(data: &[u8], rule: Rule) -> Vec<usize> {
    cuts_by(data, rule, false)
}

fn cuts_by(data: &[u8], rule: Rule, simple: bool) -> Vec<usize> {
    let mut records = Vec::new();
    let mut bounds = structural(data);
    bounds.retain(|&o| o > 0 && o < data.len());
    bounds.push(data.len());
    let hard = if data.starts_with(b"!<arch>\n") { 8 } else { 0 };
    let mut lens: Vec<usize> = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while start < data.len() {
        let mut end = bounds[i];
        while end - start < MIN_CHUNK && i + 1 < bounds.len() && end != hard {
            i += 1;
            end = bounds[i];
        }
        i += 1;
        match rule {
            Rule::Mask => mask(&data[start..end], &mut lens),
            Rule::RepMax { min, horizon } if simple => repmax(&data[start..end], min, horizon, &mut lens),
            Rule::RepMax { min, horizon } => repmax_fast(&data[start..end], min, horizon, &mut lens, &mut records),
        }
        start = end;
    }
    lens
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let Ok(t) = e.file_type() else { continue };
        if t.is_dir() {
            files(&e.path(), out);
        } else if t.is_file() {
            out.push(e.path());
        }
    }
}

fn versions(project: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(project)
        .unwrap_or_else(|e| panic!("{}: {e}", project.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.len() == 3 && n.starts_with('v') && n[1..].bytes().all(|b| b.is_ascii_digit())
                })
        })
        .collect();
    v.sort();
    v
}

struct Totals {
    bytes: u64,
    chunks: u64,
    store_raw: u64,
    store_zstd: u64,
    send: u64,
    send_base: u64,
    out_of_bounds: u64,
}

fn measure(projects: &[PathBuf], rule: Rule) -> Totals {
    let distinct: DashMap<Hash, ()> = DashMap::new();
    let store_raw = AtomicU64::new(0);
    let store_zstd = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let chunks = AtomicU64::new(0);
    let oob = AtomicU64::new(0);
    let mismatched = AtomicU64::new(0);
    let (mut send, mut send_base) = (0u64, 0u64);
    for project in projects {
        let mut previous: Option<HashSet<Hash>> = None;
        for v in versions(project) {
            let mut list = Vec::new();
            files(&v, &mut list);
            let per_file: Vec<Vec<(Hash, u32)>> = list
                .par_iter()
                .map(|p| {
                    let Ok(data) = std::fs::read(p) else { return Vec::new() };
                    let lens = cuts(&data, rule);
                    let reference: Vec<usize> = match rule {
                        Rule::Mask => rargo_core::chunk::chunk(&data).iter().map(|c| c.len as usize).collect(),
                        Rule::RepMax { .. } => cuts_by(&data, rule, true),
                    };
                    if reference != lens {
                        mismatched.fetch_add(1, Ordering::Relaxed);
                    }
                    let mut out = Vec::with_capacity(lens.len());
                    let mut pos = 0;
                    for &len in &lens {
                        let piece = &data[pos..pos + len];
                        pos += len;
                        if let Rule::RepMax { min, .. } = rule {
                            if len >= 2 * min {
                                oob.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        let h = Hash::of(piece);
                        if distinct.insert(h, ()).is_none() {
                            store_raw.fetch_add(len as u64, Ordering::Relaxed);
                            let z = zstd::bulk::compress(piece, 3).expect("zstd").len();
                            store_zstd.fetch_add(z.min(len) as u64, Ordering::Relaxed);
                        }
                        out.push((h, len as u32));
                    }
                    bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                    chunks.fetch_add(lens.len() as u64, Ordering::Relaxed);
                    out
                })
                .collect();
            let mut here: HashSet<Hash> = HashSet::new();
            let mut new_bytes = 0u64;
            let mut all_bytes = 0u64;
            for (h, len) in per_file.iter().flatten() {
                let first_here = here.insert(*h);
                all_bytes += *len as u64;
                if let Some(prev) = &previous {
                    if first_here && !prev.contains(h) {
                        new_bytes += *len as u64;
                    }
                }
            }
            if previous.is_some() {
                send += new_bytes;
                send_base += all_bytes;
            }
            eprintln!(
                "  {rule:?} {}: {} files, {} bytes, {} new bytes",
                v.display(),
                list.len(),
                all_bytes,
                if previous.is_some() { new_bytes.to_string() } else { "-".into() }
            );
            previous = Some(here);
        }
    }
    let m = mismatched.load(Ordering::Relaxed);
    assert_eq!(m, 0, "{m} files cut differently from the reference implementation");
    Totals {
        bytes: bytes.into_inner(),
        chunks: chunks.into_inner(),
        store_raw: store_raw.into_inner(),
        store_zstd: store_zstd.into_inner(),
        send,
        send_base,
        out_of_bounds: oob.into_inner(),
    }
}

fn speed(projects: &[PathBuf], rules: &[Rule], limit: u64) -> Vec<f64> {
    let mut sample: Vec<Vec<u8>> = Vec::new();
    let mut total = 0u64;
    'outer: for project in projects {
        let per_project = limit / projects.len() as u64;
        let mut taken = 0u64;
        if let Some(v) = versions(project).first() {
            let mut list = Vec::new();
            files(v, &mut list);
            for p in list {
                if taken >= per_project {
                    break;
                }
                if let Ok(d) = std::fs::read(&p) {
                    taken += d.len() as u64;
                    total += d.len() as u64;
                    sample.push(d);
                }
                if total >= limit {
                    break 'outer;
                }
            }
        }
    }
    rules
        .iter()
        .map(|&rule| {
            let mut best = f64::MAX;
            for _ in 0..3 {
                let t = Instant::now();
                let mut n = 0usize;
                for d in &sample {
                    n += cuts(d, rule).len();
                }
                std::hint::black_box(n);
                best = best.min(t.elapsed().as_secs_f64());
            }
            total as f64 / (1 << 20) as f64 / best
        })
        .collect()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut min, mut horizon) = (32 << 10, None);
    let mut which = vec!["mask".to_string(), "repmax".to_string()];
    let mut speed_limit: u64 = 2 << 30;
    let mut projects = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--min" => min = args.next().unwrap().parse().unwrap(),
            "--horizon" => horizon = Some(args.next().unwrap().parse().unwrap()),
            "--rules" => which = args.next().unwrap().split(',').map(String::from).collect(),
            "--speed-bytes" => speed_limit = args.next().unwrap().parse().unwrap(),
            _ => projects.push(PathBuf::from(a)),
        }
    }
    let horizon = horizon.unwrap_or(8 * min);
    let rules: Vec<Rule> = which
        .iter()
        .map(|w| match w.as_str() {
            "mask" => Rule::Mask,
            "repmax" => Rule::RepMax { min, horizon },
            other => panic!("unknown rule {other}"),
        })
        .collect();
    let speeds = if speed_limit > 0 { speed(&projects, &rules, speed_limit) } else { vec![0.0; rules.len()] };
    println!("rule\tmean_chunk\tchunks\tbytes\tstore_raw\tstore_zstd\tsend\tsend_of\tcut_MiB_s\tout_of_bounds");
    for (rule, mbps) in rules.iter().zip(speeds) {
        let t = measure(&projects, *rule);
        println!(
            "{}\t{:.0}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.0}\t{}",
            match rule {
                Rule::Mask => "mask".to_string(),
                Rule::RepMax { min, horizon } => format!("repmax(min={min},horizon={horizon})"),
            },
            t.bytes as f64 / t.chunks.max(1) as f64,
            t.chunks,
            t.bytes,
            t.store_raw,
            t.store_zstd,
            t.send,
            t.send_base,
            mbps,
            t.out_of_bounds
        );
    }
}
