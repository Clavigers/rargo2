//! Packet routing for one UDP socket. The shell reads and writes that socket.
//! This type decides which connection a datagram belongs to, and which datagrams to send back.

use super::{AGREEMENT, DATA_HDR, LinkKey, T_ACK, T_DATA, TAG, open, seal};
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

pub struct EndpointState {
    accepting: bool,
    per_conn: bool,
    key: LinkKey,
    conns: BTreeMap<u64, Route>,
    forgotten: BTreeSet<u64>,
    acks: BTreeMap<u64, PendingAck>,
}

struct Route {
    peer: SocketAddr,
    tag: Option<[u8; 32]>,
    last_heard: u64,
}

struct PendingAck {
    to: SocketAddr,
    body: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RouteEvent {
    Open { id: u64, from: SocketAddr },
    Peer { id: u64, from: SocketAddr },
    Data { id: u64, msg: u64, frag: u32, nfrags: u32, payload: Vec<u8> },
    Ack { id: u64, msg: u64, frag: u32 },
}

pub struct Routed {
    pub id: Option<u64>,
    pub len: usize,
    pub events: Vec<RouteEvent>,
}

impl EndpointState {
    pub fn new(accepting: bool, per_conn: bool, key: LinkKey) -> EndpointState {
        let s = EndpointState {
            accepting,
            per_conn,
            key,
            conns: BTreeMap::new(),
            forgotten: BTreeSet::new(),
            acks: BTreeMap::new(),
        };
        s.invariants();
        s
    }

    pub fn connect(&mut self, id: u64, peer: SocketAddr, now: u64) -> bool {
        if self.conns.contains_key(&id) || self.forgotten.contains(&id) {
            self.invariants();
            return false;
        }
        self.conns.insert(id, Route { peer, tag: None, last_heard: now });
        self.invariants();
        true
    }

    pub fn set_tag(&mut self, id: u64, tag: [u8; 32]) {
        if let Some(c) = self.conns.get_mut(&id) {
            c.tag = Some(tag);
        }
        self.invariants();
    }

    pub fn forget(&mut self, id: u64) {
        self.conns.remove(&id);
        self.acks.remove(&id);
        self.forgotten.insert(id);
        self.invariants();
    }

    #[cfg(test)]
    fn peer(&self, id: u64) -> Option<SocketAddr> {
        self.conns.get(&id).map(|c| c.peer)
    }

    #[cfg(test)]
    fn last_heard(&self, id: u64) -> Option<u64> {
        self.conns.get(&id).map(|c| c.last_heard)
    }

    /// One datagram. `now` is the shell's clock reading for this arrival.
    pub fn recv(&mut self, from: SocketAddr, bytes: &[u8], now: u64) -> Routed {
        let out = self.route(from, bytes, now);
        self.invariants();
        out
    }

    pub fn ack(&mut self, id: u64, msg: u64, frag: u32, to: SocketAddr) {
        let entry = self.acks.entry(id).or_insert_with(|| {
            let mut body = vec![T_ACK];
            body.extend_from_slice(&id.to_le_bytes());
            PendingAck { to, body }
        });
        entry.body.extend_from_slice(&msg.to_le_bytes());
        entry.body.extend_from_slice(&frag.to_le_bytes());
        self.invariants();
    }

    pub fn ack_full(&self) -> bool {
        self.acks.values().any(|a| a.body.len() > 1100)
    }

    pub fn take_datagrams(&mut self) -> Vec<(SocketAddr, Vec<u8>)> {
        let mut out = Vec::new();
        for (id, ack) in std::mem::take(&mut self.acks) {
            let mut pkt = ack.body;
            seal(&self.key, &mut pkt);
            if self.per_conn {
                if let Some(k) = self.conns.get(&id).and_then(|c| c.tag) {
                    let tag = blake3::keyed_hash(&k, &pkt);
                    pkt.extend_from_slice(&tag.as_bytes()[..TAG]);
                }
            }
            out.push((ack.to, pkt));
        }
        self.invariants();
        out
    }

    pub fn invariants(&self) {
        for id in &self.forgotten {
            assert!(!self.conns.contains_key(id), "a forgotten connection is not routed");
            assert!(!self.acks.contains_key(id), "a forgotten connection has no acknowledgement waiting");
        }
        for (id, ack) in &self.acks {
            assert!(self.conns.contains_key(id), "an acknowledgement names a live connection");
            assert_eq!(ack.body.first().copied(), Some(T_ACK), "an acknowledgement starts with its type");
            assert!(ack.body.len() >= 9, "an acknowledgement carries its connection id");
            assert_eq!(ack.body.len() % 12, 9, "an acknowledgement is a header plus whole entries");
        }
    }

