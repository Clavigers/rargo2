//! One UDP connection's handshake.
//!
//! The caller supplies the ephemeral key. Each step takes a received packet or
//! reports a lost one, and returns a packet to send, the agreed cipher and the
//! proven user, or a refusal.

use rargo_machine::StateMachine;
use super::{handshake_key, user_handshake_key, DATA_HDR, T_ACK, T_DATA};
use rargo_proto::secure::{LinkCipher, LinkPending, MAGIC, link_accept_with, link_start_with};
use std::io;

/// Noise_NNpsk0 writes the ephemeral public key and a tag, and nothing else.
const NOISE_LEN: usize = 48;
const RESENDS: u32 = 1;

pub struct Agreed {
    pub cipher: LinkCipher,
    pub user: Option<String>,
    pub binding: Option<[u8; 32]>,
}

impl std::fmt::Debug for Agreed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agreed").field("user", &self.user).field("binding", &self.binding).finish_non_exhaustive()
    }
}

pub enum Action {
    Send(Vec<u8>),
    Wait,
    NeedKey(String),
    Done(Agreed),
}

impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::Send(pkt) => f.debug_tuple("Send").field(&pkt.len()).finish(),
            Action::Wait => f.write_str("Wait"),
            Action::NeedKey(name) => f.debug_tuple("NeedKey").field(name).finish(),
            Action::Done(agreed) => f.debug_tuple("Done").field(agreed).finish(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Fresh,
    Waiting,
    Sending,
    NeedKey,
    Done,
    Failed,
}

enum Role {
    SharedInit { psk: [u8; 32] },
    SharedResp { psk: [u8; 32] },
    UserInit { name: String, psk: [u8; 32], marked: bool },
    UserResp,
}

pub struct Handshake {
    role: Role,
    phase: Phase,
    ephemeral: [u8; 32],
    hs: Option<LinkPending>,
    name: Option<String>,
    noise: Option<Vec<u8>>,
    last_sent: Option<Vec<u8>>,
    pending: Option<Agreed>,
    notify: Option<Vec<u8>>,
    seen: Vec<Vec<u8>>,
    resends_left: u32,
    failure: Option<(io::ErrorKind, String)>,
    timeout: &'static str,
}

impl Handshake {
    pub fn shared_initiator(key: [u8; 32], ephemeral: [u8; 32]) -> Handshake {
        Handshake::new(Role::SharedInit { psk: key }, ephemeral, "no answer from the server (is it running, and is the worker key the same?)")
    }

    pub fn shared_responder(key: [u8; 32], ephemeral: [u8; 32]) -> Handshake {
        Handshake::new(Role::SharedResp { psk: key }, ephemeral, "no key agreement")
    }

    pub fn user_initiator(name: &str, psk: [u8; 32], ephemeral: [u8; 32], marked: bool) -> Handshake {
        Handshake::new(Role::UserInit { name: name.to_string(), psk, marked }, ephemeral, "no answer from the server")
    }

    pub fn user_responder(ephemeral: [u8; 32]) -> Handshake {
        Handshake::new(Role::UserResp, ephemeral, "no key agreement")
    }

    fn new(role: Role, ephemeral: [u8; 32], timeout: &'static str) -> Handshake {
        let hs = Handshake {
            role,
            phase: Phase::Fresh,
            ephemeral,
            hs: None,
            name: None,
            noise: None,
            last_sent: None,
            pending: None,
            notify: None,
            seen: Vec::new(),
            resends_left: RESENDS,
            failure: None,
            timeout,
        };
        hs.invariants();
        hs
    }

    pub fn start(&mut self) -> io::Result<Action> {
        if self.phase != Phase::Fresh {
            return self.fail(io::ErrorKind::InvalidInput, "handshake already started");
        }
        let derived = match &self.role {
            Role::SharedInit { psk } => Some(handshake_key(psk)),
            Role::UserInit { psk, .. } => Some(user_handshake_key(psk)),
            Role::SharedResp { .. } | Role::UserResp => None,
        };
        let action = if let Some(psk) = derived {
            self.open_init(&psk)?
        } else {
            self.phase = Phase::Waiting;
            Action::Wait
        };
        self.invariants();
        Ok(action)
    }

