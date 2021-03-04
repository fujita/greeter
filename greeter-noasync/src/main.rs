use byteorder::{BigEndian, ByteOrder};
use mio::{net::TcpListener, net::TcpStream, Events, Interest, Token};
use quick_protobuf::{BytesReader, MessageRead, MessageWrite, Writer};
use rustc_hash::FxHashMap;
use solicit::http::connection::HttpFrame;
use solicit::http::frame::{
    unpack_header, DataFrame, Frame, FrameIR, HeadersFlag, HeadersFrame, HttpSetting, PingFrame,
    RawFrame, SettingsFrame, WindowUpdateFrame, FRAME_HEADER_LEN,
};
use solicit::http::{Header, INITIAL_CONNECTION_WINDOW_SIZE};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{Cursor, Read, Write};
use std::os::unix::io::AsRawFd;
use std::time::SystemTime;
use std::{env, io, thread};
use thiserror::Error;

#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate slice_as_array;

thread_local! {
    static POLL : RefCell<mio::Poll> = {
        RefCell::new(mio::Poll::new().unwrap())
    };
}

lazy_static! {
    static ref HTTP_STATUS_HEADERS: Vec<u8> = {
        let headers = vec![
            Header::new(b":status", b"200"),
            Header::new(b"content-type".to_vec(), b"application/grpc".to_vec()),
        ];
        hpack::Encoder::new().encode(headers.iter().map(|h| (h.name(), h.value())))
    };
    static ref GRPC_STATUS_HEADERS: Vec<u8> = {
        let headers = vec![
            Header::new(b"grpc-status".to_vec(), b"0"),
            Header::new(b"grpc-message".to_vec(), b"".to_vec()),
        ];
        hpack::Encoder::new().encode(headers.iter().map(|h| (h.name(), h.value())))
    };
    static ref REQUEST_HEADERS: HashMap<Vec<u8>, Vec<u8>> = vec![(
        String::from(":path").into_bytes(),
        String::from("/helloworld.Greeter/SayHello").into_bytes()
    ),]
    .into_iter()
    .collect();
    static ref PREFACE: Vec<u8> = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("wrong preface string")]
    WrongPreface,
    #[error("wrong headers")]
    WrongHeaders,
    #[error("disconnected")]
    Disconnected(#[from] io::Error),
    #[error("wrong http frame")]
    WrongHttpFrame(solicit::http::HttpError),
}

struct Client {
    stream: TcpStream,
    buffer: bytes::BytesMut,
    established: bool,
    wqueue: VecDeque<Vec<u8>>,
    ping: u64,
    window_size: i32,
    decoder: hpack::Decoder<'static>,
}

impl Drop for Client {
    fn drop(&mut self) {
        POLL.with(|poll| {
            poll.borrow_mut()
                .registry()
                .deregister(&mut self.stream)
                .unwrap();
        });
    }
}

impl Client {
    fn new(mut stream: TcpStream) -> Self {
        stream.set_nodelay(true).unwrap();
        let raw_fd = stream.as_raw_fd();
        let token = Token(raw_fd as usize);

        let flags =
            nix::fcntl::OFlag::from_bits(nix::fcntl::fcntl(raw_fd, nix::fcntl::F_GETFL).unwrap())
                .unwrap()
                | nix::fcntl::OFlag::O_NONBLOCK;
        nix::fcntl::fcntl(raw_fd, nix::fcntl::F_SETFL(flags)).unwrap();

        POLL.with(|poll| {
            poll.borrow_mut()
                .registry()
                .register(&mut stream, token, Interest::READABLE | Interest::WRITABLE)
                .unwrap();
        });

        let mut c = Client {
            stream,
            buffer: bytes::BytesMut::new(),
            established: false,
            wqueue: VecDeque::new(),
            ping: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            window_size: INITIAL_CONNECTION_WINDOW_SIZE,
            decoder: hpack::Decoder::new(),
        };
        let mut s = SettingsFrame::new();
        s.add_setting(HttpSetting::MaxFrameSize(16384));
        c.queue(s);
        c
    }

