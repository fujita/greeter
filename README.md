# gRPC greeter server supporting multiple async runtimes

Tested with the following runtimes.

- Tokio
- async-std
- smol
- glommio
- minimum (simple runtime using io_uring API with poll opcode)

## Hardware

- Server: c6gn.8xlarge (32 vCPU/64 GiB)
- Client: c5n.9xlarge (36 vCPU/96 GiB)

## Benchmark

```
ghz --insecure --proto helloworld.proto --call helloworld.Greeter.SayHello -d '{\"name\":\"Joe\"}' --connections=3000 172.31.22.145:50051 -c 12000 -n 3000000 -t 0
```

- One client machine runs 3,000 gRPC clients (i.e. 3,000 HTTP/2 clients).
- One client machine issues 3,000,000 requests in total.
- Tested with one, two, four, and eight client machines.

## Results

![Throughput (requests per second)](https://raw.githubusercontent.com/fujita/greeter/images/20210210-01.png)

Note that glommio couldn't complete the benchmark with 24,000 clietns.