    fn route(&mut self, from: SocketAddr, bytes: &[u8], now: u64) -> Routed {
        let Some((pkt, proven)) = self.admit(bytes) else {
            return Routed { id: None, len: 0, events: Vec::new() };
        };
        if pkt.len() < 9 {
            return Routed { id: None, len: 0, events: Vec::new() };
        }
        let id = u64::from_le_bytes(pkt[1..9].try_into().unwrap());
        let first_msg = (pkt.len() >= 17).then(|| u64::from_le_bytes(pkt[9..17].try_into().unwrap()));
        if !proven && pkt[0] == T_DATA && first_msg != Some(AGREEMENT) {
            return Routed { id: None, len: 0, events: Vec::new() };
        }
        if self.forgotten.contains(&id) {
            return Routed { id: None, len: 0, events: Vec::new() };
        }
        let mut events = Vec::new();
        if !self.conns.contains_key(&id) {
            if !(self.accepting && pkt[0] == T_DATA) {
                return Routed { id: None, len: 0, events: Vec::new() };
            }
            self.conns.insert(id, Route { peer: from, tag: None, last_heard: now });
            events.push(RouteEvent::Open { id, from });
        }
        if proven {
            let c = self.conns.get_mut(&id).expect("a routed packet names a live connection");
            c.peer = from;
            c.last_heard = now;
            events.push(RouteEvent::Peer { id, from });
        }
        let len = pkt.len();
        match pkt[0] {
            T_DATA if pkt.len() >= DATA_HDR => {
                let msg = u64::from_le_bytes(pkt[9..17].try_into().unwrap());
                let frag = u32::from_le_bytes(pkt[17..21].try_into().unwrap());
                let nfrags = u32::from_le_bytes(pkt[21..25].try_into().unwrap());
                events.push(RouteEvent::Data { id, msg, frag, nfrags, payload: pkt[DATA_HDR..].to_vec() });
            }
            T_ACK => {
                let mut rest = &pkt[9..];
                while rest.len() >= 12 {
                    let msg = u64::from_le_bytes(rest[..8].try_into().unwrap());
                    let frag = u32::from_le_bytes(rest[8..12].try_into().unwrap());
                    if proven || msg == AGREEMENT {
                        events.push(RouteEvent::Ack { id, msg, frag });
                    }
                    rest = &rest[12..];
                }
            }
            _ => {}
        }
        Routed { id: Some(id), len, events }
    }

