use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr::{self};

use async_io::poll::{Events, Poller};
use async_io::socket::Addr;
use io_uring_callback::{Fd, IoHandle, IoUringArray, IoUringCell};
use memoffset::offset_of;

use super::ConnectedStream;
use crate::prelude::*;
use crate::runtime::Runtime;
use crate::util::CircularBuf;

impl<A: Addr + 'static, R: Runtime> ConnectedStream<A, R> {
    pub async fn write(self: &Arc<Self>, buf: &[u8]) -> Result<usize> {
        if buf.len() == 0 {
            return Ok(0);
        }

        // Initialize the poller only when needed
        let mut poller = None;
        loop {
            // Attempt to write
            let res = self.try_write(buf);
            if !res.has_errno(EAGAIN) {
                return res;
            }

            // Wait for interesting events by polling
            if poller.is_none() {
                poller = Some(Poller::new());
            }
            let mask = Events::OUT;
            let events = self.common.pollee().poll_by(mask, poller.as_mut());
            if events.is_empty() {
                poller.as_ref().unwrap().wait().await;
            }
        }
    }

    fn try_write(self: &Arc<Self>, buf: &[u8]) -> Result<usize> {
        let mut inner = self.sender.inner.lock().unwrap();

        // Check for error condition before write.
        //
        // Case 1. If the write side of the connection has been shutdown...
        if inner.is_shutdown {
            return_errno!(EPIPE, "write side is shutdown");
        }
        // Case 2. If the connenction has been broken...
        if let Some(errno) = inner.fatal {
            return_errno!(errno, "write failed");
        }

        let nbytes = inner.send_buf.produce(buf);

        if inner.send_buf.is_full() {
            // Mark the socket as non-writable
            self.common.pollee().del_events(Events::OUT);
        }

        // Since the send buffer is not empty, we can try to flush the buffer
        if inner.io_handle.is_none() {
            self.do_send(&mut inner);
        }

        if nbytes > 0 {
            Ok(nbytes)
        } else {
            return_errno!(EAGAIN, "try write again");
        }
    }

    fn do_send(self: &Arc<Self>, inner: &mut MutexGuard<Inner>) {
        debug_assert!(!inner.send_buf.is_empty());
        debug_assert!(!inner.is_shutdown);
        debug_assert!(inner.io_handle.is_none());

        // Init the callback invoked upon the completion of the async send
        let stream = self.clone();
        let complete_fn = move |retval: i32| {
            let mut inner = stream.sender.inner.lock().unwrap();

            // Release the handle to the async send
            inner.io_handle.take();

            // Handle error
            if retval < 0 {
                // TODO: guard against Iago attack through errno
                // TODO: should we ignore EINTR and try again?
                let errno = Errno::from(-retval as u32);
                inner.fatal = Some(errno);
                stream.common.pollee().add_events(Events::ERR);
                return;
            }
            assert!(retval != 0);

            // Handle the normal case of a successful write
            let nbytes = retval as usize;
            inner.send_buf.consume_without_copy(nbytes);

            // Now that we have consume non-zero bytes, the buf must become
            // ready to write.
            stream.common.pollee().add_events(Events::OUT);

            // Attempt to send again if there are available data in the buf.
            if !inner.send_buf.is_empty() {
                stream.do_send(&mut inner);
            }
        };

        // Generate the async send request
        let msghdr_ptr = inner.new_send_req();

        // Submit the async send to io_uring
        let io_uring = self.common.io_uring();
        let host_fd = Fd(self.common.host_fd() as _);
        let handle = unsafe { io_uring.sendmsg(host_fd, msghdr_ptr, 0, complete_fn) };
        inner.io_handle.replace(handle);
    }
}

pub struct Sender {
    inner: Mutex<Inner>,
}

impl Sender {
    pub fn new() -> Self {
        let inner = Mutex::new(Inner::new());
        Self { inner }
    }

    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.is_shutdown = true;
        // TODO: update pollee?
    }
}

struct Inner {
    send_buf: CircularBuf<IoUringArray<u8>>,
    send_req: IoUringCell<SendReq>,
    io_handle: Option<IoHandle>,
    is_shutdown: bool,
    fatal: Option<Errno>,
}

// Safety. `SendReq` does not implement `Send`. But since all pointers in `SengReq`
// refer to `send_buf`, we can be sure that it is ok for `SendReq` to move between
// threads. All other fields in `SendReq` implement `Send` as well. So the entirety
// of `Inner` is `Send`-safe.
unsafe impl Send for Inner {}

impl Inner {
    pub fn new() -> Self {
        Self {
            send_buf: CircularBuf::new(IoUringArray::with_capacity(super::SEND_BUF_SIZE)),
            send_req: IoUringCell::new(unsafe { MaybeUninit::<SendReq>::uninit().assume_init() }),
            io_handle: None,
            is_shutdown: false,
            fatal: None,
        }
    }

    /// Constructs a new send request according to the sender's internal state.
    ///
    /// The new `SendReq` will be put into `self.send_req`, which is a location that is
    /// accessible by io_uring. A pointer to the C version of the resulting `SendReq`,
    /// which is `libc::msghdr`, will be returned.
    ///
    /// The buffer used in the new `SendReq` is part of `self.send_buf`.
    pub fn new_send_req(&mut self) -> *mut libc::msghdr {
        let (iovecs, iovecs_len) = self.gen_iovecs_from_send_buf();

        let (msghdr_ptr, iovecs_ptr) = {
            let send_req_ptr = self.send_req.as_ptr() as *mut u8;
            let msghdr_ptr = unsafe { send_req_ptr.add(offset_of!(SendReq, msg)) };
            let iovecs_ptr = unsafe { send_req_ptr.add(offset_of!(SendReq, iovecs)) };
            (
                msghdr_ptr as *mut libc::msghdr,
                iovecs_ptr as *mut libc::iovec,
            )
        };

        let msg = libc::msghdr {
            msg_name: ptr::null_mut() as _,
            msg_namelen: 0,
            msg_iov: iovecs_ptr,
            msg_iovlen: iovecs_len,
            msg_control: ptr::null_mut() as _,
            msg_controllen: 0,
            msg_flags: 0,
        };

        let new_send_req = SendReq { msg, iovecs };
        self.send_req.set(new_send_req);

        msghdr_ptr
    }

    fn gen_iovecs_from_send_buf(&mut self) -> ([libc::iovec; 2], usize) {
        let mut iovecs_len = 0;
        let mut iovecs = unsafe { MaybeUninit::<[libc::iovec; 2]>::uninit().assume_init() };
        self.send_buf.with_consumer_view(|part0, part1| {
            debug_assert!(part0.len() > 0);

            iovecs[0] = libc::iovec {
                iov_base: part0.as_ptr() as _,
                iov_len: part0.len() as _,
            };

            iovecs[1] = if part1.len() > 0 {
                iovecs_len = 2;
                libc::iovec {
                    iov_base: part1.as_ptr() as _,
                    iov_len: part1.len() as _,
                }
            } else {
                iovecs_len = 1;
                libc::iovec {
                    iov_base: ptr::null_mut(),
                    iov_len: 0,
                }
            };

            // Only access the consumer's buffer; zero bytes consumed for now.
            0
        });
        debug_assert!(iovecs_len > 0);
        (iovecs, iovecs_len)
    }
}

#[repr(C)]
struct SendReq {
    msg: libc::msghdr,
    iovecs: [libc::iovec; 2],
}

// Acquired by `IoUringCell<T: Copy>`.
impl Copy for SendReq {}

impl Clone for SendReq {
    fn clone(&self) -> Self {
        *self
    }
}