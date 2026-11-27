use std::io;
use std::rc::Rc;
use std::sync::Arc;

use io_uring::{opcode, squeue::Entry128, types};

use super::fiber::op_event;
use super::ops::{Event, Op};

use fusewire::abi::{
    FUSE_BATCH_FORGET, FUSE_DESTROY, FUSE_FORGET, FUSE_INTERRUPT, FUSE_NOTIFY_REPLY,
};
use fusewire::{Filesystem, Session, BUF_SIZE};

/// Where the handlers of one mounted filesystem run.
trait Serve: Clone + 'static {
    fn request(&self, session: &Session, buffer: &mut RingBuffer) -> io::Result<Option<u64>>;
    fn control(&self, session: &Session, request: &[u8]) -> Vec<Vec<u8>>;
}

/// Handlers run in the fiber that took the request, on the loop thread.
struct OnLoop<F>(Rc<F>);

impl<F> Clone for OnLoop<F> {
    fn clone(&self) -> Self {
        OnLoop(Rc::clone(&self.0))
    }
}

impl<F: Filesystem + 'static> Serve for OnLoop<F> {
    fn request(&self, session: &Session, buffer: &mut RingBuffer) -> io::Result<Option<u64>> {
        buffer.reply(session, self.0.as_ref())
    }

    fn control(&self, session: &Session, request: &[u8]) -> Vec<Vec<u8>> {
        replies(session, self.0.as_ref(), request)
    }
}

/// Handlers run on a blocking pool thread while the fiber waits.
struct OnPool<F>(Arc<F>);

impl<F> Clone for OnPool<F> {
    fn clone(&self) -> Self {
        OnPool(Arc::clone(&self.0))
    }
}

impl<F: Filesystem + Send + Sync + 'static> Serve for OnPool<F> {
    fn request(&self, session: &Session, buffer: &mut RingBuffer) -> io::Result<Option<u64>> {
        super::blocking(|| buffer.reply(session, self.0.as_ref()))
    }

    fn control(&self, session: &Session, request: &[u8]) -> Vec<Vec<u8>> {
        super::blocking(|| replies(session, self.0.as_ref(), request))
    }
}

pub struct FuseDrive {
    readers: Vec<super::JoinHandle<()>>,
    stop: bool,
}

impl FuseDrive {
    /// Serves `session` from `fs` on the loop thread: each kernel queue has a
    /// fiber that takes a request, runs its handler and commits the reply.
    pub fn spawn<F>(session: &Arc<Session>, fs: &Rc<F>) -> io::Result<Self>
    where
        F: Filesystem + 'static,
    {
        Self::start(session, OnLoop(Rc::clone(fs)))
    }

    /// Serves `session` from `fs` with every handler run on a blocking pool thread.
    pub fn spawn_threaded<F>(session: &Arc<Session>, fs: &Arc<F>) -> io::Result<Self>
    where
        F: Filesystem + Send + Sync + 'static,
    {
        Self::start(session, OnPool(Arc::clone(fs)))
    }

    fn start<S: Serve>(session: &Arc<Session>, serve: S) -> io::Result<Self> {
        let queues = possible_queues()?;
        let mut readers = Vec::with_capacity(usize::from(queues) + 1);
        for queue in 0..queues {
            let (session, serve) = (Arc::clone(session), serve.clone());
            readers.push(super::spawn(move || serve_queue(session, serve, queue)));
        }

        // Linux still delivers FORGET, INTERRUPT and other control traffic here.
        let fd = session.device_fd();
        let session = Arc::clone(session);
        let reader = super::spawn(move || loop {
            match super::read_at(fd, 0, BUF_SIZE) {
                Ok(request) if request.is_empty() => break session.ended(),
                Ok(request) => {
                    let opcode = request
                        .get(4..8)
                        .map(|bytes| u32::from_ne_bytes(bytes.try_into().unwrap()));
                    if !matches!(
                        opcode,
                        Some(
                            FUSE_FORGET
                                | FUSE_BATCH_FORGET
                                | FUSE_INTERRUPT
                                | FUSE_DESTROY
                                | FUSE_NOTIFY_REPLY
                        )
                    ) {
                        eprintln!("error: FUSE-over-io_uring received an unexpected opcode on /dev/fuse: {opcode:?}");
                        std::process::exit(101);
                    }
                    let (session, serve) = (Arc::clone(&session), serve.clone());
                    drop(super::spawn(move || answer(fd, &session, &serve, &request)));
                }
                Err(e)
                    if matches!(
                        e.raw_os_error(),
                        Some(libc::EINTR | libc::EAGAIN | libc::ENOENT)
                    ) => {}
                Err(_) => break session.ended(),
            }
        });
        readers.push(reader);
        Ok(Self {
            readers,
            stop: true,
        })
    }

