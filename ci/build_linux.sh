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

cross build --target=$TARGET --release
EXITCODE=$?
if [ $EXITCODE -ne 0 ]; then
    echo "cross build failed"
    exit $EXITCODE
fi

# Package up the release binary
tar -C target/$TARGET/release -cf rsnova-$VERSION-$TARGET.tar rsnova
tar uf rsnova-$VERSION-$TARGET.tar 
gzip rsnova-$VERSION-$TARGET.tar
mv rsnova-$VERSION-$TARGET.tar.gz $PKG_DIR