    fn open_init(&mut self, psk: &[u8; 32]) -> io::Result<Action> {
        let (pending, noise) = match link_start_with(psk, &self.ephemeral) {
            Ok(v) => v,
            Err(e) => {
                self.invariants();
                return Err(e);
            }
        };
        assert_eq!(noise.len(), NOISE_LEN, "a handshake message has a fixed length");
        let pkt = match &self.role {
            Role::UserInit { name, marked, .. } => {
                let n = name.len().min(255);
                let mut first = Vec::with_capacity(1 + n + MAGIC.len() + noise.len());
                first.push(n as u8);
                first.extend_from_slice(&name.as_bytes()[..n]);
                if *marked {
                    first.extend_from_slice(MAGIC);
                }
                first.extend_from_slice(&noise);
                first
            }
            _ => noise,
        };
        self.hs = Some(pending);
        self.last_sent = Some(pkt.clone());
        self.phase = Phase::Sending;
        Ok(Action::Send(pkt))
    }

    /// The handshake packet just handed to the shell has been sent.
    pub fn sent(&mut self) -> io::Result<Action> {
        if self.phase != Phase::Sending {
            return self.fail(io::ErrorKind::InvalidInput, "no handshake packet is in flight");
        }
        let agreed = self.pending.take();
        self.noise = None;
        self.phase = if agreed.is_some() { Phase::Done } else { Phase::Waiting };
        self.invariants();
        Ok(match agreed {
            Some(agreed) => Action::Done(agreed),
            None => Action::Wait,
        })
    }

    pub fn recv(&mut self, pkt: &[u8]) -> io::Result<Action> {
        if let Some((kind, msg)) = &self.failure {
            let err = io::Error::new(*kind, msg.clone());
            self.invariants();
            return Err(err);
        }
        if self.seen.iter().any(|seen| seen == pkt) {
            return self.fail(io::ErrorKind::InvalidData, "replayed handshake packet");
        }
        if self.phase == Phase::Done {
            return self.fail(io::ErrorKind::InvalidData, "packet from another connection");
        }
        if self.phase != Phase::Waiting {
            return self.fail(io::ErrorKind::InvalidInput, "handshake is not waiting for a packet");
        }
        let kind = match &self.role {
            Role::SharedInit { .. } => 0u8,
            Role::UserInit { .. } => 1,
            Role::SharedResp { .. } => 2,
            Role::UserResp => 3,
        };
        let action = match kind {
            0 => self.recv_init(pkt, false)?,
            1 => self.recv_init(pkt, true)?,
            2 => self.recv_shared_resp(pkt)?,
            _ => self.recv_user_resp(pkt)?,
        };
        self.invariants();
        Ok(action)
    }

    fn recv_init(&mut self, pkt: &[u8], user: bool) -> io::Result<Action> {
        if !user && looks_like_transport(pkt) {
            return self.fail(io::ErrorKind::InvalidData, "packet from another connection");
        }
        let noise = if user {
            match pkt.first() {
                Some(1) => {
                    if &pkt[1..] == rargo_proto::REFUSED.as_bytes()
                        && matches!(self.role, Role::UserInit { marked: true, .. })
                    {
                        return self.fail(io::ErrorKind::Unsupported, "the server uses unmarked UDP hellos");
                    }
                    let why = String::from_utf8_lossy(&pkt[1..]).trim().to_string();
                    return self.fail(io::ErrorKind::PermissionDenied, format!("the server refused the connection: {why}"));
                }
                Some(0) => pkt[1..].to_vec(),
                _ => return self.fail(io::ErrorKind::UnexpectedEof, "truncated handshake packet"),
            }
        } else {
            pkt.to_vec()
        };
        if noise.len() < NOISE_LEN {
            return self.fail(io::ErrorKind::UnexpectedEof, "truncated handshake packet");
        }
        let hs = self.hs.take().expect("the initiator is mid-handshake");
        match hs.finish(&noise) {
            Ok(cipher) => {
                self.seen.push(pkt.to_vec());
                let binding = user.then(|| cipher.binding());
                self.phase = Phase::Done;
                Ok(Action::Done(Agreed { cipher, user: None, binding }))
            }
            Err(e) => self.fail(e.kind(), e.to_string()),
        }
    }

