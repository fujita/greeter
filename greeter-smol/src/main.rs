use smol::Async;
use socket2::{Domain, Socket, Type};
use std::net::SocketAddr;

fn main() {
    // smol uses one cpu by default. explicitly needs to specify
    std::env::var("SMOL_THREADS").err().and_then(|_| {
        Some(std::env::set_var(
            "SMOL_THREADS",
            num_cpus::get().to_string(),
        ))
    });
    println!("Hello, greeter-smol!");

    // std::net listen socket has too small backlog so create by hand
    let addr: SocketAddr = "[::]:50051".parse().unwrap();
    let sock = Socket::new(Domain::ipv6(), Type::stream(), None).unwrap();

    sock.set_reuse_address(true).unwrap();
    sock.set_reuse_port(true).unwrap();
    sock.set_nonblocking(true).unwrap();
    sock.bind(&addr.into()).unwrap();
    sock.listen(8192).unwrap();

    smol::block_on(async {
        let listener = Async::new(sock.into_tcp_listener()).unwrap();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            smol::spawn(async move {
                proto::client::Client::new(async_compat::Compat::new(stream))
                    .serve()
                    .await;
            })
            .detach();
        }
    });
}
