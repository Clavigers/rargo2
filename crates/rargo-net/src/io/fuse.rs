use std::sync::Arc;

use fusewire::{Filesystem, Session, BUF_SIZE};

pub struct FuseDrive {
    reader: super::JoinHandle<()>,
    stop: bool,
}

impl FuseDrive {
    pub fn spawn<F>(session: &Arc<Session>, fs: &Arc<F>) -> Self
    where
        F: Filesystem + 'static,
    {
        let fd = session.device_fd();
        let (session, fs) = (Arc::clone(session), Arc::clone(fs));
        let reader = super::spawn(move || loop {
            match super::read_at(fd, 0, BUF_SIZE) {
                Ok(request) if request.is_empty() => break session.ended(),
                Ok(request) => {
                    let (session, fs) = (Arc::clone(&session), Arc::clone(&fs));
                    drop(super::spawn(move || answer(fd, &session, fs.as_ref(), &request)));
                }
                Err(e) if matches!(e.raw_os_error(), Some(libc::EINTR | libc::EAGAIN | libc::ENOENT)) => {}
                Err(_) => break session.ended(),
            }
        });
        Self { reader, stop: true }
    }

    pub fn detach(mut self) {
        self.stop = false;
    }
}

impl Drop for FuseDrive {
    fn drop(&mut self) {
        if self.stop {
            self.reader.abort();
        }
    }
}

fn answer<F: Filesystem>(fd: i32, session: &Session, fs: &F, request: &[u8]) {
    let replies = super::blocking(|| {
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
    });
    for reply in replies {
        if write_fuse(fd, &reply).is_err() {
            return session.ended();
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
