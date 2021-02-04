use async_std::net::TcpListener;
use async_std::task;

fn main() {
    println!("Hello, greeter-async-std!");

    task::block_on(async {
        let listener: TcpListener = TcpListener::from(proto::create_listen_socket());
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
