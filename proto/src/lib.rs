#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate slice_as_array;

pub mod client;
pub mod helloworld;

use socket2::{Domain, Socket, Type};
use std::net::SocketAddr;

// std::net/mio listen socket has too small backlog so create by hand
pub fn create_listen_socket() -> std::net::TcpListener {
    let addr: SocketAddr = "[::]:50051".parse().unwrap();

    let sock = Socket::new(
        match addr {
            SocketAddr::V4(_) => Domain::ipv4(),
            SocketAddr::V6(_) => Domain::ipv6(),
        },
        Type::stream(),
        None,
    )
    .unwrap();

    sock.set_reuse_address(true).unwrap();
    sock.set_reuse_port(true).unwrap();
    sock.set_nonblocking(true).unwrap();
    sock.bind(&addr.into()).unwrap();
    sock.listen(8192).unwrap();

    sock.into_tcp_listener()
}