    fn recv_shared_resp(&mut self, pkt: &[u8]) -> io::Result<Action> {
        if looks_like_transport(pkt) {
            return self.fail(io::ErrorKind::InvalidData, "packet from another connection");
        }
        if pkt.len() < NOISE_LEN {
            return self.fail(io::ErrorKind::UnexpectedEof, "truncated handshake packet");
        }
        let psk = match &self.role {
            Role::SharedResp { psk } => handshake_key(psk),
            _ => panic!("shared responder"),
        };
        match link_accept_with(&psk, pkt, &self.ephemeral) {
            Ok((m2, cipher)) => {
                self.seen.push(pkt.to_vec());
                self.last_sent = Some(m2.clone());
                self.pending = Some(Agreed { cipher, user: None, binding: None });
                self.phase = Phase::Sending;
                Ok(Action::Send(m2))
            }
            Err(e) => self.fail(e.kind(), e.to_string()),
        }
    }

    fn recv_user_resp(&mut self, pkt: &[u8]) -> io::Result<Action> {
        if pkt.is_empty() {
            return self.fail(io::ErrorKind::Other, "empty key agreement");
        }
        let n = pkt[0] as usize;
        let Some(body) = pkt.get(1..1 + n) else {
            return self.fail(io::ErrorKind::Other, "malformed key agreement");
        };
        let Ok(name) = std::str::from_utf8(body) else {
            return self.fail(io::ErrorKind::Other, "malformed key agreement");
        };
        let body = &pkt[1 + n..];
        let noise = match body.len() {
            len if len < NOISE_LEN => return self.fail(io::ErrorKind::UnexpectedEof, "truncated handshake packet"),
            NOISE_LEN => return self.reject_protocol(rargo_proto::protocol_unmarked()),
            len if len == MAGIC.len() + NOISE_LEN && body[..5] == MAGIC[..5] && body[6..8] == MAGIC[6..8] => {
                if &body[..MAGIC.len()] != MAGIC {
                    return self.reject_protocol(rargo_proto::protocol_mismatch(u32::from(body[5])));
                }
                &body[MAGIC.len()..]
            }
            _ => return self.fail(io::ErrorKind::InvalidData, "malformed key agreement"),
        };
        self.seen.push(pkt.to_vec());
        self.name = Some(name.to_string());
        self.noise = Some(noise.to_vec());
        self.phase = Phase::NeedKey;
        Ok(Action::NeedKey(name.to_string()))
    }

    fn reject_protocol(&mut self, why: String) -> io::Result<Action> {
        assert!(self.notify.is_none(), "the first hello gets only one refusal");
        self.notify = Some(refusal(&why));
        self.fail(io::ErrorKind::InvalidData, why)
    }

    pub fn provide_key(&mut self, psk: Option<[u8; 32]>) -> io::Result<Action> {
        if self.phase != Phase::NeedKey {
            return self.fail(io::ErrorKind::InvalidInput, "handshake is not waiting for a user key");
        }
        let name = self.name.clone().expect("a user hello names its user");
        let noise = self.noise.take().expect("a user hello carries its key agreement");
        let Some(psk) = psk else {
            self.notify = Some(refusal(&format!("{}\n", rargo_proto::REFUSED)));
            return self.fail(io::ErrorKind::PermissionDenied, refused(&name));
        };
        match link_accept_with(&user_handshake_key(&psk), &noise, &self.ephemeral) {
            Ok((m2, cipher)) => {
                let binding = cipher.binding();
                let mut reply = vec![0u8];
                reply.extend_from_slice(&m2);
                self.last_sent = Some(reply.clone());
                self.pending = Some(Agreed { cipher, user: Some(name), binding: Some(binding) });
                self.phase = Phase::Sending;
                self.invariants();
                Ok(Action::Send(reply))
            }
            Err(_) => {
                self.notify = Some(refusal(&format!("{}\n", rargo_proto::REFUSED)));
                self.fail(io::ErrorKind::PermissionDenied, refused(&name))
            }
        }
    }

    /// The last handshake datagram, when it can be sent again.
    pub fn lost(&mut self) -> Option<Vec<u8>> {
        let pkt = if self.failure.is_some() || self.resends_left == 0 {
            None
        } else if let Some(pkt) = self.last_sent.clone() {
            self.resends_left -= 1;
            Some(pkt)
        } else {
            None
        };
        self.invariants();
        pkt
    }

