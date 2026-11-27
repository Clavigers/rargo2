use snow::{HandshakeState, StatelessTransportState};
use std::io::{self, Read, Write};
use std::sync::Arc;

pub const MAGIC: &[u8; 8] = b"rargo\x0a\r\n";
const PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const RECORD: usize = 65000;
pub const EARLY_MAX: usize = 60000;
const MESSAGE_MAX: usize = 1 << 30;
const _: () = assert!(RECORD + 1 + 16 <= 65535, "a record with its flag and tag fits a Noise message");
const _: () = assert!(EARLY_MAX + 256 <= 65535, "early data fits the first handshake message");
const _: () = assert!(EARLY_MAX < RECORD);

fn err(e: snow::Error) -> io::Error {
    io::Error::other(format!("secure channel: {e}"))
}

pub fn psk_from_secret(secret: &[u8]) -> [u8; 32] {
    let trimmed = secret.trim_ascii();
    *blake3::Hasher::new_derive_key("rargo client key v1").update(trimmed).finalize().as_bytes()
}

pub fn open_psk() -> [u8; 32] {
    psk_from_secret(b"rargo: no key configured (loopback only)")
}

pub struct Sealer {
    keys: Arc<StatelessTransportState>,
    nonce: u64,
}

pub struct Opener {
    keys: Arc<StatelessTransportState>,
    nonce: u64,
    partial: Vec<u8>,
}

impl Sealer {
    /// Ciphertext records, one per chunk, with no length prefix. The caller frames them.
    pub fn seal_records(&mut self, msg: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        let mut chunks = msg.chunks(RECORD).peekable();
        let mut plain = Vec::with_capacity(RECORD + 1);
        let mut cipher = vec![0u8; RECORD + 64];
        let mut records = Vec::new();
        let mut first = true;
        let nonce_before = self.nonce;
        while first || chunks.peek().is_some() {
            first = false;
            let chunk = chunks.next().unwrap_or(&[]);
            plain.clear();
            plain.push(chunks.peek().is_none() as u8);
            plain.extend_from_slice(chunk);
            let n = self.keys.write_message(self.nonce, &plain, &mut cipher).map_err(err)?;
            self.nonce += 1;
            assert_eq!(n, plain.len() + 16, "ciphertext is plaintext plus the tag");
            records.push(cipher[..n].to_vec());
        }
        let count = msg.len().div_ceil(RECORD).max(1);
        assert_eq!(self.nonce - nonce_before, count as u64);
        assert_eq!(records.len(), count);
        Ok(records)
    }

    pub fn seal(&mut self, msg: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        let nonce_before = self.nonce;
        let out_before = out.len();
        for rec in self.seal_records(msg)? {
            out.extend_from_slice(&(rec.len() as u32).to_le_bytes());
            out.extend_from_slice(&rec);
        }
        let records = msg.len().div_ceil(RECORD).max(1);
        assert_eq!(self.nonce - nonce_before, records as u64);
        assert_eq!(out.len() - out_before, msg.len() + records * (1 + 16 + 4));
        Ok(())
    }
}

impl Opener {
    pub fn open(&mut self, record: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let mut plain = vec![0u8; record.len()];
        let n = self.keys.read_message(self.nonce, record, &mut plain).map_err(err)?;
        self.nonce += 1;
        if n == 0 {
            return Err(io::Error::other("secure channel: empty record"));
        }
        if self.partial.len() + n > MESSAGE_MAX {
            return Err(io::Error::other("secure channel: message too large"));
        }
        assert!(n <= record.len());
        self.partial.extend_from_slice(&plain[1..n]);
        assert!(self.partial.len() <= MESSAGE_MAX);
        Ok(if plain[0] == 1 { Some(std::mem::take(&mut self.partial)) } else { None })
    }
}

fn split(hs: HandshakeState) -> io::Result<(Sealer, Opener)> {
    let keys = Arc::new(hs.into_stateless_transport_mode().map_err(err)?);
    Ok((Sealer { keys: keys.clone(), nonce: 0 }, Opener { keys, nonce: 0, partial: Vec::new() }))
}

pub fn client_start(user: &str, psk: &[u8; 32], early: &[u8]) -> io::Result<(HandshakeState, Vec<u8>)> {
    if user.len() > 255 {
        return Err(io::Error::other("user name too long"));
    }
    let mut hs = snow::Builder::new(PARAMS.parse().map_err(err)?).psk(0, psk).map_err(err)?.build_initiator().map_err(err)?;
    let mut msg = vec![0u8; early.len() + 256];
    assert!(early.len() <= EARLY_MAX);
    let n = hs.write_message(early, &mut msg).map_err(err)?;
    assert!(n <= u16::MAX as usize);
    assert!(n >= early.len());
    let mut out = Vec::with_capacity(n + 16 + user.len());
    out.extend_from_slice(MAGIC);
    out.push(user.len() as u8);
    out.extend_from_slice(user.as_bytes());
    out.extend_from_slice(&(n as u16).to_le_bytes());
    out.extend_from_slice(&msg[..n]);
    assert_eq!(out.len(), MAGIC.len() + 1 + user.len() + 2 + n);
    Ok((hs, out))
}

