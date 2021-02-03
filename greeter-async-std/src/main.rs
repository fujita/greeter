use async_std::net::TcpListener;
use async_std::task;
use socket2::{Domain, Socket, Type};
use std::net::SocketAddr;

fn main() {
    println!("Hello, greeter-async-std!");

    // std::net listen socket has too small backlog so create by hand
    let addr: SocketAddr = "[::]:50051".parse().unwrap();
    let sock = Socket::new(Domain::ipv6(), Type::stream(), None).unwrap();

    sock.set_reuse_address(true).unwrap();
    sock.set_reuse_port(true).unwrap();
    sock.set_nonblocking(true).unwrap();
    sock.bind(&addr.into()).unwrap();
    sock.listen(8192).unwrap();

    task::block_on(async {
        let listener: TcpListener = TcpListener::from(sock.into_tcp_listener());
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            task::spawn(async move {
                proto::client::Client::new(async_compat::Compat::new(stream))
                    .serve()
                    .await;
            });
        }
    });
}
