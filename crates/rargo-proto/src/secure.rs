//! The encrypted channel between client and server.
//!
//! Noise `NNpsk0` with a pre-shared key per user: both ends prove knowledge
//! of the key, the session keys are fresh per connection, and everything
//! after the first message is forward secret.
//!
//! Wire format:
//!
//! ```text
//! client: MAGIC | u8 n | user (n bytes) | u16 len | handshake message 1
//! server: u8 0 | u16 len | handshake message 2
//!     or: u8 1 | u16 len | error text            (then closes)
//! then, both ways: records of  u32 len | ciphertext
//! ```
//!
//! The first handshake message may carry the request itself (encrypted
//! under the pre-shared key), which saves a round trip per command. A
//! record's plaintext is one flag byte (1 = last record of a message)
//! followed by data; a message is the concatenation of its records.

use snow::{HandshakeState, StatelessTransportState};
use std::io::{self, Read, Write};
use std::sync::Arc;

/// Ends in the protocol version, so that a client of another version is
/// told so instead of failing to decode.
pub const MAGIC: &[u8; 8] = b"rargo\x06\r\n";
const PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
/// Plaintext bytes per record (Noise messages are at most 65535 bytes,
/// including the 16-byte tag).
const RECORD: usize = 65000;
/// Largest request that still fits in the first handshake message.
pub const EARLY_MAX: usize = 60000;
/// Largest message accepted from the peer.
const MESSAGE_MAX: usize = 1 << 30;

fn err(e: snow::Error) -> io::Error {
    io::Error::other(format!("secure channel: {e}"))
}

/// The key shared with a user, derived from whatever secret text the key
/// file holds.
pub fn psk_from_secret(secret: &[u8]) -> [u8; 32] {
    let trimmed = secret.trim_ascii();
    *blake3::Hasher::new_derive_key("rargo client key v1").update(trimmed).finalize().as_bytes()
}

/// The key used on a loopback-only server started without a key file.
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
    /// Appends the records carrying `msg` to `out`.
    pub fn seal(&mut self, msg: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        let mut chunks = msg.chunks(RECORD).peekable();
        let mut plain = Vec::with_capacity(RECORD + 1);
        let mut cipher = vec![0u8; RECORD + 64];
        // An empty message is still one record.
        let mut first = true;
        while first || chunks.peek().is_some() {
            first = false;
            let chunk = chunks.next().unwrap_or(&[]);
            plain.clear();
            plain.push(chunks.peek().is_none() as u8);
            plain.extend_from_slice(chunk);
            let n = self.keys.write_message(self.nonce, &plain, &mut cipher).map_err(err)?;
            self.nonce += 1;
            out.extend_from_slice(&(n as u32).to_le_bytes());
            out.extend_from_slice(&cipher[..n]);
        }
        Ok(())
    }
}

impl Opener {
    /// Takes one record's ciphertext; returns a message when it completes
    /// one.
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
        self.partial.extend_from_slice(&plain[1..n]);
        Ok(if plain[0] == 1 { Some(std::mem::take(&mut self.partial)) } else { None })
    }
}

fn split(hs: HandshakeState) -> io::Result<(Sealer, Opener)> {
    let keys = Arc::new(hs.into_stateless_transport_mode().map_err(err)?);
    Ok((Sealer { keys: keys.clone(), nonce: 0 }, Opener { keys, nonce: 0, partial: Vec::new() }))
}

/// Client side, step one: the bytes to send first. `early` (at most
/// [`EARLY_MAX`] bytes) rides in the handshake message.
pub fn client_start(user: &str, psk: &[u8; 32], early: &[u8]) -> io::Result<(HandshakeState, Vec<u8>)> {
    if user.len() > 255 {
        return Err(io::Error::other("user name too long"));
    }
    let mut hs = snow::Builder::new(PARAMS.parse().map_err(err)?).psk(0, psk).map_err(err)?.build_initiator().map_err(err)?;
    let mut msg = vec![0u8; early.len() + 256];
    let n = hs.write_message(early, &mut msg).map_err(err)?;
    let mut out = Vec::with_capacity(n + 16 + user.len());
    out.extend_from_slice(MAGIC);
    out.push(user.len() as u8);
    out.extend_from_slice(user.as_bytes());
    out.extend_from_slice(&(n as u16).to_le_bytes());
    out.extend_from_slice(&msg[..n]);
    Ok((hs, out))
}