pub fn client_finish(mut hs: HandshakeState, msg2: &[u8]) -> io::Result<(Sealer, Opener)> {
    let mut payload = vec![0u8; msg2.len()];
    hs.read_message(msg2, &mut payload).map_err(|_| {
        io::Error::new(io::ErrorKind::PermissionDenied, "the server did not accept this key (wrong key or user?)")
    })?;
    split(hs)
}

pub fn server_accept(psk: &[u8; 32], msg1: &[u8]) -> io::Result<(Vec<u8>, Vec<u8>, Sealer, Opener)> {
    let mut hs = snow::Builder::new(PARAMS.parse().map_err(err)?).psk(0, psk).map_err(err)?.build_responder().map_err(err)?;
    let mut early = vec![0u8; msg1.len()];
    let n = hs
        .read_message(msg1, &mut early)
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "key mismatch"))?;
    early.truncate(n);
    let mut msg2 = vec![0u8; 256];
    let n = hs.write_message(&[], &mut msg2).map_err(err)?;
    msg2.truncate(n);
    assert!(n <= u16::MAX as usize);
    let (s, o) = split(hs)?;
    Ok((msg2, early, s, o))
}

pub fn write_msg<W: Write>(w: &mut W, sealer: &mut Sealer, msg: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(msg.len() + msg.len() / RECORD * 24 + 64);
    sealer.seal(msg, &mut buf)?;
    w.write_all(&buf)
}

pub fn read_msg<R: Read>(r: &mut R, opener: &mut Opener) -> io::Result<Option<Vec<u8>>> {
    loop {
        let mut len = [0u8; 4];
        match r.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let n = u32::from_le_bytes(len) as usize;
        if n > 65535 {
            return Err(io::Error::other("secure channel: oversized record"));
        }
        let mut rec = vec![0u8; n];
        r.read_exact(&mut rec)?;
        if let Some(m) = opener.open(&rec)? {
            return Ok(Some(m));
        }
    }
}


const LINK_PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const LINK_PROLOGUE: &[u8] = b"rargo worker link v2";

/// The agreed transport keys of a link. They never change, so a pool thread may seal and open with them.
pub struct LinkKeys {
    keys: StatelessTransportState,
}

impl LinkKeys {
    fn records(len: usize) -> usize {
        len.div_ceil(RECORD).max(1)
    }

    /// Seals `msg` under the record numbers starting at `base`, which the caller reserved for it.
    pub fn seal_from(&self, base: u64, msg: &[u8]) -> io::Result<Vec<u8>> {
        let records = Self::records(msg.len());
        let mut out = vec![0u8; 8 + msg.len() + records * 16];
        out[..8].copy_from_slice(&base.to_le_bytes());
        let mut at = 8;
        for i in 0..records {
            let chunk = &msg[i * RECORD..((i + 1) * RECORD).min(msg.len())];
            at += self.keys.write_message(base + i as u64, chunk, &mut out[at..]).map_err(err)?;
        }
        assert_eq!(at, out.len());
        Ok(out)
    }

    pub fn open(&self, wire: &[u8]) -> io::Result<Vec<u8>> {
        if wire.len() < 8 + 16 {
            return Err(io::Error::other("secure link: short message"));
        }
        let base = u64::from_le_bytes(wire[..8].try_into().unwrap());
        let body = &wire[8..];
        let mut out = vec![0u8; body.len()];
        let mut at = 0;
        for (i, rec) in body.chunks(RECORD + 16).enumerate() {
            at += self.keys.read_message(base + i as u64, rec, &mut out[at..]).map_err(err)?;
        }
        assert!(at <= body.len());
        out.truncate(at);
        Ok(out)
    }
}

/// One end of a link: its keys and the next unused record number, which the owning thread hands out.
pub struct LinkCipher {
    keys: LinkKeys,
    next: std::cell::Cell<u64>,
    binding: [u8; 32],
}

impl LinkCipher {
    fn new(hs: HandshakeState) -> io::Result<LinkCipher> {
        let binding = blake3::derive_key("rargo link datagram tag v1", hs.get_handshake_hash());
        Ok(LinkCipher {
            keys: LinkKeys { keys: hs.into_stateless_transport_mode().map_err(err)? },
            next: std::cell::Cell::new(0),
            binding,
        })
    }

