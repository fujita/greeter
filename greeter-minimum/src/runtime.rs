use futures::future::{BoxFuture, FutureExt};
use futures::prelude::*;
use iou::{IoUring, SQE};
use nix::poll::PollFlags;
use nix::sys::socket::SockFlag;
use polling::Poller;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use uring_sys::io_uring_prep_poll_add;

struct Parking {
    kind: Option<Kind>,
    ring: IoUring,
    poller: Poller,
    completion: rustc_hash::FxHashMap<u64, Completion>,
}
const NR_TASKS: usize = 2048;

thread_local! {
    static PARKING : RefCell<Parking> = {
        RefCell::new(Parking{
            kind: None,
            ring: IoUring::new(4096).unwrap(),
            poller: Poller::new().unwrap(),
            completion: rustc_hash::FxHashMap::default(),
        })
    };

    static RUNNABLE : RefCell<VecDeque<Rc<Task>>> = {
        RefCell::new(VecDeque::with_capacity(NR_TASKS))
    };

}

impl Parking {
    fn add<T: AsRawFd>(&mut self, a: &Async<T>) {
        let raw_fd = a.io.as_raw_fd();
        match self.kind.as_ref().unwrap() {
            Kind::Epoll => {
                a.set_nonblocking(true);
                self.poller
                    .add(a.io.as_ref(), polling::Event::none(raw_fd as usize))
                    .unwrap();
            }
            Kind::UringPoll => a.set_nonblocking(true),
            Kind::Async | Kind::Hybrid => a.set_nonblocking(false),
        }
    }

    fn delete<T: AsRawFd>(&mut self, a: &Async<T>) {
        if let Kind::Epoll = self.kind.as_ref().unwrap() {
            self.poller.delete(a.io.as_ref()).unwrap();
        }
    }

    fn modify<T: AsRawFd>(&mut self, a: &Async<T>, cx: &mut Context, is_readable: bool) {
        let raw_fd = a.io.as_raw_fd() as u64;
        let c = Completion::new(Some(cx.waker().clone()));
        self.completion.insert(raw_fd, c);

        match self.kind.as_ref().unwrap() {
            Kind::Epoll => {
                let e = if is_readable {
                    polling::Event::readable(raw_fd as usize)
                } else {
                    polling::Event::writable(raw_fd as usize)
                };
                self.poller.modify(a.io.as_ref(), e).unwrap();
            }
            Kind::UringPoll => {
                let mut q = self.ring.prepare_sqe().unwrap();
                unsafe {
                    let flags = if is_readable {
                        PollFlags::POLLIN.bits()
                    } else {
                        PollFlags::POLLOUT.bits()
                    };
                    io_uring_prep_poll_add(q.raw_mut(), raw_fd as i32, flags);
                    q.set_user_data(raw_fd as u64);
                    self.ring.submit_sqes().unwrap();
                }
            }
            _ => unreachable!(),
        }
    }

    fn wait(&mut self, sleepable: bool) {
        match self.kind.as_ref().unwrap() {
            Kind::Epoll => {
                let mut events = Vec::with_capacity(NR_TASKS);
                let _ = self.poller.wait(&mut events, None);
                for ev in &events {
                    if let Some(c) = self.completion.remove(&(ev.key as u64)) {
                        c.waker.unwrap().wake();
                    }
                }
            }
            Kind::UringPoll => {
                let _ = self.ring.wait_for_cqes(1);
                while let Some(cqe) = self.ring.peek_for_cqe() {
                    if let Some(c) = self.completion.remove(&cqe.user_data()) {
                        c.waker.unwrap().wake();
                    }
                }
            }
            Kind::Async | Kind::Hybrid => {
                if sleepable {
                    let _ = self.ring.wait_for_cqes(1);
                }
                while let Some(cqe) = self.ring.peek_for_cqe() {
                    let result = cqe.result();
                    let user_data = cqe.user_data();
                    self.completion.entry(user_data).and_modify(|e| {
                        e.complete(result.map(|x| x as usize));
                    });
                }
            }
        }
    }
}

fn run_task(t: Rc<Task>) {
    let future = t.future.borrow_mut();
    let w = waker(t.clone());
    let mut context = Context::from_waker(&w);
    let _ = Pin::new(future).as_mut().poll(&mut context);
}

struct Task {
    future: RefCell<BoxFuture<'static, ()>>,
}

impl RcWake for Task {
    fn wake_by_ref(arc_self: &Rc<Self>) {
        RUNNABLE.with(|runnable| runnable.borrow_mut().push_back(arc_self.clone()));
    }
}

