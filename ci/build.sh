#!/bin/bash
set -euo pipefail

# Install the Rust stdlib for the current target
rustup target add $TARGET

# Download the Raspberry Pi cross-compilation toolchain if needed
if [ "$TARGET" = "armv5te-unknown-linux-musleabi" ]
then
  wget https://musl.cc/arm-linux-musleabihf-cross.tgz
  tar zxf ./arm-linux-musleabihf-cross.tgz -C /tmp
  export PATH=/tmp/arm-linux-musleabihf-cross/bin:$PATH
fi

# Compile the binary for the current target
cargo build --target=$TARGET --release

# Package up the release binary
tar -C target/$TARGET/release -czf rsnova-$TRAVIS_TAG-$TARGET.tar rsnova
tar uf rsnova-$TRAVIS_TAG-$TARGET.tar client.toml server.toml