use tokio::net::TcpListener;
use tokio::runtime::Runtime;

fn main() {
    println!("Hello, greeter-tokio!");

    let rt = Runtime::new().unwrap();

    rt.block_on(async {
        let listener = TcpListener::from_std(proto::create_listen_socket()).unwrap();

        loop {
            let (stream, _) = listener.accept().await.unwrap();
            rt.spawn(async move {
                proto::client::Client::new(stream).serve().await;
            });
        }
    });
}