    /// How many equal slices of the remaining wait the next receive gets.
    /// A datagram that can still be repeated splits the wait in two.
    pub fn resend_share(&self) -> u32 {
        if self.resends_left > 0 && self.last_sent.is_some() && self.phase == Phase::Waiting { 2 } else { 1 }
    }

    pub fn timeout_error(&self) -> io::Error {
        io::Error::new(io::ErrorKind::TimedOut, self.timeout)
    }

    pub fn take_notify(&mut self) -> Option<Vec<u8>> {
        let pkt = self.notify.take();
        self.invariants();
        pkt
    }

    fn fail(&mut self, kind: io::ErrorKind, msg: impl Into<String>) -> io::Result<Action> {
        let msg = msg.into();
        self.phase = Phase::Failed;
        self.pending = None;
        self.hs = None;
        self.failure = Some((kind, msg.clone()));
        self.invariants();
        Err(io::Error::new(kind, msg))
    }
}

impl StateMachine for Handshake {
    fn invariants(&self) {
        assert!(self.resends_left <= RESENDS, "a handshake repeats its datagram once");
        if self.last_sent.is_none() {
            assert_eq!(self.resends_left, RESENDS, "an unsent handshake has its resend");
        }
        for pkt in &self.seen {
            assert!(!pkt.is_empty(), "a consumed handshake packet has bytes");
        }
        match self.phase {
            Phase::Fresh => {
                assert!(self.last_sent.is_none());
                assert!(self.seen.is_empty());
                assert!(self.hs.is_none());
                assert!(self.pending.is_none());
                assert!(self.noise.is_none());
                assert!(self.failure.is_none());
            }
            Phase::Waiting => {
                assert!(self.pending.is_none());
                assert!(self.failure.is_none());
                match &self.role {
                    Role::SharedInit { .. } | Role::UserInit { .. } => {
                        assert!(self.last_sent.is_some(), "an initiator has sent its hello");
                        assert!(self.hs.is_some(), "an initiator still holds its handshake");
                    }
                    Role::SharedResp { .. } | Role::UserResp => {
                        assert!(self.last_sent.is_none());
                        assert!(self.hs.is_none());
                    }
                }
            }
            Phase::Sending => {
                assert!(self.last_sent.is_some());
                assert!(self.failure.is_none());
                match &self.role {
                    Role::SharedResp { .. } | Role::UserResp => assert!(self.pending.is_some(), "a reply finishes the handshake"),
                    Role::SharedInit { .. } | Role::UserInit { .. } => {
                        assert!(self.pending.is_none());
                        assert!(self.hs.is_some());
                    }
                }
            }
            Phase::NeedKey => {
                assert!(matches!(self.role, Role::UserResp));
                assert!(self.name.is_some());
                assert!(self.noise.as_ref().is_some_and(|n| n.len() >= NOISE_LEN));
                assert!(self.pending.is_none());
                assert!(self.hs.is_none());
                assert!(self.failure.is_none());
            }
            Phase::Done => {
                assert!(self.failure.is_none());
                assert!(self.hs.is_none());
                assert!(self.pending.is_none());
                assert!(self.noise.is_none());
                if matches!(self.role, Role::UserResp) {
                    assert!(self.name.is_some(), "a proven user has a name");
                }
            }
            Phase::Failed => {
                assert!(self.failure.is_some());
                assert!(self.pending.is_none());
                assert!(self.hs.is_none());
            }
        }
    }
}

fn refusal(text: &str) -> Vec<u8> {
    let mut pkt = vec![1u8];
    pkt.extend_from_slice(text.as_bytes());
    pkt
}

fn refused(name: &str) -> String {
    format!("refused token `{name}`: unknown, revoked, expired or a wrong secret")
}

fn looks_like_transport(pkt: &[u8]) -> bool {
    if pkt.len() < DATA_HDR || pkt.len() == NOISE_LEN {
        return false;
    }
    if pkt[0] != T_DATA && pkt[0] != T_ACK {
        return false;
    }
    let frag = u32::from_le_bytes(pkt[17..21].try_into().unwrap());
    let nfrags = u32::from_le_bytes(pkt[21..25].try_into().unwrap());
    nfrags >= 1 && frag < nfrags
}

#[cfg(test)]
mod tests {
    use super::*;
    use expect_test::expect;

    fn eph(b: u8) -> [u8; 32] {
        [b; 32]
    }

