# gRPC greeter server supporting multiple async runtimes

Tested with the following runtimes.

- glommio
- Tokio
- smol
- async-std
- minimum (simple runtime that shares nothing between CPUs and uses io_uring API with IORING_OP_POLL_ADD)

## Hardware

- Server: c6gn.8xlarge (32 vCPU/64 GiB) / Kernel 5.8.0
- Client: c5n.9xlarge (36 vCPU/96 GiB)

## Benchmark

```
ghz --insecure --proto helloworld.proto --call helloworld.Greeter.SayHello -d '{\"name\":\"Joe\"}' --connections=3000 172.31.22.145:50051 -c 12000 -n 3000000 -t 0
```

- One client machine runs 3,000 gRPC clients (i.e. 3,000 HTTP/2 clients).
- One client machine issues 3,000,000 requests in total.
- Tested with one, two, four, and eight client machines.

## Results

![Throughput (requests per second)](https://raw.githubusercontent.com/fujita/greeter/images/20210219-01.png)