    pub fn binding(&self) -> [u8; 32] {
        self.binding
    }

    pub fn keys(&self) -> &LinkKeys {
        &self.keys
    }

    /// Takes the record numbers a message of `len` bytes is sealed under and returns the first.
    pub fn reserve(&self, len: usize) -> u64 {
        let base = self.next.get();
        self.next.set(base.wrapping_add(LinkKeys::records(len) as u64));
        base
    }

    pub fn seal(&self, msg: &[u8]) -> io::Result<Vec<u8>> {
        self.keys.seal_from(self.reserve(msg.len()), msg)
    }

    pub fn open(&self, wire: &[u8]) -> io::Result<Vec<u8>> {
        self.keys.open(wire)
    }
}

fn link_builder(psk: &[u8; 32]) -> io::Result<snow::Builder<'_>> {
    snow::Builder::new(LINK_PARAMS.parse().map_err(err)?).prologue(LINK_PROLOGUE).map_err(err)?.psk(0, psk).map_err(err)
}

pub fn link_start(psk: &[u8; 32]) -> io::Result<(HandshakeState, Vec<u8>)> {
    let mut hs = link_builder(psk)?.build_initiator().map_err(err)?;
    let mut msg = vec![0u8; 128];
    let n = hs.write_message(&[], &mut msg).map_err(err)?;
    msg.truncate(n);
    Ok((hs, msg))
}

pub fn link_accept(psk: &[u8; 32], msg1: &[u8]) -> io::Result<(Vec<u8>, LinkCipher)> {
    let mut hs = link_builder(psk)?.build_responder().map_err(err)?;
    let mut scratch = vec![0u8; msg1.len() + 16];
    hs.read_message(msg1, &mut scratch).map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "link key mismatch"))?;
    let mut msg2 = vec![0u8; 128];
    let n = hs.write_message(&[], &mut msg2).map_err(err)?;
    msg2.truncate(n);
    Ok((msg2, LinkCipher::new(hs)?))
}

pub fn link_finish(mut hs: HandshakeState, msg2: &[u8]) -> io::Result<LinkCipher> {
    let mut scratch = vec![0u8; msg2.len() + 16];
    hs.read_message(msg2, &mut scratch).map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "link key mismatch"))?;
    LinkCipher::new(hs)
}

/// An initiator handshake whose ephemeral key was supplied by the caller.
pub struct LinkPending(HandshakeState);

impl LinkPending {
    pub fn finish(self, msg: &[u8]) -> io::Result<LinkCipher> {
        link_finish(self.0, msg)
    }
}

fn link_builder_fixed<'a>(psk: &'a [u8; 32], ephemeral: &'a [u8]) -> io::Result<snow::Builder<'a>> {
    let params = LINK_PARAMS.parse().map_err(err)?;
    let resolver = snow::resolvers::FallbackResolver::new(Box::new(SuppliedResolver), Box::new(snow::resolvers::DefaultResolver));
    Ok(snow::Builder::with_resolver(params, Box::new(resolver))
        .prologue(LINK_PROLOGUE)
        .map_err(err)?
        .psk(0, psk)
        .map_err(err)?
        .fixed_ephemeral_key_for_testing_only(ephemeral))
}

pub fn link_start_with(psk: &[u8; 32], ephemeral: &[u8; 32]) -> io::Result<(LinkPending, Vec<u8>)> {
    let mut hs = link_builder_fixed(psk, ephemeral)?.build_initiator().map_err(err)?;
    let mut msg = vec![0u8; 128];
    let n = hs.write_message(&[], &mut msg).map_err(err)?;
    msg.truncate(n);
    Ok((LinkPending(hs), msg))
}

pub fn link_accept_with(psk: &[u8; 32], msg1: &[u8], ephemeral: &[u8; 32]) -> io::Result<(Vec<u8>, LinkCipher)> {
    let mut hs = link_builder_fixed(psk, ephemeral)?.build_responder().map_err(err)?;
    let mut scratch = vec![0u8; msg1.len() + 16];
    hs.read_message(msg1, &mut scratch).map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "link key mismatch"))?;
    let mut msg2 = vec![0u8; 128];
    let n = hs.write_message(&[], &mut msg2).map_err(err)?;
    msg2.truncate(n);
    Ok((msg2, LinkCipher::new(hs)?))
}

/// Randomness comes from the caller. Asking this resolver for bytes is a bug.
struct SuppliedRng;