    fn outbound(action: Action) -> Vec<u8> {
        match action {
            Action::Send(pkt) => pkt,
            Action::Wait => panic!("expected a packet to send"),
            Action::NeedKey(name) => panic!("expected a packet to send, asked for {name}"),
            Action::Done(_) => panic!("expected a packet to send, handshake finished"),
        }
    }

    fn done(action: Action) -> Agreed {
        match action {
            Action::Done(agreed) => agreed,
            Action::Send(_) => panic!("expected the handshake to finish"),
            Action::Wait => panic!("expected the handshake to finish"),
            Action::NeedKey(name) => panic!("expected the handshake to finish, asked for {name}"),
        }
    }

    fn roundtrip(from: &LinkCipher, to: &LinkCipher, msg: &[u8]) {
        assert_eq!(to.open(&from.seal(msg).unwrap()).unwrap(), msg);
    }

    #[test]
    fn a_correct_key_agrees_and_carries_messages() {
        let key = [1u8; 32];
        let mut once = Handshake::shared_initiator(key, eph(1));
        let mut twice = Handshake::shared_initiator(key, eph(1));
        let first = outbound(once.start().unwrap());
        once.invariants();
        let second = outbound(twice.start().unwrap());
        twice.invariants();
        assert_eq!(first, second, "the same key and ephemeral produce the same hello");
        assert_eq!(first.len(), NOISE_LEN);

        let (client, server) = agree_shared(key, key, eph(3), eph(4));
        assert!(client.user.is_none());
        assert!(server.user.is_none());
        assert!(client.binding.is_none());
        assert!(server.binding.is_none());
        assert_eq!(client.cipher.binding(), server.cipher.binding());
        let big: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        roundtrip(&client.cipher, &server.cipher, &big);
        roundtrip(&server.cipher, &client.cipher, b"back");
    }

    fn agree_shared(key_i: [u8; 32], key_r: [u8; 32], ei: [u8; 32], er: [u8; 32]) -> (Agreed, Agreed) {
        let mut i = Handshake::shared_initiator(key_i, ei);
        let mut r = Handshake::shared_responder(key_r, er);
        i.invariants();
        r.invariants();
        let m1 = outbound(i.start().unwrap());
        i.invariants();
        assert!(matches!(i.sent().unwrap(), Action::Wait));
        i.invariants();
        assert!(matches!(r.start().unwrap(), Action::Wait));
        r.invariants();
        let m2 = outbound(r.recv(&m1).unwrap());
        r.invariants();
        let server = done(r.sent().unwrap());
        r.invariants();
        let client = done(i.recv(&m2).unwrap());
        i.invariants();
        (client, server)
    }

    #[test]
    fn a_correct_user_agrees_and_is_proven() {
        let (client, server, hello, _reply) = agree_user("ann", [7; 32], Some([7; 32]));
        let server = server.expect("ann");
        let client = client.expect("ann");
        assert_eq!(server.user.as_deref(), Some("ann"));
        assert!(client.user.is_none());
        assert_eq!(client.binding, server.binding);
        assert_eq!(client.binding.unwrap(), client.cipher.binding());
        let big: Vec<u8> = (0..200_000u32).map(|i| (i * 7) as u8).collect();
        roundtrip(&client.cipher, &server.cipher, &big);
        roundtrip(&server.cipher, &client.cipher, b"back");
        assert_eq!(hello[0] as usize, "ann".len());
        assert_eq!(&hello[1..4], b"ann");
    }

    #[test]
    fn the_first_udp_datagram_gets_an_upgrade_notice_when_its_protocol_is_old_or_unmarked() {
        let mut client = Handshake::user_initiator("ann", [7; 32], eph(1), true);
        let marked = outbound(client.start().unwrap());
        let mut old_server_refusal = vec![1];
        old_server_refusal.extend_from_slice(rargo_proto::REFUSED.as_bytes());
        client.sent().unwrap();
        assert_eq!(client.recv(&old_server_refusal).unwrap_err().kind(), io::ErrorKind::Unsupported);
        let mut legacy_client = Handshake::user_initiator("ann", [7; 32], eph(1), false);
        let unmarked = outbound(legacy_client.start().unwrap());
        assert_eq!(unmarked.len(), 4 + NOISE_LEN);
        assert_eq!(&unmarked[4..], &marked[4 + MAGIC.len()..]);

        // Protocols 7, 8 and the original protocol 9 had the same unmarked layout.
        for version in [None, Some(7), Some(8), Some(9)] {
            let bytes = match version {
                None => unmarked.clone(),
                Some(version) => {
                    let mut bytes = marked.clone();
                    bytes[4 + 5] = version;
                    bytes
                }
            };
            let mut server = Handshake::user_responder(eph(2));
            server.start().unwrap();
            assert!(server.recv(&bytes).is_err());
            let response = server.take_notify().unwrap();
            assert_eq!(response[0], 1);
            let why = std::str::from_utf8(&response[1..]).unwrap();
            assert!(why.contains(&format!("cargo install rargo --version {}", env!("CARGO_PKG_VERSION"))));
        }
    }

