# Performance

## Software

- Go 1.14.8
- Rust 1.46.0

- grpc-go (1c32b026)
- tonic (c7fd9d45)
[with modification for accepting any IP addresses and returning the same response as grpc-go](https://github.com/fujita/tonic/tree/benchmark)
- this software (5ff94430)
- ghz 0.59.0

## Hardware

- Server: c5a.8xlarge (32 vCPU/64 GiB)
- Client: c5a.16xlarge (64 vCPU/128 GiB)

## Benchmark

```
ghz --insecure --proto helloworld.proto --call helloworld.Greeter.SayHello -d '{\"name\":\"Joe\"}' --connections=3000 172.31.22.145:50051 -c 12000 -n 6000000 -t 0
```

- One client machine runs 3,000 gRPC clients (i.e. 3,000 HTTP/2 clients).
- One client issues 6,000,000 requests in total.
- Tested with one, two, and four client matchies.

## Results

Throughput (requests per second)

|        |3000     |6000     |12000    |24000    |
---------|---------|---------|---------|----------
|  tonic |175574.16|356524.64|501463.54|N/A      |
| grpc-go|172038.42|340815.79|578428.48|602509.78|
|    this|179150.62|360018.75|709114.78|925444.54|
