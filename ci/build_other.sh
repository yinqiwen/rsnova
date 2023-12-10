#!/bin/bash

if [ "$#" -ne 1 ]; then
    echo "Illegal number of parameters"
    exit 1
fi

TARGET=$1
CUR_DIR=$( cd $( dirname $0 ) && pwd )
PKG_DIR="${CUR_DIR}/release"
mkdir -p "${PKG_DIR}"
VERSION=$(grep -E '^version' ${CUR_DIR}/../Cargo.toml | awk '{print $3}' | sed 's/"//g')

# if [ "$TARGET" = "arm-unknown-linux-musleabi" ]
# then
#   wget https://musl.cc/arm-linux-musleabi-cross.tgz
#   tar zxf ./arm-linux-musleabi-cross.tgz -C /tmp
#   export PATH=/tmp/arm-linux-musleabi-cross/bin:$PATH
# fi

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
    echo "Build for windows"
    #echo "`ls -l target/$TARGET/release`"
    tar -C target/$TARGET/release -cf rsnova-$VERSION-$TARGET.tar rsnova.exe
else
    tar -C target/$TARGET/release -cf rsnova-$VERSION-$TARGET.tar rsnova
fi
tar uf rsnova-$VERSION-$TARGET.tar 
gzip rsnova-$VERSION-$TARGET.tar
mv rsnova-$VERSION-$TARGET.tar.gz $PKG_DIR