pub fn spawn(future: impl Future<Output = ()> + Send + 'static) {
    let t = Rc::new(Task {
        future: RefCell::new(future.boxed()),
    });
    RUNNABLE.with(|runnable| runnable.borrow_mut().push_back(t));
}

pub struct Async<T: AsRawFd> {
    io: Box<T>,
    flags: iou::sqe::OFlag,
}

impl<T: AsRawFd> Async<T> {
    pub fn new(io: T) -> Self {
        let raw_fd = io.as_raw_fd();
        let a = Async {
            io: Box::new(io),
            flags: nix::fcntl::OFlag::from_bits(
                nix::fcntl::fcntl(raw_fd, nix::fcntl::F_GETFL).unwrap(),
            )
            .unwrap(),
        };
        PARKING.with(|parker| {
            parker.borrow_mut().add(&a);
        });
        a
    }

    fn set_nonblocking(&self, enable: bool) {
        let raw_fd = self.io.as_raw_fd();
        let flags = if enable {
            self.flags | nix::fcntl::OFlag::O_NONBLOCK
        } else {
            self.flags & !nix::fcntl::OFlag::O_NONBLOCK
        };
        nix::fcntl::fcntl(raw_fd, nix::fcntl::F_SETFL(flags)).unwrap();
    }

    fn has_completion(&self) -> bool {
        let raw_fd = self.io.as_raw_fd() as u64;
        PARKING.with(|parking| {
            let parking = parking.borrow_mut();
            parking.completion.contains_key(&raw_fd)
        })
    }

    fn poll_submit<'a>(
        &'a mut self,
        cx: &mut Context<'_>,
        f: impl FnOnce(&mut SQE<'_>),
    ) -> Option<std::io::Result<usize>> {
        let c = Completion::new(None);
        let raw_fd = self.io.as_raw_fd() as u64;
        PARKING.with(|parking| {
            let mut parking = parking.borrow_mut();
            let mut q = parking.ring.prepare_sqe().unwrap();
            f(&mut q);
            unsafe {
                q.set_user_data(raw_fd as u64);
            }
            parking.completion.insert(raw_fd, c);
            parking.ring.submit_sqes().unwrap();

            parking.wait(false);
        });
        self.poll_finish(cx)
    }

    fn poll_finish(&mut self, cx: &mut Context<'_>) -> Option<std::io::Result<usize>> {
        let raw_fd = self.io.as_raw_fd() as u64;
        let is_completed = PARKING.with(|parking| {
            let mut parking = parking.borrow_mut();
            let c = parking.completion.get_mut(&raw_fd).unwrap();
            if c.result.is_none() {
                c.waker.replace(cx.waker().clone());
                None
            } else {
                Some(c.result.take().unwrap())
            }
        });
        is_completed.map(|r| {
            PARKING.with(|parking| {
                parking.borrow_mut().completion.remove(&raw_fd);
            });
            r
        })
    }
}

impl<T: AsRawFd> Drop for Async<T> {
    fn drop(&mut self) {
        PARKING.with(|parking| {
            parking.borrow_mut().delete(self);
        });
    }
}

impl Async<TcpListener> {
    fn poll_sync(
        self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> Poll<Option<std::io::Result<Async<TcpStream>>>> {
        match self.as_ref().io.accept() {
            Ok((stream, _)) => Poll::Ready(Some(Ok(Async::<TcpStream>::new(stream)))),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                PARKING.with(|parking| {
                    let mut parking = parking.borrow_mut();
                    parking.modify(&self, cx, true);
                });
                Poll::Pending
            }
            Err(e) => std::task::Poll::Ready(Some(Err(e))),
        }
    }
}

impl Stream for Async<TcpListener> {
    type Item = std::io::Result<Async<TcpStream>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let kind = PARKING.with(|parking| parking.borrow().kind.unwrap());
        let raw_fd = self.io.as_raw_fd();
        let new_stream = |x: i32| {
            let stream = unsafe { TcpStream::from_raw_fd(x as i32) };
            std::task::Poll::Ready(Some(Ok(Async::<TcpStream>::new(stream))))
        };
        match kind {
            Kind::Epoll | Kind::UringPoll => self.poll_sync(cx),
            Kind::Async | Kind::Hybrid => {
                if !self.has_completion() {
                    if kind == Kind::Hybrid || kind == Kind::Async {
                        self.set_nonblocking(true);
                        let res = self.as_ref().io.accept();
                        self.set_nonblocking(false);
                        if let Ok((stream, _)) = res {
                            return Poll::Ready(Some(Ok(Async::<TcpStream>::new(stream))));
                        }
                    }

                    if let Some(x) = self.poll_submit(cx, |q| unsafe {
                        q.prep_accept(raw_fd, None, SockFlag::empty());
                    }) {
                        match x {
                            Ok(x) => return new_stream(x as i32),
                            Err(e) => return std::task::Poll::Ready(Some(Err(e))),
                        }
                    }
                    return std::task::Poll::Pending;
                }

                match self.poll_finish(cx) {
                    Some(x) => match x {
                        Ok(x) => new_stream(x as i32),
                        Err(e) => std::task::Poll::Ready(Some(Err(e))),
                    },
                    None => std::task::Poll::Pending,
                }
            }
        }
    }
}

