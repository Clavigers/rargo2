pub struct Sha1 {
    h: [u32; 5],
    block: [u8; 64],
    filled: usize,
    len: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Sha1 { h: [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0], block: [0; 64], filled: 0, len: 0 }
    }
}

impl Sha1 {
    pub fn update(&mut self, mut data: &[u8]) {
        self.len += data.len() as u64;
        while !data.is_empty() {
            let take = (64 - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled == 64 {
                compress(&mut self.h, &self.block);
                self.filled = 0;
            }
        }
    }

    pub fn finish(mut self) -> [u8; 20] {
        let bits = self.len.wrapping_mul(8);
        let mut tail = vec![0x80u8];
        let used = (self.filled + 1) % 64;
        tail.resize(1 + if used <= 56 { 56 - used } else { 120 - used }, 0);
        tail.extend_from_slice(&bits.to_be_bytes());
        let len = self.len;
        self.update(&tail);
        self.len = len;
        assert_eq!(self.filled, 0);
        let mut out = [0u8; 20];
        for (i, word) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

fn compress(h: &mut [u32; 5], block: &[u8; 64]) {
    let mut w = [0u32; 80];
    for i in 0..16 {
        w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *h;
    for (i, wi) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
            20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
            _ => (b ^ c ^ d, 0xCA62_C1D6),
        };
        let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = t;
    }
    for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
        *x = x.wrapping_add(y);
    }
}

pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut s = Sha1::default();
    s.update(data);
    s.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(d: [u8; 20]) -> String {
        d.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn known_digests_in_one_piece_and_in_many() {
        assert_eq!(hex(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex(sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(sha1(b"blob 6\0hello\n")), "ce013625030ba8dba906f756967f9e9ca394464a");
        let long: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        for len in [55, 56, 63, 64, 65, 119, 120, 1000] {
            let whole = sha1(&long[..len]);
            let mut parts = Sha1::default();
            for chunk in long[..len].chunks(13) {
                parts.update(chunk);
            }
            assert_eq!(parts.finish(), whole, "{len}");
        }
        assert_eq!(hex(sha1(&[b'a'; 1_000_000])), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    }
}