    fn token(&self) -> Token {
        Token(self.stream.as_raw_fd() as usize)
    }

    fn queue<T: FrameIR>(&mut self, frame: T) {
        let mut buf = Cursor::new(Vec::new());
        frame.serialize_into(&mut buf).unwrap();
        self.wqueue.push_back(buf.into_inner());
    }

    fn flush(&mut self) {
        while let Some(buf) = self.wqueue.pop_front() {
            if buf.len() > self.window_size as usize {
                println!("window size is full!");
                self.wqueue.push_front(buf);
                return;
            }
            match self.stream.write(&buf) {
                Ok(_) => {}
                Err(_) => {
                    self.wqueue.push_front(buf);
                    return;
                }
            }
        }
    }

    fn consume(&mut self, size: usize) -> Option<Vec<u8>> {
        if self.buffer.len() < size {
            return None;
        }
        Some(self.buffer.split_to(size).to_vec())
    }

    fn read_all(&mut self) -> Result<(), Error> {
        const RESERVE: usize = 8192;
        loop {
            let len = self.buffer.len();
            self.buffer.reserve(len + RESERVE);
            unsafe {
                self.buffer.set_len(len + RESERVE);
            }
            match self.stream.read(&mut self.buffer.as_mut()[len..]) {
                Ok(n) => {
                    unsafe {
                        self.buffer.set_len(len + n);
                    }
                    if n == 0 {
                        return Err(Error::Disconnected(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            "read returned zero",
                        )));
                    }
                }
                Err(e) => {
                    unsafe {
                        self.buffer.set_len(len);
                    }
                    if e.kind() == io::ErrorKind::WouldBlock {
                        break;
                    }
                    return Err(Error::Disconnected(e));
                }
            }
        }
        Ok(())
    }

    fn handle_dataframe(&mut self, frame: DataFrame) {
        let mut buf = Cursor::new(Vec::new());

        let req = proto::helloworld::HelloRequest::from_reader(
            &mut BytesReader::from_bytes(&frame.data[5..]),
            &frame.data[5..],
        )
        .unwrap();

        let stream_id = frame.get_stream_id();

        WindowUpdateFrame::for_connection(frame.payload_len())
            .serialize_into(&mut buf)
            .unwrap();
        PingFrame::with_data(self.ping)
            .serialize_into(&mut buf)
            .unwrap();

        self.ping += 1;

        let mut frame = HeadersFrame::new(HTTP_STATUS_HEADERS.to_vec(), stream_id);
        frame.set_flag(HeadersFlag::EndHeaders);
        frame.serialize_into(&mut buf).unwrap();

        let reply = self.say_hello(req);

        let mut data = vec![0; 5];
        BigEndian::write_u32(&mut data[1..], reply.get_size() as u32);
        reply.write_message(&mut Writer::new(&mut data)).unwrap();

        let frame = DataFrame::with_data(stream_id, data);
        self.window_size -= frame.payload_len() as i32;
        frame.serialize_into(&mut buf).unwrap();

        let mut frame = HeadersFrame::new(GRPC_STATUS_HEADERS.to_vec(), stream_id);
        frame.set_flag(HeadersFlag::EndHeaders);
        frame.set_flag(HeadersFlag::EndStream);
        frame.serialize_into(&mut buf).unwrap();

        self.wqueue.push_back(buf.into_inner());
    }

    #[allow(clippy::transmute_ptr_to_ref)]
    fn handle(&mut self, readable: bool, _writable: bool) -> Result<(), Error> {
        if readable {
            self.read_all()?;

            if !self.established {
                match self.consume(PREFACE.len()) {
                    Some(buf) => {
                        if &buf != &PREFACE as &'static Vec<u8> {
                            return Err(Error::WrongPreface);
                        }
                        self.established = true;
                    }
                    None => {
                        return Ok(());
                    }
                }
            }

            if self.established {
                loop {
                    if self.buffer.len() < FRAME_HEADER_LEN {
                        break;
                    }
                    let header = unpack_header(
                        slice_as_array!(
                            &self.buffer.as_ref()[0..FRAME_HEADER_LEN],
                            [u8; FRAME_HEADER_LEN]
                        )
                        .unwrap(),
                    );

                    match self.consume(FRAME_HEADER_LEN + header.0 as usize) {
                        Some(buf) => {
                            let raw = RawFrame::from(buf);
                            match HttpFrame::from_raw(&raw) {
                                Ok(frame) => match frame {
                                    HttpFrame::DataFrame(frame) => {
                                        self.handle_dataframe(frame);
                                    }
                                    HttpFrame::HeadersFrame(frame) => {
                                        for (k, v) in
                                            self.decoder.decode(&frame.header_fragment()).unwrap()
                                        {
                                            if let Some(expected) = REQUEST_HEADERS.get(&k) {
                                                if &v != expected {
                                                    // should send an error response instead
                                                    return Err(Error::WrongHeaders);
                                                }
                                            }
                                        }
                                    }
                                    HttpFrame::RstStreamFrame(_) => {}
                                    HttpFrame::SettingsFrame(frame) => {
                                        if !frame.is_ack() {
                                            self.queue(SettingsFrame::new_ack());
                                        }
                                    }
                                    HttpFrame::PingFrame(frame) => {
                                        if !frame.is_ack() {
                                            self.queue(PingFrame::new_ack(frame.opaque_data()));
                                        }
                                    }
                                    HttpFrame::GoawayFrame(_) => {}
                                    HttpFrame::WindowUpdateFrame(frame) => {
                                        self.window_size += frame.increment() as i32;
                                    }
                                    HttpFrame::UnknownFrame(_) => {}
                                },
                                Err(e) => return Err(Error::WrongHttpFrame(e)),
                            }
                        }
                        None => break,
                    }
                }
            }
        }
        self.flush();
        Ok(())
    }

    fn say_hello(&self, req: proto::helloworld::HelloRequest) -> proto::helloworld::HelloReply {
        proto::helloworld::HelloReply {
            message: Cow::Owned(format!("Hello {}", req.name)),
        }
    }
}

