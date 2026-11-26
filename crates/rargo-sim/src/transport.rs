use rargo_net::sans_io::{
    Action, Admit, ConnState, EndpointState, HANDSHAKE_WAIT, Handshake, LinkKey, RouteEvent,
    ack_packet, data_packet, seal,
};
use rargo_proto::secure::LinkCipher;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const PSK: [u8; 32] = [37; 32];

pub(crate) fn key() -> LinkKey {
    Some(Arc::new(PSK))
}

pub(crate) struct Side {
    conn: ConnState,
    handshake: Handshake,
    cipher: Option<LinkCipher>,
    handshake_deadline: Duration,
    handshake_wake: Option<Duration>,
    received_handshake: Vec<Vec<u8>>,
    pending: VecDeque<Vec<u8>>,
    sealed: Option<Vec<u8>>,
}

#[derive(Default)]
pub(crate) struct Effects {
    pub packets: Vec<Vec<u8>>,
    pub messages: Vec<Vec<u8>>,
    pub wake: Option<u64>,
    pub retransmits: u64,
}

impl Side {
    pub fn new(id: u64, initiator: bool, now: u64) -> io::Result<Self> {
        let mut ephemeral = [0; 32];
        ephemeral[1..9].copy_from_slice(&id.to_le_bytes());
        ephemeral[9] = u8::from(initiator);
        let handshake = if initiator {
            Handshake::shared_initiator(PSK, ephemeral)
        } else {
            Handshake::shared_responder(PSK, ephemeral)
        };
        let mut side = Self {
            conn: ConnState::new(id),
            handshake,
            cipher: None,
            handshake_deadline: Duration::from_millis(now) + HANDSHAKE_WAIT,
            handshake_wake: None,
            received_handshake: Vec::new(),
            pending: VecDeque::new(),
            sealed: None,
        };
        let action = side.handshake.start()?;
        side.advance(action, Duration::from_millis(now))?;
        Ok(side)
    }

    pub fn agreed(&self) -> bool {
        self.cipher.is_some()
    }

    pub fn send(&mut self, msg: Vec<u8>) {
        self.pending.push_back(msg);
    }

    fn advance(&mut self, mut action: Action, now: Duration) -> io::Result<()> {
        loop {
            match action {
                Action::Send(bytes) => {
                    self.agreement(&bytes)?;
                    action = self.handshake.sent()?;
                }
                Action::Wait => {
                    let left = self.handshake_deadline.saturating_sub(now);
                    let slice = (left / self.handshake.resend_share())
                        .max(Duration::from_millis(1))
                        .min(left);
                    self.handshake_wake = Some(now + slice);
                    return Ok(());
                }
                Action::Done(agreed) => {
                    self.cipher = Some(agreed.cipher);
                    self.handshake_wake = None;
                    return Ok(());
                }
                Action::NeedKey(_) => unreachable!("simulator uses a shared key"),
            }
        }
    }

    fn agreement(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self.conn.send(bytes) {
            Admit::Queued => Ok(()),
            _ => Err(io::Error::other("could not queue link agreement")),
        }
    }

    pub fn route(
        &mut self,
        endpoint: &mut EndpointState,
        event: RouteEvent,
        from: SocketAddr,
        now: u64,
        effects: &mut Effects,
    ) -> io::Result<()> {
        let packet = match &event {
            RouteEvent::Data {
                id,
                msg,
                frag,
                nfrags,
                payload,
            } => data_packet(*id, *msg, *frag, *nfrags, payload),
            RouteEvent::Ack { id, msg, frag } => ack_packet(*id, &[(*msg, *frag)]),
            RouteEvent::Open { .. } | RouteEvent::Peer { .. } => return Ok(()),
        };
        let now = Duration::from_millis(now);
        let out = self.conn.on_datagram(now, &packet);
        if let RouteEvent::Data { id, .. } = event {
            for (msg, frag) in out.acks {
                endpoint.ack(id, msg, frag, from);
            }
        }
        for bytes in out.messages {
            if self.received_handshake.contains(&bytes) {
                continue;
            }
            if let Some(cipher) = &self.cipher {
                effects.messages.push(cipher.open(&bytes)?);
            } else {
                let action = self.handshake.recv(&bytes)?;
                self.received_handshake.push(bytes);
                self.advance(action, now)?;
            }
        }
        Ok(())
    }

    pub fn drive(&mut self, key: &LinkKey, at: u64, effects: &mut Effects) -> io::Result<()> {
        let now = Duration::from_millis(at);
        if self.handshake_wake.is_some_and(|wake| wake <= now) {
            let Some(bytes) = self.handshake.lost() else {
                return Err(self.handshake.timeout_error());
            };
            self.agreement(&bytes)?;
            self.advance(Action::Wait, now)?;
        }
        loop {
            if let Some(cipher) = &self.cipher {
                while let Some(bytes) = self.pending.front() {
                    if self.sealed.is_none() {
                        self.sealed = Some(cipher.seal(bytes)?);
                    }
                    match self
                        .conn
                        .send(self.sealed.as_deref().expect("a sealed record"))
                    {
                        Admit::Queued => {
                            self.pending.pop_front();
                            self.sealed = None;
                        }
                        Admit::Full => break,
                        Admit::Closed => return Err(io::Error::other("link closed")),
                    }
                }
            }
            let out = self.conn.on_timer(now, &mut || 0);
            effects.retransmits += out.retransmits;
            for packet in out.datagrams {
                let mut bytes = packet.bytes;
                seal(key, &mut bytes);
                effects.packets.push(bytes);
            }
            if !out.again {
                effects.wake = out
                    .deadline
                    .into_iter()
                    .chain(self.handshake_wake)
                    .min()
                    .map(|deadline| (deadline.as_nanos().div_ceil(1_000_000) as u64).max(at + 1));
                return Ok(());
            }
        }
    }
}