/// Client side, step two: the server's handshake message.
pub fn client_finish(mut hs: HandshakeState, msg2: &[u8]) -> io::Result<(Sealer, Opener)> {
    let mut payload = vec![0u8; msg2.len()];
    hs.read_message(msg2, &mut payload).map_err(|_| {
        io::Error::new(io::ErrorKind::PermissionDenied, "the server did not accept this key (wrong key or user?)")
    })?;
    split(hs)
}

/// Server side: answers the client's handshake message. Returns the reply
/// to send (without the status byte and length), the early data, and the
/// channel. Fails if the client does not hold `psk`.
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
    let (s, o) = split(hs)?;
    Ok((msg2, early, s, o))
}

/// Writes one message (blocking).
pub fn write_msg<W: Write>(w: &mut W, sealer: &mut Sealer, msg: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(msg.len() + msg.len() / RECORD * 24 + 64);
    sealer.seal(msg, &mut buf)?;
    w.write_all(&buf)
}

/// Reads one message (blocking). `None` at a clean end of stream.
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

// ---------------------------------------------------------------------------
// The server-worker link
// ---------------------------------------------------------------------------

const LINK_PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const LINK_PROLOGUE: &[u8] = b"rargo worker link v2";

/// Encryption for a link whose messages arrive in any order (the transport
/// between server and workers): every message names the nonces it was
/// sealed with, and no nonce is used twice. Keys are fresh per connection.
pub struct LinkCipher {
    keys: StatelessTransportState,
    next: std::sync::atomic::AtomicU64,
}

impl LinkCipher {
    fn new(hs: HandshakeState) -> io::Result<LinkCipher> {
        Ok(LinkCipher { keys: hs.into_stateless_transport_mode().map_err(err)?, next: std::sync::atomic::AtomicU64::new(0) })
    }

    /// `u64 first nonce | records`, every record but the last carrying
    /// [`RECORD`] bytes of plaintext.
    pub fn seal(&self, msg: &[u8]) -> io::Result<Vec<u8>> {
        let records = msg.len().div_ceil(RECORD).max(1);
        let base = self.next.fetch_add(records as u64, std::sync::atomic::Ordering::Relaxed);
        let mut out = vec![0u8; 8 + msg.len() + records * 16];
        out[..8].copy_from_slice(&base.to_le_bytes());
        let mut at = 8;
        for i in 0..records {
            let chunk = &msg[i * RECORD..((i + 1) * RECORD).min(msg.len())];
            at += self.keys.write_message(base + i as u64, chunk, &mut out[at..]).map_err(err)?;
        }
        debug_assert_eq!(at, out.len());
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
        out.truncate(at);
        Ok(out)
    }
}

fn link_builder(psk: &[u8; 32]) -> io::Result<snow::Builder<'_>> {
    snow::Builder::new(LINK_PARAMS.parse().map_err(err)?).prologue(LINK_PROLOGUE).map_err(err)?.psk(0, psk).map_err(err)
}

/// The connecting side's first message.
pub fn link_start(psk: &[u8; 32]) -> io::Result<(HandshakeState, Vec<u8>)> {
    let mut hs = link_builder(psk)?.build_initiator().map_err(err)?;
    let mut msg = vec![0u8; 128];
    let n = hs.write_message(&[], &mut msg).map_err(err)?;
    msg.truncate(n);
    Ok((hs, msg))
}

/// The accepting side: the reply and the cipher. Fails if the peer does
/// not hold `psk`.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(a: &[u8; 32], b: &[u8; 32], early: &[u8]) -> io::Result<((Sealer, Opener), (Sealer, Opener), Vec<u8>)> {
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
        // The client cannot open its own messages (directions have their own keys).
        assert!(client.open(&sealed[1]).is_err());
        let (_, m1) = link_start(&psk_from_secret(b"other")).unwrap();
        assert!(link_accept(&k, &m1).is_err());
    }
}
