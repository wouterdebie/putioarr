#!/bin/bash
set -e

# Derive TARGETARCH from TARGETPLATFORM if not already set
if [ -z "$TARGETARCH" ]; then
    TARGETARCH=$(echo $TARGETPLATFORM | cut -d'/' -f2)
fi

case $TARGETARCH in
    "amd64")
        export CARGO_BUILD_TARGET=x86_64-unknown-linux-musl
    ;;
    "arm64")
        export CARGO_BUILD_TARGET=aarch64-unknown-linux-musl
    ;;
    *)
        echo "Unsupported architecture: $TARGETARCH"
        exit 1
    ;;
esac

# musl-gcc builds the C parts of dependencies (ring). Rust links musl targets
# statically by default, so the binary needs no libc from the runtime image.
apt install -y musl-tools

rustup target add ${CARGO_BUILD_TARGET}
cargo build --release --target ${CARGO_BUILD_TARGET}

while getopts "c:" o; do
    case "${o}" in
        c)
            location=${OPTARG}
            file target/${CARGO_BUILD_TARGET}/release/putioarr
            cp target/${CARGO_BUILD_TARGET}/release/putioarr $location
            ;;
    esac
done
