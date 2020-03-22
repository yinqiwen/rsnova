#!/bin/bash

# Install the Rust stdlib for the current target
rustup target add $TARGET

if [ "$TARGET" = "armv5te-unknown-linux-musleabi" ]
then
  wget https://musl.cc/arm-linux-musleabihf-cross.tgz
  tar zxf ./arm-linux-musleabihf-cross.tgz -C /tmp
  export PATH=/tmp/arm-linux-musleabihf-cross/bin:$PATH
fi

# Compile the binary for the current target
cargo build --target=$TARGET --release
EXITCODE=$?
if [ $EXITCODE -ne 0 ]; then
    echo "cargo build failed"
    exit $EXITCODE
fi

# Package up the release binary
if [ "$TARGET" = "x86_64-pc-windows-msvc" ]
then
    mv target/$TARGET/release/rsnova target/$TARGET/release/rsnova.exe
    tar -C target/$TARGET/release -cf rsnova-$TRAVIS_TAG-$TARGET.tar rsnova.exe
else
    tar -C target/$TARGET/release -cf rsnova-$TRAVIS_TAG-$TARGET.tar rsnova
fi
tar uf rsnova-$TRAVIS_TAG-$TARGET.tar client.toml server.toml
gzip rsnova-$TRAVIS_TAG-$TARGET.tar