    fn admit<'a>(&self, pkt: &'a [u8]) -> Option<(&'a [u8], bool)> {
        if !self.per_conn {
            return open(&self.key, pkt).map(|p| (p, true));
        }
        if pkt.len() >= 9 + TAG {
            let id = u64::from_le_bytes(pkt[1..9].try_into().unwrap());
            if let Some(k) = self.conns.get(&id).and_then(|c| c.tag) {
                let (body, tag) = pkt.split_at(pkt.len() - TAG);
                let want = blake3::keyed_hash(&k, body);
                let diff = want.as_bytes()[..TAG].iter().zip(tag).fold(0u8, |d, (a, b)| d | (a ^ b));
                if diff == 0 {
                    return Some((body, true));
                }
            }
        }
        Some((pkt, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn at(port: u16) -> SocketAddr {
        SocketAddr::from(([10, 1, 0, 1], port))
    }

    fn data(id: u64, msg: u64, payload: &[u8]) -> Vec<u8> {
        let mut pkt = vec![T_DATA];
        pkt.extend_from_slice(&id.to_le_bytes());
        pkt.extend_from_slice(&msg.to_le_bytes());
        pkt.extend_from_slice(&0u32.to_le_bytes());
        pkt.extend_from_slice(&1u32.to_le_bytes());
        pkt.extend_from_slice(payload);
        pkt
    }

    fn with_tag(mut pkt: Vec<u8>, key: &[u8; 32]) -> Vec<u8> {
        let tag = blake3::keyed_hash(key, &pkt);
        pkt.extend_from_slice(&tag.as_bytes()[..TAG]);
        pkt
    }

    fn feed(s: &mut EndpointState, from: SocketAddr, bytes: &[u8], now: u64) -> Routed {
        let out = s.recv(from, bytes, now);
        s.invariants();
        out
    }

    #[test]
    fn packets_reach_the_right_connection() {
        let mut s = EndpointState::new(false, false, None);
        assert!(s.connect(1, at(1), 0));
        assert!(s.connect(3, at(2), 0));
        s.invariants();
        let a = feed(&mut s, at(1), &data(1, 4, b"one"), 1);
        let b = feed(&mut s, at(2), &data(3, 4, b"two"), 2);
        match &a.events[..] {
            [RouteEvent::Peer { id: 1, .. }, RouteEvent::Data { id: 1, payload, .. }] => assert_eq!(payload, b"one"),
            other => panic!("connection 1 got {other:?}"),
        }
        match &b.events[..] {
            [RouteEvent::Peer { id: 3, .. }, RouteEvent::Data { id: 3, payload, .. }] => assert_eq!(payload, b"two"),
            other => panic!("connection 3 got {other:?}"),
        }
        s.ack(1, 4, 0, at(9));
        s.ack(1, 5, 1, at(8));
        s.ack(3, 4, 0, at(7));
        s.invariants();
        let sent = s.take_datagrams();
        s.invariants();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, at(9), "the first acknowledgement keeps its destination");
        assert_eq!(sent[1].0, at(7));
        assert!(sent[0].1.windows(8).any(|w| w == 4u64.to_le_bytes()));
        assert!(sent[0].1.windows(8).any(|w| w == 5u64.to_le_bytes()));
        assert!(!sent[0].1.windows(8).any(|w| w == 3u64.to_le_bytes()));
    }

    #[test]
    fn a_spoofed_or_unproven_packet_is_dropped() {
        let key = Arc::new([9u8; 32]);
        let mut shared = EndpointState::new(true, false, Some(Arc::clone(&key)));
        assert!(shared.connect(1, at(1), 0));
        let mut bad = data(1, 4, b"forged");
        super::seal(&Some(Arc::new([8u8; 32])), &mut bad);
        let dropped = feed(&mut shared, at(4), &bad, 1);
        assert!(dropped.events.is_empty());
        assert_eq!(dropped.id, None);
        assert_eq!(shared.peer(1), Some(at(1)));
        assert_eq!(shared.last_heard(1), Some(0));

        let mut per = EndpointState::new(true, true, None);
        assert!(per.connect(1, at(1), 0));
        per.set_tag(1, [3u8; 32]);
        per.invariants();
        let spoof = with_tag(data(1, 4, b"forged"), &[4u8; 32]);
        let dropped = feed(&mut per, at(4), &spoof, 2);
        assert!(dropped.events.is_empty());
        assert_eq!(per.peer(1), Some(at(1)));
        assert_eq!(per.last_heard(1), Some(0));
        let bare = data(1, 4, b"forged");
        let dropped = feed(&mut per, at(5), &bare, 3);
        assert!(dropped.events.is_empty());
        assert_eq!(per.peer(1), Some(at(1)));
    }

    #[test]
    fn an_address_change_is_followed_only_after_proof() {
        let mut s = EndpointState::new(true, true, None);
        assert!(s.connect(1, at(1), 0));
        let tag = [6u8; 32];
        s.set_tag(1, tag);
        let handshake = data(1, AGREEMENT, b"hi");
        let early = feed(&mut s, at(2), &handshake, 5);
        assert!(early.events.iter().any(|e| matches!(e, RouteEvent::Data { msg: AGREEMENT, .. })));
        assert!(!early.events.iter().any(|e| matches!(e, RouteEvent::Peer { .. })));
        assert_eq!(s.peer(1), Some(at(1)));
        assert_eq!(s.last_heard(1), Some(0));

        let moved = with_tag(data(1, 4, b"there"), &tag);
        let got = feed(&mut s, at(2), &moved, 9);
        assert!(got.events.iter().any(|e| matches!(e, RouteEvent::Peer { id: 1, from } if *from == at(2))));
        assert!(got.events.iter().any(|e| matches!(e, RouteEvent::Data { payload, .. } if payload == b"there")));
        assert_eq!(s.peer(1), Some(at(2)));
        assert_eq!(s.last_heard(1), Some(9));
    }

    #[test]
    fn a_forgotten_connections_late_packets_are_ignored() {
        let mut s = EndpointState::new(true, false, None);
        assert!(s.connect(1, at(1), 0));
        s.forget(1);
        s.invariants();
        assert!(!s.connect(1, at(1), 1), "a forgotten id is not opened again");
        let late = feed(&mut s, at(3), &data(1, AGREEMENT, b"again"), 4);
        assert!(late.events.is_empty());
        assert_eq!(late.id, None);
        assert_eq!(s.peer(1), None);
        let fresh = feed(&mut s, at(3), &data(5, AGREEMENT, b"new"), 6);
        assert!(fresh.events.iter().any(|e| matches!(e, RouteEvent::Open { id: 5, .. })));
    }

    #[test]
    fn many_connections_stay_independent() {
        let mut s = EndpointState::new(false, false, None);
        let mut keys = Vec::new();
        for i in 0..24u64 {
            let id = i * 2 + 1;
            keys.push(id);
            assert!(s.connect(id, at(1000 + i as u16), i));
            s.invariants();
        }
        for (n, id) in keys.iter().enumerate() {
            let payload = [n as u8];
            let from = at(2000 + n as u16);
            let got = feed(&mut s, from, &data(*id, 4, &payload), 100 + n as u64);
            let data: Vec<_> = got
                .events
                .iter()
                .filter_map(|e| match e {
                    RouteEvent::Data { id: got_id, payload, .. } => Some((*got_id, payload.clone())),
                    _ => None,
                })
                .collect();
            assert_eq!(data, vec![(*id, payload.to_vec())]);
            assert_eq!(s.peer(*id), Some(from));
            s.ack(*id, 4, 0, from);
            s.invariants();
        }
        let sent = s.take_datagrams();
        s.invariants();
        assert_eq!(sent.len(), keys.len());
        for (i, id) in keys.iter().enumerate() {
            assert_eq!(sent[i].0, at(2000 + i as u16));
            assert!(sent[i].1.windows(8).any(|w| w == id.to_le_bytes()));
        }
    }
}
