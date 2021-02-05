use smol::Async;

fn main() {
    // smol uses one cpu by default. explicitly needs to specify
    if std::env::var("SMOL_THREADS").is_err() {
        std::env::set_var("SMOL_THREADS", num_cpus::get().to_string())
    }

    println!("Hello, greeter-smol!");

    smol::block_on(async {
        let listener = Async::new(proto::create_listen_socket()).unwrap();
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