    pub fn detach(mut self) {
        self.stop = false;
    }
}

impl Drop for FuseDrive {
    fn drop(&mut self) {
        if self.stop {
            for reader in &self.readers {
                reader.abort();
            }
        }
    }
}

fn possible_queues() -> io::Result<u16> {
    // The kernel sizes its queue array by num_possible_cpus(), regardless of
    // the CPUs available to the daemon under its affinity or cgroup settings.
    let possible = super::file::read_to_string("/sys/devices/system/cpu/possible")?;
    possible.trim().split(',').try_fold(0u16, |count, range| {
        let invalid = || io::Error::other("invalid possible CPU list for FUSE-over-io_uring");
        let (first, last) = range.split_once('-').unwrap_or((range, range));
        let first: u16 = first.parse().map_err(|_| invalid())?;
        let last: u16 = last.parse().map_err(|_| invalid())?;
        last.checked_sub(first)
            .and_then(|span| count.checked_add(span)?.checked_add(1))
            .ok_or_else(invalid)
    })
}

const INPUT_HEADER: usize = std::mem::size_of::<fusewire::abi::fuse_in_header>();
const OUTPUT_HEADER: usize = std::mem::size_of::<fusewire::abi::fuse_out_header>();
const HEADER_REGION: usize = 128;
const METADATA: usize = 2 * HEADER_REGION;
const FUSE_IO_URING_CMD_REGISTER: u32 = 1;
const FUSE_IO_URING_CMD_COMMIT_AND_FETCH: u32 = 2;

pub(super) struct RingBuffer {
    header: [u8; METADATA + 32],
    payload: Vec<u8>,
    // Two native iovec entries, each an address followed by a length.
    iov: [usize; 4],
}

impl RingBuffer {
    fn new() -> Self {
        Self {
            header: [0; METADATA + 32],
            payload: vec![0; BUF_SIZE],
            iov: [0; 4],
        }
    }

    pub(super) fn command(&mut self, fd: i32, queue: u16, commit: Option<u64>) -> Entry128 {
        let mut cmd = [0; 80];
        cmd[8..16].copy_from_slice(&commit.unwrap_or_default().to_ne_bytes());
        cmd[16..18].copy_from_slice(&queue.to_ne_bytes());
        let opcode = if commit.is_some() {
            FUSE_IO_URING_CMD_COMMIT_AND_FETCH
        } else {
            FUSE_IO_URING_CMD_REGISTER
        };
        let mut command = opcode::UringCmd80::new(types::Fd(fd), opcode).cmd(cmd);
        if commit.is_none() {
            self.iov = [
                self.header.as_mut_ptr() as usize,
                self.header.len(),
                self.payload.as_mut_ptr() as usize,
                self.payload.len(),
            ];
            command = command.addr(Some(self.iov.as_ptr() as u64));
        }
        let mut entry = command.build();
        if commit.is_none() {
            // io-uring 0.7 does not expose URING_CMD's len field. Entry128
            // is repr(C), begins with a Linux SQE, and len is at byte 24.
            unsafe { (&raw mut entry).cast::<u8>().add(24).cast::<u32>().write(2) };
        }
        entry
    }

