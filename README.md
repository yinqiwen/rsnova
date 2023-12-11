Rust practice project

## Features

- QUIC/TLS Support
- HTTP/Socks5 Proxy


# Getting Started

**Examples**

**Build**
```sh
$ cargo build --release
```

**Generate cert/key for TLS/QUIC**
```sh
$ ./target/release/rsnova --rcgen
```

**Launch Server At Remote Server**
```sh
$ ./rsnova --role server --protocol tls --key ./key.pem --cert ./cert.pem --listen 0.0.0.0:48100
```

**Launch Server At Local Client**
```sh
$ ./rsnova --role client  --cert ./cert.pem --listen 127.0.0.1:48100 --remote tls://<ip:port>
```

**Use Proxy**    
Now you can configure `socks5://127.0.0.1:48100` as the proxy for browser/tools. 

