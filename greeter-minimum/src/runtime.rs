use futures::future::{BoxFuture, FutureExt};
use futures::prelude::*;
use iou::IoUring;
use nix::poll::PollFlags;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::{env, thread};
use uring_sys::io_uring_prep_poll_add;

struct Poller {
    ring: IoUring,
    wait: rustc_hash::FxHashMap<u64, Waker>,
}

const NR_TASKS: usize = 2048;

thread_local! {
    static POLLER : RefCell<Poller> = {
        RefCell::new(Poller{
            ring: IoUring::new(4096).unwrap(),
            wait: rustc_hash::FxHashMap::default(),
        })
    };

    static RUNNABLE : RefCell<VecDeque<Rc<Task>>> = {
        RefCell::new(VecDeque::with_capacity(NR_TASKS))
    };

}

fn run_task(t: Rc<Task>) {
    let future = t.future.borrow_mut();
    let w = waker(t.clone());
    let mut context = Context::from_waker(&w);
    let _ = Pin::new(future).as_mut().poll(&mut context);
}

pub fn run<F, T>(f: F)
where
    F: Fn() -> T,
    T: Future<Output = ()> + Send + 'static,
{
    let cpus = {
        env::var("RUSTMAXPROCS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(num_cpus::get)
    };

    println!("Hello, greeter-minimum ({} cpus)!", cpus);

    let mut handles = Vec::new();
    for i in 0..cpus {
        let r = f();
        let h = thread::spawn(move || {
            core_affinity::set_for_current(core_affinity::CoreId { id: i });
            spawn(r);

            loop {
                loop {
                    let mut ready = RUNNABLE
                        .with(|runnable| runnable.replace(VecDeque::with_capacity(NR_TASKS)));

                    if ready.is_empty() {
                        break;
                    }

                    for t in ready.drain(..) {
                        run_task(t);
                    }
                }

                POLLER.with(|poller| {
                    let mut p = poller.borrow_mut();
                    let _ = p.ring.wait_for_cqes(1);

                    while let Some(cqe) = p.ring.peek_for_cqe() {
                        if let Some(w) = p.wait.remove(&cqe.user_data()) {
                            w.wake();
                        }
                    }
                });
            }
        });
        handles.push(h);
    }

    for h in handles {
        let _ = h.join();
    }
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
}

impl Async<TcpListener> {
    pub fn new(listener: TcpListener) -> Self {
        Async {
            io: Box::new(listener),
        }
    }
}

impl Stream for Async<TcpListener> {
    type Item = std::io::Result<Async<TcpStream>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let token = self.as_ref().io.as_raw_fd() as u64;
        match self.io.accept() {
            Ok((stream, _)) => Poll::Ready(Some(Ok(Async::<TcpStream>::new(stream)))),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                POLLER.with(|poller| {
                    let mut poller = poller.borrow_mut();
                    poller.wait.insert(token, cx.waker().clone());
                    let mut q = poller.ring.prepare_sqe().unwrap();
                    unsafe {
                        io_uring_prep_poll_add(q.raw_mut(), token as i32, PollFlags::POLLIN.bits());
                        q.set_user_data(token);
                        let _ = poller.ring.submit_sqes();
                    }
                });
                Poll::Pending
            }
            Err(e) => std::task::Poll::Ready(Some(Err(e))),
        }
    }
}

impl Async<TcpStream> {
    pub fn new(io: TcpStream) -> Self {
        io.set_nodelay(true).unwrap();
        let raw_fd = io.as_raw_fd();
        let flags =
            nix::fcntl::OFlag::from_bits(nix::fcntl::fcntl(raw_fd, nix::fcntl::F_GETFL).unwrap())
                .unwrap()
                | nix::fcntl::OFlag::O_NONBLOCK;
        nix::fcntl::fcntl(raw_fd, nix::fcntl::F_SETFL(flags)).unwrap();

        Async { io: Box::new(io) }
    }
}

impl tokio::io::AsyncRead for Async<TcpStream> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::result::Result<(), std::io::Error>> {
        let token = self.io.as_raw_fd() as u64;
        unsafe {
            let b = &mut *(buf.unfilled_mut() as *mut [std::mem::MaybeUninit<u8>] as *mut [u8]);
            match self.io.read(b) {
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    POLLER.with(|poller| {
                        let mut poller = poller.borrow_mut();
                        poller.wait.insert(token, cx.waker().clone());
                        let mut q = poller.ring.prepare_sqe().unwrap();
                        io_uring_prep_poll_add(q.raw_mut(), token as i32, PollFlags::POLLIN.bits());
                        q.set_user_data(token);
                        let _ = poller.ring.submit_sqes();
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
        }
    }
}

impl tokio::io::AsyncWrite for Async<TcpStream> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::result::Result<usize, std::io::Error>> {
        let token = self.io.as_raw_fd() as u64;
        match self.io.write(buf) {
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                POLLER.with(|poller| {
                    let mut poller = poller.borrow_mut();
                    poller.wait.insert(token, cx.waker().clone());
                    let mut q = poller.ring.prepare_sqe().unwrap();
                    unsafe {
                        io_uring_prep_poll_add(
                            q.raw_mut(),
                            token as i32,
                            PollFlags::POLLOUT.bits(),
                        );
                        q.set_user_data(token);
                    }
                    let _ = poller.ring.submit_sqes();
                });
                Poll::Pending
            }
            x => Poll::Ready(x),
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