    fn agree_user(name: &str, psk: [u8; 32], known: Option<[u8; 32]>) -> (Result<Agreed, io::Error>, Result<Agreed, io::Error>, Vec<u8>, Option<Vec<u8>>) {
        let mut client = Handshake::user_initiator(name, psk, eph(1), true);
        let mut server = Handshake::user_responder(eph(2));
        client.invariants();
        server.invariants();
        let hello = outbound(client.start().unwrap());
        client.invariants();
        assert!(matches!(client.sent().unwrap(), Action::Wait));
        client.invariants();
        assert!(matches!(server.start().unwrap(), Action::Wait));
        server.invariants();
        let asked = match server.recv(&hello) {
            Ok(Action::NeedKey(asked)) => asked,
            other => panic!("the server asks for the user's key, got {other:?}"),
        };
        assert_eq!(asked, name);
        server.invariants();
        match server.provide_key(known) {
            Ok(action) => {
                let reply = outbound(action);
                server.invariants();
                assert_eq!(reply.first().copied(), Some(0));
                let server_agreed = done(server.sent().unwrap());
                server.invariants();
                let client_agreed = done(client.recv(&reply).unwrap());
                client.invariants();
                (Ok(client_agreed), Ok(server_agreed), hello, Some(reply))
            }
            Err(e) => {
                server.invariants();
                let notify = server.take_notify().expect("a refusal is sent");
                server.invariants();
                assert_eq!(notify[0], 1);
                assert_eq!(String::from_utf8_lossy(&notify[1..]).trim(), rargo_proto::REFUSED);
                let client_err = client.recv(&notify).unwrap_err();
                client.invariants();
                (Err(client_err), Err(e), hello, Some(notify))
            }
        }
    }

    #[test]
    fn a_wrong_key_is_refused() {
        let mut initiator = Handshake::shared_initiator([1; 32], eph(1));
        let mut responder = Handshake::shared_responder([2; 32], eph(2));
        let m1 = outbound(initiator.start().unwrap());
        assert!(matches!(initiator.sent().unwrap(), Action::Wait));
        assert!(matches!(responder.start().unwrap(), Action::Wait));
        let err = responder.recv(&m1).unwrap_err();
        responder.invariants();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(err.to_string(), "link key mismatch");

        let mut other = Handshake::shared_initiator([8; 32], eph(5));
        let _ = outbound(other.start().unwrap());
        assert!(matches!(other.sent().unwrap(), Action::Wait));
        let mut foreign = Handshake::shared_responder([9; 32], eph(6));
        assert!(matches!(foreign.start().unwrap(), Action::Wait));
        let mut peer = Handshake::shared_initiator([9; 32], eph(7));
        let m1 = outbound(peer.start().unwrap());
        let m2 = outbound(foreign.recv(&m1).unwrap());
        let err = other.recv(&m2).unwrap_err();
        other.invariants();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(err.to_string(), "link key mismatch");
    }

    #[test]
    fn a_wrong_user_is_refused() {
        for (name, psk, known) in [("ann", [8u8; 32], Some([7u8; 32])), ("eve", [7u8; 32], None)] {
            let (client, server, _, notify) = agree_user(name, psk, known);
            let err = server.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(err.to_string(), format!("refused token `{name}`: unknown, revoked, expired or a wrong secret"));
            let client = client.unwrap_err();
            assert_eq!(client.kind(), io::ErrorKind::PermissionDenied);
            let why = client.to_string();
            assert!(why.contains("the server refused the connection"), "{why}");
            assert!(why.contains(rargo_proto::REFUSED), "{why}");
            assert_eq!(notify.unwrap()[0], 1);
        }
    }

