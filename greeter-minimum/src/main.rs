use clap::clap_app;
use futures_util::stream::StreamExt;
use std::{env, thread};

mod runtime;

async fn serve() {
    let mut listener = runtime::Async::<std::net::TcpListener>::new(proto::create_listen_socket());
    while let Some(ret) = listener.next().await {
        if let Ok(stream) = ret {
            runtime::spawn(async move {
                proto::client::Client::new(stream).serve().await;
            });
        }
    }
}

fn main() {
    let matches = clap_app!(greeter =>
        (@arg MODE: -m --mode +takes_value "specify I/O strategy, which can be: epoll, async, uringpoll, or hybrid")
    )
    .get_matches();

    let kind = if let Some(m) = matches.value_of("MODE") {
        match m {
            "epoll" => runtime::Kind::Epoll,
            "async" => runtime::Kind::Async,
            "uringpoll" => runtime::Kind::UringPoll,
            "hybrid" => runtime::Kind::Hybrid,
            _ => {
                println!("use 'epoll', 'async', 'uringpoll', or 'hybrid'");
                std::process::exit(1);
            }
        }
    } else {
        runtime::Kind::Epoll
    };

    let cpus = {
        env::var("RUSTMAXPROCS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(num_cpus::get)
    };

    println!("Hello, greeter-minimum: {:?} mode, ({} cpus)!", kind, cpus);

    let mut handles = Vec::new();
    for i in 0..cpus {
        let h = thread::spawn(move || {
            let ex = runtime::Runtime::new(kind).pin_to_cpu(i);

            ex.run(serve);
        });
        handles.push(h);
    }
    for h in handles {
        let _ = h.join();
    }
}
