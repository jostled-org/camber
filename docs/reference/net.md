# Net Reference

`camber::net` exposes lower-level networking APIs below the HTTP router layer.

Use these when you want Camber's runtime and shutdown handling, but not the full HTTP server stack.

## Listeners

Create a listener with `net::listen(addr)`.

Supported address forms:

- `host:port` or `:port` for TCP
- `unix:/path/to/socket` for Unix domain sockets

`Listener` is the shared entrypoint for TCP and Unix socket binding. `ListenerAddr` reports which one you got back.

## TCP

`TcpStream` is the low-level connection type for plain TCP work.

Use the `serve_tcp*` entrypoints when you want Camber to accept connections and hand each connection to your async handler. Use `TcpStream::connect(...)` when you want an outbound connection.

There are separate plain-TCP and TCP+TLS server entrypoints. All of them participate in Camber's normal shutdown handling.

## UDP

`UdpSocket` is the datagram-side equivalent.

Use it directly for bind/connect/send/receive operations. Use `serve_udp` or `serve_udp_on` when you want Camber to run the recv loop for you.

UDP handlers run inline. If you need per-datagram concurrency, spawn from inside the handler.

## TLS Streams

`TlsStream` is the TLS equivalent of `TcpStream`. It keeps the same read/write shape and adds peer-certificate inspection for cases like certificate probing or custom validation flows.

## Forwarding

Use `net::forward(a, b)` to copy bytes bidirectionally between two async streams until one side closes.

This is the low-level primitive behind simple tunnel-style forwarding.
