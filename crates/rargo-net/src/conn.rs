//! Reliable delivery for one UDP connection.
//!
//! The caller supplies the clock reading and, when a loss draw is needed, the
//! next jitter value. Each step takes a message to send, a datagram, or a
//! timer, and returns datagrams to transmit, messages in send order, and the
//! next deadline.

use rargo_machine::StateMachine;
use super::{
    DATA_HDR, FRAG, INIT_CWND, MAX_CWND, MAX_RTO, MIN_CWND, MIN_RTO, SEND_BUFFER, T_ACK, T_DATA,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

const REORDER: u64 = 3;
const LOSS_BETA: f64 = 0.7;
const GROWTH: f64 = 8.0;
const PACE_BURST: u32 = 16;
const _: () = assert!(LOSS_BETA > 0.0 && LOSS_BETA < 1.0);
const _: () = assert!(REORDER > 0);

struct Inflight {
    data: Arc<[u8]>,
    nfrags: u32,
    sent_at: i64,
    retries: u32,
    seq: u64,
}

struct SendState {
    next_msg: u64,
    queue: VecDeque<(u64, u32, u32, Arc<[u8]>)>,
    queued_bytes: usize,
    /// Ordered by message and fragment, so which due packet leaves first does not depend on a hash seed.
    inflight: BTreeMap<(u64, u32), Inflight>,
    cwnd: f64,
    ssthresh: f64,
    srtt: Option<Duration>,
    rttvar: Duration,
    last_cut: i64,
    min_rtt: Option<Duration>,
    next_send: i64,
    next_seq: u64,
    high_acked: u64,
}

struct Partial {
    frags: Vec<Option<Vec<u8>>>,
    have: u32,
}

struct RecvState {
    partial: BTreeMap<u64, Partial>,
    done_below: u64,
    /// Completed messages still waiting for every earlier id to complete.
    pending: BTreeMap<u64, Vec<u8>>,
}

pub enum Admit {
    Queued,
    Full,
    Closed,
}

#[derive(Clone, Debug)]
pub struct Packet {
    pub msg: u64,
    pub bytes: Vec<u8>,
    pub dropped: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Output {
    pub datagrams: Vec<Packet>,
    pub messages: Vec<Vec<u8>>,
    pub acks: Vec<(u64, u32)>,
    pub deadline: Option<Duration>,
    pub again: bool,
    pub space: bool,
    pub wake: bool,
    pub settled: bool,
    pub retransmits: u64,
    pub stopped: bool,
}

pub(crate) struct View {
    pub cwnd: f64,
    pub ssthresh: f64,
    pub inflight: usize,
    pub queued_bytes: usize,
    pub srtt: Option<Duration>,
    pub min_rtt: Option<Duration>,
}

pub struct ConnState {
    id: u64,
    send: SendState,
    recv: RecvState,
    loss: f32,
    closed: bool,
}

pub fn data_packet(id: u64, msg: u64, frag: u32, nfrags: u32, data: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(DATA_HDR + data.len());
    pkt.push(T_DATA);
    pkt.extend_from_slice(&id.to_le_bytes());
    pkt.extend_from_slice(&msg.to_le_bytes());
    pkt.extend_from_slice(&frag.to_le_bytes());
    pkt.extend_from_slice(&nfrags.to_le_bytes());
    pkt.extend_from_slice(data);
    pkt
}

pub fn ack_packet(id: u64, entries: &[(u64, u32)]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(9 + entries.len() * 12);
    pkt.push(T_ACK);
    pkt.extend_from_slice(&id.to_le_bytes());
    for (msg, frag) in entries {
        pkt.extend_from_slice(&msg.to_le_bytes());
        pkt.extend_from_slice(&frag.to_le_bytes());
    }
    pkt
}

fn nsec(d: Duration) -> i64 {
    i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
}

fn since(earlier: i64, later: i64) -> Duration {
    if later <= earlier {
        Duration::ZERO
    } else {
        Duration::from_nanos((later - earlier) as u64)
    }
}

fn shift(t: i64, d: Duration) -> i64 {
    t.saturating_add(nsec(d))
}

fn as_dur(t: i64) -> Duration {
    if t <= 0 {
        Duration::ZERO
    } else {
        Duration::from_nanos(t as u64)
    }
}

impl ConnState {
    pub fn new(id: u64) -> ConnState {
        let s = ConnState {
            id,
            send: SendState {
                next_msg: 1,
                queue: VecDeque::new(),
                queued_bytes: 0,
                inflight: BTreeMap::new(),
                cwnd: INIT_CWND,
                ssthresh: MAX_CWND,
                srtt: None,
                rttvar: Duration::from_millis(50),
                last_cut: 0,
                min_rtt: None,
                next_send: 0,
                next_seq: 1,
                high_acked: 0,
            },
            recv: RecvState {
                partial: BTreeMap::new(),
                done_below: 1,
                pending: BTreeMap::new(),
            },
            loss: 0.0,
            closed: false,
        };
        s.invariants();
        s
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.invariants();
    }

    pub fn set_loss(&mut self, fraction: f32) {
        self.loss = fraction;
        self.invariants();
    }

    pub fn send_idle(&self) -> bool {
        self.send.queue.is_empty() && self.send.inflight.is_empty()
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.send.srtt
    }

    pub(crate) fn view(&self) -> View {
        View {
            cwnd: self.send.cwnd,
            ssthresh: self.send.ssthresh,
            inflight: self.send.inflight.len(),
            queued_bytes: self.send.queued_bytes,
            srtt: self.send.srtt,
            min_rtt: self.send.min_rtt,
        }
    }

    pub fn send(&mut self, msg: &[u8]) -> Admit {
        if self.closed {
            self.invariants();
            return Admit::Closed;
        }
        let s = &mut self.send;
        if s.queued_bytes >= SEND_BUFFER && !s.queue.is_empty() {
            self.invariants();
            return Admit::Full;
        }
        let id = s.next_msg;
        assert!(id >= 1);
        s.next_msg += 1;
        let n = msg.len().div_ceil(FRAG).max(1) as u32;
        let queue_before = s.queue.len();
        let mut cut = 0usize;
        for i in 0..n {
            let a = i as usize * FRAG;
            let b = (a + FRAG).min(msg.len());
            assert!(a <= b);
            assert!(b - a <= FRAG);
            cut += b - a;
            s.queue.push_back((id, i, n, Arc::from(&msg[a..b])));
        }
        assert_eq!(cut, msg.len(), "fragments cover the message exactly");
        assert_eq!(s.queue.len() - queue_before, n as usize);
        s.queued_bytes += msg.len();
        self.invariants();
        Admit::Queued
    }

    pub fn on_datagram(&mut self, now: Duration, bytes: &[u8]) -> Output {
        let now = nsec(now);
        let mut out = Output::default();
        if bytes.len() < 9 || u64::from_le_bytes(bytes[1..9].try_into().unwrap()) != self.id {
            self.invariants();
            return out;
        }
        match bytes[0] {
            T_DATA if bytes.len() >= DATA_HDR => {
                let msg = u64::from_le_bytes(bytes[9..17].try_into().unwrap());
                let frag = u32::from_le_bytes(bytes[17..21].try_into().unwrap());
                let nfrags = u32::from_le_bytes(bytes[21..25].try_into().unwrap());
                self.on_data(msg, frag, nfrags, &bytes[DATA_HDR..], &mut out);
            }
            T_ACK => {
                let mut rest = &bytes[9..];
                let mut saw = false;
                while rest.len() >= 12 {
                    let msg = u64::from_le_bytes(rest[..8].try_into().unwrap());
                    let frag = u32::from_le_bytes(rest[8..12].try_into().unwrap());
                    self.on_ack(now, msg, frag);
                    saw = true;
                    rest = &rest[12..];
                }
                if saw {
                    out.wake = true;
                    out.settled = self.send.queue.is_empty() && self.send.inflight.is_empty();
                }
            }
            _ => {}
        }
        self.invariants();
        out
    }

    pub fn on_timer(&mut self, now: Duration, jitter: &mut dyn FnMut() -> u64) -> Output {
        let now = nsec(now);
        if self.closed {
            self.invariants();
            return Output {
                stopped: true,
                ..Output::default()
            };
        }
        let mut out = Output::default();
        let mut resend: Vec<(u64, u32, u32, Arc<[u8]>)> = Vec::new();
        let mut batch: Vec<(u64, u32, u32, Arc<[u8]>)> = Vec::new();
        let mut pace_until: Option<i64> = None;
        let s = &mut self.send;
        let rto = Self::rto(s);
        let gain = if s.cwnd < s.ssthresh { 2.0 } else { 1.25 };
        let per_sec = s.cwnd * gain / s.srtt.map_or(0.05, |d| d.as_secs_f64()).max(0.0005);
        let interval = Duration::from_secs_f64(1.0 / per_sec);
        let burst = ((per_sec * 0.002) as u32).max(PACE_BURST);
        let earliest = now.saturating_sub(nsec(interval.saturating_mul(burst)));
        if s.next_send < earliest {
            s.next_send = earliest;
        }
        let budget = if now >= s.next_send {
            (since(s.next_send, now).as_secs_f64() * per_sec) as u32 + 1
        } else {
            0
        };
        let budget = budget.min(burst);
        let mut more_due = false;
        let mut timed_out = false;
        let mut fast_lost = false;
        let mut soonest: Option<i64> = None;
        let high_acked = s.high_acked;
        let mut seq = s.next_seq;
        for ((m, f), inf) in s.inflight.iter_mut() {
            let due = shift(inf.sent_at, rto * (1 << inf.retries.min(5)));
            let overtaken = high_acked >= inf.seq + REORDER;
            if (overtaken || due <= now) && resend.len() as u32 >= budget {
                more_due = true;
            } else if overtaken || due <= now {
                inf.sent_at = now;
                inf.retries += 1;
                inf.seq = seq;
                seq += 1;
                resend.push((*m, *f, inf.nfrags, inf.data.clone()));
                if overtaken {
                    fast_lost = true;
                } else {
                    timed_out = true;
                }
            } else {
                soonest = Some(soonest.map_or(due, |x| x.min(due)));
            }
        }
        s.next_seq = seq;
        if (timed_out || fast_lost)
            && since(s.last_cut, now) > s.srtt.unwrap_or(Duration::from_millis(100))
        {
            let beta = if timed_out { 0.5 } else { LOSS_BETA };
            s.ssthresh = (s.cwnd * beta).max(MIN_CWND);
            s.cwnd = s.ssthresh;
            s.last_cut = now;
            assert!(s.cwnd >= MIN_CWND);
        }
        let budget = budget.saturating_sub(resend.len() as u32);
        let mut sent_new = 0u32;
        while (s.inflight.len() as f64) < s.cwnd {
            if sent_new >= budget {
                if !s.queue.is_empty() {
                    pace_until = Some(shift(
                        s.next_send,
                        interval * (sent_new + resend.len() as u32 + 1),
                    ));
                }
                break;
            }
            sent_new += 1;
            let Some((m, f, n, data)) = s.queue.pop_front() else {
                break;
            };
            assert!(f < n);
            assert!(
                s.queued_bytes >= data.len(),
                "queued_bytes counts every queued fragment"
            );
            s.queued_bytes -= data.len();
            let seq = s.next_seq;
            s.next_seq += 1;
            let previous = s.inflight.insert(
                (m, f),
                Inflight {
                    data: data.clone(),
                    nfrags: n,
                    sent_at: now,
                    retries: 0,
                    seq,
                },
            );
            assert!(
                previous.is_none(),
                "a queued fragment is not in flight already"
            );
            batch.push((m, f, n, data));
        }
        assert!(s.next_seq > s.high_acked);
        if s.queue.is_empty() {
            assert_eq!(s.queued_bytes, 0);
        }
        let sent = sent_new.min(batch.len() as u32) + resend.len() as u32;
        s.next_send = shift(s.next_send, interval * sent);
        if more_due {
            pace_until = Some(s.next_send);
        }
        let next_deadline = soonest.or_else(|| (!s.inflight.is_empty()).then(|| shift(now, rto)));
        out.retransmits = resend.len() as u64;
        out.space = !batch.is_empty();
        for (m, f, n, data) in resend.into_iter().chain(batch) {
            out.datagrams.push(self.frame(m, f, n, &data, jitter));
        }
        out.again = !out.datagrams.is_empty() && pace_until.is_none();
        out.deadline = match (next_deadline, pace_until) {
            (Some(a), Some(b)) => Some(as_dur(a.min(b))),
            (a, b) => a.or(b).map(as_dur),
        };
        self.invariants();
        out
    }

    fn rto(s: &SendState) -> Duration {
        let rto = match s.srtt {
            Some(srtt) => (srtt + (4 * s.rttvar).max(srtt / 2).max(Duration::from_millis(5)))
                .clamp(MIN_RTO, MAX_RTO),
            None => Duration::from_millis(200),
        };
        assert!(rto >= MIN_RTO);
        assert!(rto <= MAX_RTO);
        rto
    }

    fn frame(
        &self,
        msg: u64,
        frag: u32,
        nfrags: u32,
        data: &[u8],
        jitter: &mut dyn FnMut() -> u64,
    ) -> Packet {
        assert!(frag < nfrags);
        assert!(data.len() <= FRAG);
        assert!(msg >= 1);
        let bytes = data_packet(self.id, msg, frag, nfrags, data);
        assert_eq!(bytes.len(), DATA_HDR + data.len());
        let dropped = self.loss > 0.0 && (jitter() % 10_000) as f32 / 10_000.0 < self.loss;
        Packet {
            msg,
            bytes,
            dropped,
        }
    }

    fn on_ack(&mut self, now: i64, msg: u64, frag: u32) {
        let s = &mut self.send;
        if let Some(inf) = s.inflight.remove(&(msg, frag)) {
            assert!(inf.seq < s.next_seq, "acknowledged seq was handed out");
            s.high_acked = s.high_acked.max(inf.seq);
            if inf.retries == 0 {
                let sample = since(inf.sent_at, now);
                match s.srtt {
                    None => {
                        s.srtt = Some(sample);
                        s.rttvar = sample / 2;
                    }
                    Some(srtt) => {
                        let diff = if sample > srtt {
                            sample - srtt
                        } else {
                            srtt - sample
                        };
                        s.rttvar = (s.rttvar * 3 + diff) / 4;
                        s.srtt = Some((srtt * 7 + sample) / 8);
                    }
                }
                let min = s.min_rtt.map_or(sample, |m| m.min(sample));
                s.min_rtt = Some(min);
                if s.cwnd < s.ssthresh
                    && s.cwnd >= 2.0 * INIT_CWND
                    && sample > min + (min / 8).max(Duration::from_millis(2))
                {
                    s.ssthresh = s.cwnd;
                }
            }
            if s.cwnd < s.ssthresh {
                s.cwnd += 1.0;
            } else {
                s.cwnd += GROWTH.max(s.cwnd * 0.05) / s.cwnd;
            }
            s.cwnd = s.cwnd.min(MAX_CWND);
            assert!(s.cwnd >= MIN_CWND);
            assert!(s.cwnd <= MAX_CWND);
        }
    }

    fn on_data(&mut self, msg: u64, frag: u32, nfrags: u32, data: &[u8], out: &mut Output) {
        if msg < self.recv.done_below || self.recv.pending.contains_key(&msg) {
            out.acks.push((msg, frag));
            return;
        }
        if nfrags == 0 {
            return;
        }
        let p = self.recv.partial.entry(msg).or_insert_with(|| Partial {
            frags: vec![None; nfrags as usize],
            have: 0,
        });
        if (frag as usize) >= p.frags.len() {
            return;
        }
        if p.frags[frag as usize].is_none() {
            p.frags[frag as usize] = Some(data.to_vec());
            p.have += 1;
        }
        assert!(p.have as usize <= p.frags.len());
        if p.have as usize == p.frags.len() {
            let p = self.recv.partial.remove(&msg).unwrap();
            let mut whole = Vec::with_capacity(
                p.frags
                    .iter()
                    .map(|f| f.as_ref().map_or(0, |v| v.len()))
                    .sum(),
            );
            for f in p.frags {
                whole.extend_from_slice(&f.unwrap());
            }
            assert!(
                self.recv.pending.insert(msg, whole).is_none(),
                "a message is delivered once"
            );
            self.drain(out);
        }
        out.acks.push((msg, frag));
    }

    fn drain(&mut self, out: &mut Output) {
        while let Some(whole) = self.recv.pending.remove(&self.recv.done_below) {
            out.messages.push(whole);
            self.recv.done_below += 1;
        }
    }
}

impl StateMachine for ConnState {
    fn invariants(&self) {
        let s = &self.send;
        assert!(
            s.cwnd >= MIN_CWND && s.cwnd <= MAX_CWND,
            "the window stays inside its bounds"
        );
        assert!(
            s.ssthresh >= MIN_CWND && s.ssthresh <= MAX_CWND,
            "the slow-start threshold stays inside its bounds"
        );
        assert!(s.next_msg >= 1, "message ids start at 1");
        assert!(
            s.next_seq > s.high_acked,
            "an acknowledgement names a sequence that was handed out"
        );
        let queued: usize = s.queue.iter().map(|(_, _, _, data)| data.len()).sum();
        assert_eq!(
            s.queued_bytes, queued,
            "queued_bytes counts every queued fragment"
        );
        if s.queue.is_empty() {
            assert_eq!(s.queued_bytes, 0);
        }
        for (&(msg, frag), inf) in &s.inflight {
            assert!(msg >= 1);
            assert!(frag < inf.nfrags);
            assert!(inf.data.len() <= FRAG);
            assert!(inf.seq < s.next_seq, "an in-flight sequence was handed out");
        }
        let rto = Self::rto(s);
        assert!(rto >= MIN_RTO && rto <= MAX_RTO);
        assert!(self.recv.done_below >= 1);
        for (&msg, part) in &self.recv.partial {
            assert!(
                msg >= self.recv.done_below,
                "a partial message is still due"
            );
            assert!(
                !self.recv.pending.contains_key(&msg),
                "a partial message is not waiting to be delivered"
            );
            assert_eq!(
                part.have as usize,
                part.frags.iter().filter(|f| f.is_some()).count()
            );
            assert!(part.have as usize <= part.frags.len());
            assert!(
                part.have as usize != part.frags.len(),
                "a complete message leaves the partial map"
            );
        }
        for &msg in self.recv.pending.keys() {
            assert!(
                msg > self.recv.done_below,
                "a complete message is delivered as soon as every earlier one is"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use expect_test::expect;

    fn spin(s: &mut ConnState, now: Duration) -> Output {
        let mut acc = Output::default();
        for _ in 0..8 {
            let out = s.on_timer(now, &mut || {
                panic!("jitter is drawn only for a loss fraction")
            });
            s.invariants();
            let again = out.again;
            acc.datagrams.extend(out.datagrams);
            acc.retransmits += out.retransmits;
            acc.messages.extend(out.messages);
            acc.acks.extend(out.acks);
            acc.space |= out.space;
            acc.wake |= out.wake;
            acc.settled |= out.settled;
            acc.stopped = out.stopped;
            acc.deadline = out.deadline;
            acc.again = again;
            if !again {
                break;
            }
        }
        acc
    }

    fn wire(out: &Output) -> Vec<Vec<u8>> {
        out.datagrams
            .iter()
            .filter(|p| !p.dropped)
            .map(|p| p.bytes.clone())
            .collect()
    }

    #[test]
    fn retransmission_backs_off() {
        let mut s = ConnState::new(4);
        assert!(matches!(s.send(b"lost"), Admit::Queued));
        s.invariants();
        let mut t = Duration::ZERO;
        let mut trace = Vec::new();
        for _ in 0..5 {
            let out = spin(&mut s, t);
            let payloads: Vec<_> = out.datagrams.iter()
                .filter(|packet| !packet.dropped)
                .map(|packet| std::str::from_utf8(&packet.bytes[DATA_HDR..]).unwrap())
                .collect();
            trace.push(format!("{t:?}: {payloads:?}, {} retransmits", out.retransmits));
            t = out.deadline.expect("the fragment stays in flight");
        }
        expect![[r#"
            0ns: ["lost"], 0 retransmits
            200ms: ["lost"], 1 retransmits
            600ms: ["lost"], 1 retransmits
            1.4s: ["lost"], 1 retransmits
            3s: ["lost"], 1 retransmits"#]].assert_eq(&trace.join("\n"));
    }

    #[test]
    fn a_lost_acknowledgement_is_recovered() {
        let mut a = ConnState::new(9);
        let mut b = ConnState::new(9);
        assert!(matches!(a.send(b"ping"), Admit::Queued));
        let first = spin(&mut a, Duration::ZERO);
        let pkt = wire(&first).pop().expect("the fragment is transmitted");
        let got = b.on_datagram(Duration::from_millis(5), &pkt);
        b.invariants();
        expect!["messages=[[112, 105, 110, 103]], acks=[(1, 0)]"]
            .assert_eq(&format!("messages={:?}, acks={:?}", got.messages, got.acks));
        let t = first
            .deadline
            .expect("the sender waits for the acknowledgement");
        let again = spin(&mut a, t);
        assert!(
            again.retransmits >= 1,
            "a missing acknowledgement is retransmitted"
        );
        let pkt = wire(&again).pop().expect("the fragment is sent again");
        let dup = b.on_datagram(t, &pkt);
        b.invariants();
        expect!["messages=[], acks=[(1, 0)]"]
            .assert_eq(&format!("messages={:?}, acks={:?}", dup.messages, dup.acks));
        let back = a.on_datagram(t + Duration::from_millis(5), &ack_packet(9, &dup.acks));
        a.invariants();
        assert!(back.wake);
        assert!(back.settled);
        assert!(
            a.send_idle(),
            "the retransmission's acknowledgement clears the window"
        );
    }

    #[test]
    fn a_closed_connection_stops() {
        let mut s = ConnState::new(1);
        assert!(matches!(s.send(b"z"), Admit::Queued));
        s.close();
        s.invariants();
        assert!(matches!(s.send(b"y"), Admit::Closed));
        let out = spin(&mut s, Duration::from_secs(1));
        let again = s.on_timer(Duration::from_secs(2), &mut || 0);
        s.invariants();
        expect![[r#"
            Output { datagrams: [], messages: [], acks: [], deadline: None, again: false, space: false, wake: false, settled: false, retransmits: 0, stopped: true }
            Output { datagrams: [], messages: [], acks: [], deadline: None, again: false, space: false, wake: false, settled: false, retransmits: 0, stopped: true }"#]]
            .assert_eq(&format!("{out:?}\n{again:?}"));
    }

    #[test]
    fn a_loss_draw_comes_from_the_caller() {
        let mut trace = Vec::new();
        for (loss, draw) in [(1.0, 0), (0.5, 6_000), (0.0, 0)] {
            let mut s = ConnState::new(3);
            s.set_loss(loss);
            assert!(matches!(s.send(b"x"), Admit::Queued));
            let mut draws = 0;
            let out = s.on_timer(Duration::ZERO, &mut || {
                draws += 1;
                draw
            });
            s.invariants();
            let dropped: Vec<_> = out.datagrams.iter().map(|packet| packet.dropped).collect();
            trace.push(format!("loss={loss}, draws={draws}, dropped={dropped:?}"));
        }
        expect![[r#"
            loss=1, draws=1, dropped=[true]
            loss=0.5, draws=1, dropped=[false]
            loss=0, draws=0, dropped=[false]"#]].assert_eq(&trace.join("\n"));
    }

    fn payloads(tag: u8) -> Vec<Vec<u8>> {
        [0usize, 1, 17, FRAG, FRAG + 1, FRAG * 2 + 100]
            .into_iter()
            .map(|n| (0..n).map(|i| tag.wrapping_add(i as u8)).collect())
            .collect()
    }

    /// Drop, duplicate, and extra delay for packet `n`. The same `n` always gets the same fate.
    fn fate(n: u64) -> (bool, bool, u64) {
        (n % 7 == 0, n % 5 == 0, (n * 3) % 11)
    }

    struct Flight {
        at: Duration,
        to_b: bool,
        bytes: Vec<u8>,
        n: u64,
    }

    fn exchange(seed: u64) {
        let mut a = ConnState::new(11);
        let mut b = ConnState::new(11);
        let expect_b = payloads(1);
        let expect_a = payloads(2);
        for m in &expect_b {
            assert!(matches!(a.send(m), Admit::Queued));
            a.invariants();
        }
        for m in &expect_a {
            assert!(matches!(b.send(m), Admit::Queued));
            b.invariants();
        }
        let mut q: Vec<Flight> = Vec::new();
        let mut n = seed;
        let mut got_a = Vec::new();
        let mut got_b = Vec::new();
        let mut t = Duration::ZERO;
        for _ in 0..200_000 {
            if got_a == expect_a
                && got_b == expect_b
                && a.send_idle()
                && b.send_idle()
                && q.is_empty()
            {
                return;
            }
            let mut deadline_a = None;
            let mut deadline_b = None;
            for (state, to_b, deadline) in [
                (&mut a, true, &mut deadline_a),
                (&mut b, false, &mut deadline_b),
            ] {
                let out = spin(state, t);
                *deadline = out.deadline;
                for bytes in wire(&out) {
                    n += 1;
                    let (drop, dup, extra) = fate(n);
                    if drop {
                        continue;
                    }
                    let at = t + Duration::from_millis(1 + extra);
                    q.push(Flight {
                        at,
                        to_b,
                        bytes: bytes.clone(),
                        n,
                    });
                    if dup {
                        q.push(Flight {
                            at: at + Duration::from_millis(3),
                            to_b,
                            bytes,
                            n: n + 10_000,
                        });
                    }
                }
            }
            let mut due: Vec<Flight> = Vec::new();
            let mut later: Vec<Flight> = Vec::new();
            for f in q.drain(..) {
                if f.at <= t {
                    due.push(f);
                } else {
                    later.push(f);
                }
            }
            due.sort_by(|x, y| x.at.cmp(&y.at).then(x.n.cmp(&y.n)));
            q = later;
            if due.is_empty() {
                let packet_at = q.iter().map(|f| f.at).min();
                let timer_at = [deadline_a, deadline_b]
                    .into_iter()
                    .flatten()
                    .filter(|d| *d > t)
                    .min();
                let Some(next) = packet_at.into_iter().chain(timer_at).min() else {
                    break;
                };
                t = next;
                continue;
            }
            for f in due {
                let state = if f.to_b { &mut b } else { &mut a };
                let out = state.on_datagram(t, &f.bytes);
                state.invariants();
                if f.to_b {
                    got_b.extend(out.messages);
                } else {
                    got_a.extend(out.messages);
                }
                if !out.acks.is_empty() {
                    n += 1;
                    let (drop, dup, extra) = fate(n);
                    if !drop {
                        let bytes = ack_packet(11, &out.acks);
                        let at = t + Duration::from_millis(1 + extra);
                        q.push(Flight {
                            at,
                            to_b: !f.to_b,
                            bytes: bytes.clone(),
                            n,
                        });
                        if dup {
                            q.push(Flight {
                                at: at + Duration::from_millis(3),
                                to_b: !f.to_b,
                                bytes,
                                n: n + 10_000,
                            });
                        }
                    }
                }
            }
        }
        panic!(
            "seed {seed} delivered {} and {} of {} by {t:?}",
            got_b.len(),
            got_a.len(),
            expect_a.len()
        );
    }

    #[test]
    fn delivery_is_exact_and_in_order_under_a_generated_schedule() {
        for seed in 1..6 {
            exchange(seed);
        }
    }
}