struct Greeter {
    listener: TcpListener,
}

impl Greeter {
    fn new() -> Self {
        let mut listener = TcpListener::from_std(proto::create_listen_socket());
        let listen_token = Token(listener.as_raw_fd() as usize);
        POLL.with(|poll| {
            poll.borrow_mut()
                .registry()
                .register(&mut listener, listen_token, Interest::READABLE)
                .unwrap();
        });
        Greeter { listener }
    }

    fn serve(&self) {
        let mut events = Events::with_capacity(2048);
        let mut clients = FxHashMap::default();
        let listen_token = Token(self.listener.as_raw_fd() as usize);

        loop {
            POLL.with(|poll| {
                poll.borrow_mut().poll(&mut events, None).unwrap();
            });

            for e in events.iter() {
                if e.token() == listen_token {
                    while let Ok((stream, _)) = self.listener.accept() {
                        let client = Client::new(stream);
                        clients.insert(client.token(), client);
                    }
                } else if let Some(client) = clients.get_mut(&e.token()) {
                    let r = client.handle(e.is_readable(), e.is_writable());
                    if r.is_err() || e.is_error() || e.is_read_closed() || e.is_write_closed() {
                        clients.remove(&e.token());
                    }
                }
            }
        }
    }
}

fn main() {
    let cpus = {
        env::var("RUSTMAXPROCS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(num_cpus::get)
    };

    println!("Hello, greeter-noasync ({} cpus)!", cpus);

    let mut handles = Vec::new();
    for i in 0..cpus {
        let h = thread::spawn(move || {
            core_affinity::set_for_current(core_affinity::CoreId { id: i });

            Greeter::new().serve();
        });
        handles.push(h);
    }
    for h in handles {
        let _ = h.join();
    }
}
