use glommio::net::TcpListener;
use glommio::{LocalExecutorBuilder, Task};

fn main() {
    let cpus = num_cpus::get();
    println!("Hello, greeter-glommio ({} cpus)!", cpus);

    let mut handles = Vec::new();
    for i in 0..cpus {
        let h = std::thread::spawn(move || {
            let ex = LocalExecutorBuilder::new().pin_to_cpu(i).make().unwrap();
            ex.run(async move {
                let listener = TcpListener::bind("[::]:50051").unwrap();
                loop {
                    let stream = listener.accept().await.unwrap();
                    Task::local(async move {
                        proto::client::Client::new(async_compat::Compat::new(stream))
                            .serve()
                            .await;
                    })
                    .detach();
                }
            });
        });
        handles.push(h);
    }
    for h in handles {
        let _ = h.join();
    }
}