impl tokio::io::AsyncRead for Async<TcpStream> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::result::Result<(), std::io::Error>> {
        let raw_fd = self.io.as_raw_fd() as u64;
        let kind = PARKING.with(|parking| parking.borrow().kind.unwrap());

        match kind {
            Kind::Epoll | Kind::UringPoll => unsafe {
                let b =
                    &mut *(buf.unfilled_mut() as *mut [std::mem::MaybeUninit<u8>] as *mut [u8]);
                match self.io.read(b) {
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        PARKING.with(|parking| {
                            let mut parking = parking.borrow_mut();
                            parking.modify(&self, cx, true);
                        });
                        Poll::Pending
                    }
                    Ok(n) => {
                        buf.assume_init(n);
                        buf.advance(n);
                        Poll::Ready(Ok(()))
                    }
                    Err(e) => Poll::Ready(Err(e)),
                }
            },
            Kind::Async | Kind::Hybrid => unsafe {
                let len = buf.remaining();
                let b =
                    &mut *(buf.unfilled_mut() as *mut [std::mem::MaybeUninit<u8>] as *mut [u8]);
                let addr = b.as_mut_ptr();

                if !self.has_completion() {
                    if kind == Kind::Hybrid {
                        let res = libc::recv(
                            raw_fd as i32,
                            addr as _,
                            len,
                            nix::sys::socket::MsgFlags::MSG_DONTWAIT.bits(),
                        );
                        if res >= 0 {
                            let n = res as usize;
                            buf.assume_init(n);
                            buf.advance(n);
                            return Poll::Ready(Ok(()));
                        }
                    }

                    if let Some(x) = self.poll_submit(cx, |q| {
                        uring_sys::io_uring_prep_recv(
                            q.raw_mut(),
                            raw_fd as i32,
                            addr as _,
                            len as _,
                            0,
                        );
                    }) {
                        match x {
                            Ok(n) => {
                                buf.assume_init(n);
                                buf.advance(n);
                                return Poll::Ready(Ok(()));
                            }
                            Err(e) => return Poll::Ready(Err(e)),
                        }
                    }
                    return std::task::Poll::Pending;
                }
                match self.poll_finish(cx) {
                    Some(x) => match x {
                        Ok(n) => {
                            buf.assume_init(n);
                            buf.advance(n);
                            Poll::Ready(Ok(()))
                        }
                        Err(e) => Poll::Ready(Err(e)),
                    },
                    None => std::task::Poll::Pending,
                }
            },
        }
    }
}

impl tokio::io::AsyncWrite for Async<TcpStream> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::result::Result<usize, std::io::Error>> {
        let raw_fd = self.io.as_raw_fd() as u64;
        let kind = PARKING.with(|parking| parking.borrow().kind.unwrap());
        match kind {
            Kind::Epoll | Kind::UringPoll => match self.io.write(buf) {
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    PARKING.with(|parking| {
                        let mut parking = parking.borrow_mut();
                        parking.modify(&self, cx, false);
                    });
                    Poll::Pending
                }
                x => Poll::Ready(x),
            },
            Kind::Async | Kind::Hybrid => {
                if !self.has_completion() {
                    if kind == Kind::Hybrid {
                        unsafe {
                            let res = libc::send(
                                raw_fd as i32,
                                buf.as_ptr() as _,
                                buf.len() as _,
                                nix::sys::socket::MsgFlags::MSG_DONTWAIT.bits(),
                            );
                            if res >= 0 {
                                return Poll::Ready(Ok(res as usize));
                            }
                        }
                    }
                    if let Some(x) = self.poll_submit(cx, |q| unsafe {
                        uring_sys::io_uring_prep_send(
                            q.raw_mut(),
                            raw_fd as i32,
                            buf.as_ptr() as _,
                            buf.len() as _,
                            0,
                        );
                    }) {
                        match x {
                            Ok(n) => return Poll::Ready(Ok(n)),
                            Err(e) => return Poll::Ready(Err(e)),
                        }
                    }
                    return std::task::Poll::Pending;
                }
                match self.poll_finish(cx) {
                    Some(x) => match x {
                        Ok(n) => Poll::Ready(Ok(n)),
                        Err(e) => Poll::Ready(Err(e)),
                    },
                    None => std::task::Poll::Pending,
                }
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), std::io::Error>> {
        self.io.shutdown(std::net::Shutdown::Write)?;
        Poll::Ready(Ok(()))
    }
}