    fn reply<F: Filesystem>(&mut self, session: &Session, fs: &F) -> io::Result<Option<u64>> {
        let total = u32::from_ne_bytes(self.header[..4].try_into().unwrap()) as usize;
        let payload = u32::from_ne_bytes(
            self.header[METADATA + 16..METADATA + 20]
                .try_into()
                .unwrap(),
        ) as usize;
        let commit =
            u64::from_ne_bytes(self.header[METADATA + 8..METADATA + 16].try_into().unwrap());
        let args = total
            .checked_sub(INPUT_HEADER + payload)
            .filter(|&len| len <= HEADER_REGION && payload <= self.payload.len())
            .ok_or_else(|| io::Error::other("invalid FUSE-over-io_uring request length"))?;

        let mut aligned = vec![0u64; total.div_ceil(8)];
        let request =
            unsafe { std::slice::from_raw_parts_mut(aligned.as_mut_ptr().cast::<u8>(), total) };
        request[..INPUT_HEADER].copy_from_slice(&self.header[..INPUT_HEADER]);
        request[INPUT_HEADER..INPUT_HEADER + args]
            .copy_from_slice(&self.header[HEADER_REGION..HEADER_REGION + args]);
        request[INPUT_HEADER + args..].copy_from_slice(&self.payload[..payload]);
        let mut replied = false;
        let active = session.respond(&mut session.handler(), fs, request, |bytes| {
            let (header, body) = bytes.split_at(OUTPUT_HEADER);
            if body.len() > self.payload.len() {
                return Err(io::Error::other("FUSE-over-io_uring reply is too large"));
            }
            self.header[..OUTPUT_HEADER].copy_from_slice(header);
            self.header[METADATA + 16..METADATA + 20]
                .copy_from_slice(&(body.len() as u32).to_ne_bytes());
            self.payload[..body.len()].copy_from_slice(body);
            replied = true;
            Ok(())
        });
        if replied {
            Ok(Some(commit))
        } else if !active {
            Ok(None)
        } else {
            Err(io::Error::other("FUSE-over-io_uring request had no reply"))
        }
    }
}

fn serve_queue<S: Serve>(session: Arc<Session>, serve: S, queue: u16) {
    let fd = session.device_fd();
    let mut commit = None;
    let mut buffer = Box::new(RingBuffer::new());
    loop {
        buffer = match op_event(Op::Fuse {
            fd,
            queue,
            commit,
            buffer,
        }) {
            Ok(Event::Fuse(buffer)) => buffer,
            Ok(_) => unreachable!("FUSE completion returns its buffer"),
            Err(err)
                if matches!(
                    err.raw_os_error(),
                    Some(libc::ENOTCONN | libc::ECONNABORTED | libc::ECANCELED)
                ) =>
            {
                break
            }
            Err(err) => {
                eprintln!("error: FUSE-over-io_uring command failed: {err}");
                std::process::exit(101);
            }
        };
        let next = serve.request(&session, &mut buffer);
        match next {
            Ok(Some(id)) => commit = Some(id),
            Ok(None) => break,
            Err(err) => {
                eprintln!("error: {err}");
                std::process::exit(101);
            }
        }
    }
    session.ended();
}

fn answer<S: Serve>(fd: i32, session: &Session, serve: &S, request: &[u8]) {
    for reply in serve.control(session, request) {
        if write_fuse(fd, &reply).is_err() {
            return session.ended();
        }
    }
}

fn replies<F: Filesystem>(session: &Session, fs: &F, request: &[u8]) -> Vec<Vec<u8>> {
    let mut aligned: Vec<u64> = vec![0; request.len().div_ceil(8)];
    let request = unsafe {
        let dst = std::slice::from_raw_parts_mut(aligned.as_mut_ptr() as *mut u8, request.len());
        dst.copy_from_slice(request);
        std::slice::from_raw_parts(aligned.as_ptr() as *const u8, request.len())
    };
    let mut out = Vec::new();
    session.respond(&mut session.handler(), fs, request, |bytes: &[u8]| {
        out.push(bytes.to_vec());
        Ok(())
    });
    out
}

fn write_fuse(fd: i32, bytes: &[u8]) -> std::io::Result<()> {
    let mut off = 0;
    while off < bytes.len() {
        let n = super::write_at(fd, 0, &bytes[off..])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "a fuse write wrote nothing",
            ));
        }
        off += n;
    }
    Ok(())
}
