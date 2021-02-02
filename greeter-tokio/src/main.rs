use socket2::{Domain, Socket, Type};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

fn main() {
    println!("Hello, greeter-tokio!");

    // tokio listen socket (created by mio) has too small backlog so create by hand
    let addr: SocketAddr = "[::]:50051".parse().unwrap();
    let sock = Socket::new(Domain::ipv6(), Type::stream(), None).unwrap();

    sock.set_reuse_address(true).unwrap();
    sock.set_reuse_port(true).unwrap();
    sock.set_nonblocking(true).unwrap();
    sock.bind(&addr.into()).unwrap();
    sock.listen(8192).unwrap();

    let rt = Runtime::new().unwrap();

    rt.block_on(async {
        let listener = TcpListener::from_std(sock.into_tcp_listener()).unwrap();

        loop {
            let (stream, _) = listener.accept().await.unwrap();
            rt.spawn(async move {
                proto::client::Client::new(stream).serve().await;
            });
        }
    });
}
