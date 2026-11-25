use std::sync::{mpsc, Arc, Mutex as StdMutex};

use fusewire::{Filesystem, Session, BUF_SIZE};

pub struct FuseDrive {
    reader: super::JoinHandle<()>,
    writer: super::JoinHandle<()>,
    stop: bool,
}

impl FuseDrive {
    pub fn spawn<F>(session: &Arc<Session>, fs: &Arc<F>) -> Self
    where
        F: Filesystem + 'static,
    {
        let fd = session.device_fd();
        let (jobs, taken) = mpsc::channel::<Vec<u8>>();
        let taken = Arc::new(StdMutex::new(taken));
        let (replies, mut answered) = super::unbounded_channel::<(Vec<Vec<u8>>, bool)>();
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).clamp(8, 32);
        for i in 0..threads {
            let (session, fs, taken, replies) = (Arc::clone(session), Arc::clone(fs), Arc::clone(&taken), replies.clone());
            std::thread::Builder::new()
                .name(format!("fuse-{i}"))
                .spawn(move || handle(&session, fs.as_ref(), &taken, &replies))
                .expect("a fuse thread starts");
        }
        let reading = Arc::clone(session);
        let reader = super::spawn(move || loop {
            match super::read_at(fd, 0, BUF_SIZE) {
                Ok(request) if request.is_empty() => break reading.ended(),
                Ok(request) => {
                    if jobs.send(request).is_err() {
                        break;
                    }
                }
                Err(e) if matches!(e.raw_os_error(), Some(libc::EINTR | libc::EAGAIN | libc::ENOENT)) => {}
                Err(_) => break reading.ended(),
            }
        });
        let writing = Arc::clone(session);
        let writer = super::spawn(move || {
            while let Some((bytes, go_on)) = answered.recv() {
                for reply in bytes {
                    if write_fuse(fd, &reply).is_err() {
                        return writing.ended();
                    }
                }
                if !go_on {
                    return;
                }
            }
        });
        Self { reader, writer, stop: true }
    }

    pub fn detach(mut self) {
        self.stop = false;
    }
}

impl Drop for FuseDrive {
    fn drop(&mut self) {
        if self.stop {
            self.reader.abort();
            self.writer.abort();
        }
    }
}

fn handle<F: Filesystem>(
    session: &Session,
    fs: &F,
    taken: &StdMutex<mpsc::Receiver<Vec<u8>>>,
    replies: &super::UnboundedSender<(Vec<Vec<u8>>, bool)>,
) {
    let mut handler = session.handler();
    let mut aligned: Vec<u64> = vec![0; BUF_SIZE / 8];
    loop {
        let next = taken.lock().unwrap_or_else(|e| e.into_inner()).recv();
        let Ok(request) = next else { return };
        let n = request.len().min(BUF_SIZE);
        let request = unsafe {
            let dst = std::slice::from_raw_parts_mut(aligned.as_mut_ptr() as *mut u8, BUF_SIZE);
            dst[..n].copy_from_slice(&request[..n]);
            std::slice::from_raw_parts(aligned.as_ptr() as *const u8, n)
        };
        let mut out = Vec::new();
        let go_on = session.respond(&mut handler, fs, request, |bytes: &[u8]| {
            out.push(bytes.to_vec());
            Ok(())
        });
        if replies.send((out, go_on)).is_err() {
            return;
        }
    }
}

fn write_fuse(fd: i32, bytes: &[u8]) -> std::io::Result<()> {
    let mut off = 0;
    while off < bytes.len() {
        let n = super::write_at(fd, 0, &bytes[off..])?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "a fuse write wrote nothing"));
        }
        off += n;
    }
    Ok(())
}
