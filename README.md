# RSnova: Private Proxy Solution & Network Troubleshooting Tool.    
[![Build Status](https://travis-ci.org/yinqiwen/rsnova.svg?branch=master)](https://travis-ci.org/yinqiwen/rsnova) [![License](https://img.shields.io/badge/License-BSD%203--Clause-blue.svg)](https://opensource.org/licenses/BSD-3-Clause) ![GitHub release (latest by date)](https://img.shields.io/github/v/release/yinqiwen/rsnova) ![GitHub last commit](https://img.shields.io/github/last-commit/yinqiwen/rsnova)   
[![Deploy](https://www.herokucdn.com/deploy/button.svg)](https://heroku.com/deploy)

# Features
- Multiplexing 
    - All proxy connections running over N persist proxy channel connections
- Simple PAC(Proxy Auto Config)
- Multiple Ciphers support
    - Chacha20Poly1305
    - AES128
- HTTP/Socks4/Socks5 Proxy
    - Local client running as HTTP/Socks4/Socks5 Proxy
- Transparent TCP Proxy
	- Transparent tcp proxy implementation 
- Low-memory Environments Support
    - Use 10MB RSS memory at client/server side

# Usage
```shell
./target/debug/rsnova -h
rsnova 0.1.0
yinqiwen<yinqiwen@gmail.com>
Private proxy solution & network troubleshooting tool.

USAGE:
    rsnova [OPTIONS]

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information

OPTIONS:
    -c, --config <FILE>    Sets a custom config file [default: ./rsnova.toml]
```

## Client Side
```shell
./rsnova -c ./client.toml
```

client.toml 
```toml
[log]
logtostderr = true
level = "info"
logdir = "./"

[[tunnel]]
listen = "127.0.0.1:48100"
pac=[{host = ".*", channel = "rmux"}]

[[channel]]
# name of current channel
name = "rmux"
# host & port of server
url = "127.0.0.1:48101"
ping_interval_sec = 10
conns_per_host = 5
max_alive_mins = 40
# cipher to communicate with server
cipher = {key="abcdefg", method = "chacha20poly1305"}
```

## Server Side
```shell
./rsnova -c ./server.toml
```

server.toml
```toml
[log]
logtostderr = true
level = "info"
logdir = "./"


[[tunnel]]
# listen address of tunnel server
listen = "rmux://127.0.0.1:48101"
# pac rule to relay traffic, 'direct' is special channel which relay direct to remote target server
pac=[{host = ".*", channel = "direct"}]
cipher = {key="abcdefg", method = "chacha20poly1305"}
```