impl snow::types::Random for SuppliedRng {
    fn try_fill_bytes(&mut self, _dest: &mut [u8]) -> Result<(), snow::Error> {
        Err(snow::Error::Rng)
    }
}

struct SuppliedResolver;

impl snow::resolvers::CryptoResolver for SuppliedResolver {
    fn resolve_rng(&self) -> Option<Box<dyn snow::types::Random>> {
        Some(Box::new(SuppliedRng))
    }

    fn resolve_dh(&self, _choice: &snow::params::DHChoice) -> Option<Box<dyn snow::types::Dh>> {
        None
    }

    fn resolve_hash(&self, _choice: &snow::params::HashChoice) -> Option<Box<dyn snow::types::Hash>> {
        None
    }

    fn resolve_cipher(&self, _choice: &snow::params::CipherChoice) -> Option<Box<dyn snow::types::Cipher>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Ends = (Sealer, Opener);

    fn pair(a: &[u8; 32], b: &[u8; 32], early: &[u8]) -> io::Result<(Ends, Ends, Vec<u8>)> {
        let (hs, first) = client_start("ann", a, early)?;
        assert_eq!(&first[..8], MAGIC);
        let ulen = first[8] as usize;
        assert_eq!(&first[9..9 + ulen], b"ann");
        let mlen = u16::from_le_bytes([first[9 + ulen], first[10 + ulen]]) as usize;
        let msg1 = &first[11 + ulen..11 + ulen + mlen];
        let (msg2, got_early, ss, so) = server_accept(b, msg1)?;
        let (cs, co) = client_finish(hs, &msg2)?;
        Ok(((cs, co), (ss, so), got_early))
    }

    #[test]
    fn messages_cross_in_both_directions() {
        let k = psk_from_secret(b"secret\n");
        assert_eq!(k, psk_from_secret(b"  secret"));
        let ((mut cs, mut co), (mut ss, mut so), early) = pair(&k, &k, b"early request").unwrap();
        assert_eq!(early, b"early request");
        for msg in [vec![], vec![1u8; 10], vec![2u8; RECORD], vec![3u8; RECORD * 3 + 17]] {
            let mut wire = Vec::new();
            write_msg(&mut wire, &mut cs, &msg).unwrap();
            assert_eq!(read_msg(&mut &wire[..], &mut so).unwrap().unwrap(), msg);
            let mut wire = Vec::new();
            write_msg(&mut wire, &mut ss, &msg).unwrap();
            assert_eq!(read_msg(&mut &wire[..], &mut co).unwrap().unwrap(), msg);
        }
        assert!(read_msg(&mut &[][..], &mut co).unwrap().is_none());
    }

    #[test]
    fn magic_names_the_protocol_version() {
        assert_eq!(MAGIC[5] as u32, crate::PROTO_VERSION);
    }

    #[test]
    fn a_different_key_is_refused() {
        let a = psk_from_secret(b"one");
        let b = psk_from_secret(b"two");
        assert!(pair(&a, &b, b"x").is_err());
    }

    #[test]
    fn tampering_is_detected() {
        let k = open_psk();
        let ((mut cs, _), (_, mut so), _) = pair(&k, &k, b"").unwrap();
        let mut wire = Vec::new();
        write_msg(&mut wire, &mut cs, b"hello").unwrap();
        let last = wire.len() - 1;
        wire[last] ^= 1;
        assert!(read_msg(&mut &wire[..], &mut so).is_err());
    }

    #[test]
    fn link_messages_open_in_any_order_and_only_with_the_key() {
        let k = psk_from_secret(b"link");
        let (hs, m1) = link_start(&k).unwrap();
        let (m2, server) = link_accept(&k, &m1).unwrap();
        let client = link_finish(hs, &m2).unwrap();
        let msgs = [vec![], vec![1u8; 10], vec![2u8; RECORD], vec![3u8; RECORD * 2 + 5]];
        let sealed: Vec<Vec<u8>> = msgs.iter().map(|m| client.seal(m).unwrap()).collect();
        for (m, w) in msgs.iter().zip(&sealed).rev() {
            assert!(m.len() < 8 || !w.windows(8).any(|x| x == &m[..8]), "plaintext on the wire");
            assert_eq!(&server.open(w).unwrap(), m);
        }
        assert_eq!(client.open(&server.seal(b"back").unwrap()).unwrap(), b"back");
        let mut bad = sealed[3].clone();
        bad[RECORD + 100] ^= 1;
        assert!(server.open(&bad).is_err());
        assert!(client.open(&sealed[1]).is_err());
        let (_, m1) = link_start(&psk_from_secret(b"other")).unwrap();
        assert!(link_accept(&k, &m1).is_err());
    }
}