    #[test]
    fn a_replayed_packet_is_refused() {
        let mut initiator = Handshake::shared_initiator([1; 32], eph(1));
        let mut responder = Handshake::shared_responder([1; 32], eph(2));
        let m1 = outbound(initiator.start().unwrap());
        assert!(matches!(initiator.sent().unwrap(), Action::Wait));
        assert!(matches!(responder.start().unwrap(), Action::Wait));
        let m2 = outbound(responder.recv(&m1).unwrap());
        responder.invariants();
        let server = done(responder.sent().unwrap());
        responder.invariants();
        let err = responder.recv(&m1).unwrap_err();
        responder.invariants();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "replayed handshake packet");
        let _ = done(initiator.recv(&m2).unwrap());
        initiator.invariants();
        let err = initiator.recv(&m2).unwrap_err();
        initiator.invariants();
        assert_eq!(err.to_string(), "replayed handshake packet");
        let _ = server;

        let mut client = Handshake::user_initiator("ann", [7; 32], eph(3), true);
        let mut server = Handshake::user_responder(eph(4));
        let hello = outbound(client.start().unwrap());
        assert!(matches!(client.sent().unwrap(), Action::Wait));
        assert!(matches!(server.start().unwrap(), Action::Wait));
        assert!(matches!(server.recv(&hello).unwrap(), Action::NeedKey(_)));
        server.invariants();
        let err = server.recv(&hello).unwrap_err();
        server.invariants();
        assert_eq!(err.to_string(), "replayed handshake packet");
    }

    #[test]
    fn a_truncated_packet_is_refused() {
        let cases: [(Handshake, &[u8]); 6] = [
            (Handshake::shared_responder([1; 32], eph(1)), &[]),
            (Handshake::shared_responder([1; 32], eph(1)), &[0; 10]),
            (Handshake::shared_responder([1; 32], eph(1)), &[9; NOISE_LEN - 1]),
            (Handshake::user_responder(eph(3)), &[]),
            (Handshake::user_responder(eph(4)), &[5, b'a', b'b']),
            (Handshake::user_responder(eph(5)), &[2, b'a', b'n']),
        ];
        let mut trace = Vec::new();
        for (mut responder, packet) in cases {
            let start = responder.start().unwrap();
            let error = responder.recv(packet).unwrap_err();
            responder.invariants();
            trace.push(format!("{start:?} -> {:?}: {error}", error.kind()));
        }

        let mut shared = Handshake::shared_initiator([1; 32], eph(2));
        let mut user = Handshake::user_initiator("ann", [7; 32], eph(6), true);
        for (initiator, packet) in [(&mut shared, &[0; 8][..]), (&mut user, &[0])] {
            let start = initiator.start().unwrap();
            let sent = initiator.sent().unwrap();
            let error = initiator.recv(packet).unwrap_err();
            initiator.invariants();
            trace.push(format!("{start:?} -> {sent:?} -> {:?}: {error}", error.kind()));
        }

        expect![[r#"
            Wait -> UnexpectedEof: truncated handshake packet
            Wait -> UnexpectedEof: truncated handshake packet
            Wait -> UnexpectedEof: truncated handshake packet
            Wait -> Other: empty key agreement
            Wait -> Other: malformed key agreement
            Wait -> UnexpectedEof: truncated handshake packet
            Send(48) -> Wait -> UnexpectedEof: truncated handshake packet
            Send(60) -> Wait -> UnexpectedEof: truncated handshake packet"#]]
            .assert_eq(&trace.join("\n"));
    }

    #[test]
    fn a_packet_from_another_connection_is_refused() {
        let mut responder = Handshake::shared_responder([1; 32], eph(1));
        assert!(matches!(responder.start().unwrap(), Action::Wait));
        let mut frame = vec![T_DATA];
        frame.extend_from_slice(&7u64.to_le_bytes());
        frame.extend_from_slice(&99u64.to_le_bytes());
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame.extend_from_slice(&1u32.to_le_bytes());
        frame.extend_from_slice(b"forged");
        let err = responder.recv(&frame).unwrap_err();
        responder.invariants();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(err.to_string(), "packet from another connection");

        let mut a_i = Handshake::shared_initiator([4; 32], eph(2));
        let mut a_r = Handshake::shared_responder([4; 32], eph(3));
        let mut b_i = Handshake::shared_initiator([4; 32], eph(4));
        let mut b_r = Handshake::shared_responder([4; 32], eph(5));
        let m1a = outbound(a_i.start().unwrap());
        assert!(matches!(a_i.sent().unwrap(), Action::Wait));
        let m1b = outbound(b_i.start().unwrap());
        assert!(matches!(b_i.sent().unwrap(), Action::Wait));
        assert_ne!(m1a, m1b, "two connections do not share a handshake");
        assert!(matches!(a_r.start().unwrap(), Action::Wait));
        assert!(matches!(b_r.start().unwrap(), Action::Wait));
        let m2a = outbound(a_r.recv(&m1a).unwrap());
        let _ = done(a_r.sent().unwrap());
        let m2b = outbound(b_r.recv(&m1b).unwrap());
        let _ = done(b_r.sent().unwrap());
        let _ = done(a_i.recv(&m2a).unwrap());
        a_i.invariants();
        let err = a_i.recv(&m2b).unwrap_err();
        a_i.invariants();
        assert_eq!(err.to_string(), "packet from another connection");
        let err = a_r.recv(&m1b).unwrap_err();
        a_r.invariants();
        assert_eq!(err.to_string(), "packet from another connection");

        let mut ann = Handshake::user_initiator("ann", [7; 32], eph(6), true);
        let mut eve = Handshake::user_initiator("eve", [7; 32], eph(7), true);
        let mut server = Handshake::user_responder(eph(8));
        let hello = outbound(ann.start().unwrap());
        assert!(matches!(ann.sent().unwrap(), Action::Wait));
        let other = outbound(eve.start().unwrap());
        assert!(matches!(server.start().unwrap(), Action::Wait));
        assert!(matches!(server.recv(&hello).unwrap(), Action::NeedKey(_)));
        let reply = outbound(server.provide_key(Some([7; 32])).unwrap());
        let _ = done(server.sent().unwrap());
        server.invariants();
        let err = server.recv(&other).unwrap_err();
        server.invariants();
        assert_eq!(err.to_string(), "packet from another connection");
        let _ = reply;
    }

    #[test]
    fn a_lost_handshake_packet_is_retried() {
        let key = [3u8; 32];
        let mut initiator = Handshake::shared_initiator(key, eph(1));
        let mut responder = Handshake::shared_responder(key, eph(2));
        let m1 = outbound(initiator.start().unwrap());
        assert!(matches!(initiator.sent().unwrap(), Action::Wait));
        initiator.invariants();
        let again = initiator.lost().expect("a lost hello is sent again");
        initiator.invariants();
        assert_eq!(again, m1);
        assert!(initiator.lost().is_none(), "a hello is repeated once");
        initiator.invariants();
        assert!(matches!(responder.start().unwrap(), Action::Wait));
        let m2 = outbound(responder.recv(&again).unwrap());
        responder.invariants();
        let reply = responder.lost().expect("a lost reply is sent again");
        responder.invariants();
        assert_eq!(reply, m2);
        assert!(responder.lost().is_none());
        responder.invariants();
        let server = done(responder.sent().unwrap());
        responder.invariants();
        let client = done(initiator.recv(&reply).unwrap());
        initiator.invariants();
        roundtrip(&client.cipher, &server.cipher, b"after a loss");
        roundtrip(&server.cipher, &client.cipher, b"back");

        let mut user = Handshake::user_initiator("ann", [7; 32], eph(3), true);
        let mut server = Handshake::user_responder(eph(4));
        let hello = outbound(user.start().unwrap());
        assert!(matches!(user.sent().unwrap(), Action::Wait));
        let again = user.lost().unwrap();
        user.invariants();
        assert_eq!(again, hello);
        assert!(matches!(server.start().unwrap(), Action::Wait));
        assert!(matches!(server.recv(&again).unwrap(), Action::NeedKey(_)));
        let body = outbound(server.provide_key(Some([7; 32])).unwrap());
        let resent = server.lost().unwrap();
        server.invariants();
        assert_eq!(resent, body);
        let server = done(server.sent().unwrap());
        let client = done(user.recv(&resent).unwrap());
        user.invariants();
        assert_eq!(server.user.as_deref(), Some("ann"));
        assert_eq!(client.binding, server.binding);
    }
}