// strolen from the future code
use std::mem::{self, ManuallyDrop};
use std::rc::Rc;
use std::task::{RawWaker, RawWakerVTable};

pub trait RcWake {
    fn wake(self: Rc<Self>) {
        Self::wake_by_ref(&self)
    }

    fn wake_by_ref(arc_self: &Rc<Self>);
}

fn waker<W>(wake: Rc<W>) -> Waker
where
    W: RcWake,
{
    let ptr = Rc::into_raw(wake) as *const ();
    let vtable = &Helper::<W>::VTABLE;
    unsafe { Waker::from_raw(RawWaker::new(ptr, vtable)) }
}

#[allow(clippy::redundant_clone)] // The clone here isn't actually redundant.
unsafe fn increase_refcount<T: RcWake>(data: *const ()) {
    // Retain Arc, but don't touch refcount by wrapping in ManuallyDrop
    let arc = mem::ManuallyDrop::new(Rc::<T>::from_raw(data as *const T));
    // Now increase refcount, but don't drop new refcount either
    let _arc_clone: mem::ManuallyDrop<_> = arc.clone();
}

struct Helper<F>(F);

impl<T: RcWake> Helper<T> {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::clone_waker,
        Self::wake,
        Self::wake_by_ref,
        Self::drop_waker,
    );

    unsafe fn clone_waker(data: *const ()) -> RawWaker {
        increase_refcount::<T>(data);
        let vtable = &Helper::<T>::VTABLE;
        RawWaker::new(data, vtable)
    }

    unsafe fn wake(ptr: *const ()) {
        let rc: Rc<T> = Rc::from_raw(ptr as *const T);
        RcWake::wake(rc);
    }

    unsafe fn wake_by_ref(ptr: *const ()) {
        let arc = ManuallyDrop::new(Rc::<T>::from_raw(ptr as *const T));
        RcWake::wake_by_ref(&arc);
    }

    unsafe fn drop_waker(ptr: *const ()) {
        drop(Rc::from_raw(ptr as *const Task));
    }
}

#[derive(Debug)]
pub struct Completion {
    waker: Option<Waker>,
    result: Option<std::io::Result<usize>>,
}

impl Completion {
    fn new(w: Option<Waker>) -> Completion {
        Completion {
            waker: w,
            result: None,
        }
    }

    fn complete(&mut self, result: std::io::Result<usize>) {
        self.result = Some(result);
        if let Some(w) = self.waker.take() {
            w.wake();
        }
    }
}

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum Kind {
    Epoll,     // socket API with epoll; non-blocking synchronous socket API
    UringPoll, // socket API with io_uring (POLL); replace epoll with io_uring (POLL)
    Async,     // io_uring(ACCEPT/RECV/SEND); always doing asynchronous I/Os.
    Hybrid, // try non-blocking synchronous socket API first, it fails, io_uring(ACCEPT/RECV/SEND)
}

pub struct Runtime {
    kind: Kind,
}

impl Runtime {
    pub fn new(kind: Kind) -> Runtime {
        Runtime { kind }
    }

    pub fn pin_to_cpu(self, id: usize) -> Self {
        core_affinity::set_for_current(core_affinity::CoreId { id });
        self
    }

    pub fn run<F, T>(&self, f: F)
    where
        F: Fn() -> T,
        T: Future<Output = ()> + Send + 'static,
    {
        PARKING.with(|parking| {
            parking.borrow_mut().kind = Some(self.kind);
        });

        let waker = waker_fn::waker_fn(|| {});
        let cx = &mut Context::from_waker(&waker);

        let fut = f();
        pin_utils::pin_mut!(fut);

        loop {
            if fut.as_mut().poll(cx).is_ready() {
                break;
            }

            loop {
                let mut ready =
                    RUNNABLE.with(|runnable| runnable.replace(VecDeque::with_capacity(NR_TASKS)));

                if ready.is_empty() {
                    break;
                }

                for t in ready.drain(..) {
                    run_task(t);
                }
            }

            if fut.as_mut().poll(cx).is_ready() {
                break;
            }
            PARKING.with(|parking| {
                parking.borrow_mut().wait(true);
            });
        }
    }
}
