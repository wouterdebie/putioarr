
FROM rust:1.72-alpine as builder
WORKDIR /usr/src

# Install build dependencies
RUN apk update
RUN apk add --no-cache pkgconfig musl musl-dev gcc openssl openssl-dev

# Create blank project
RUN USER=root cargo new putioarr

# We want dependencies cached, so copy those first.
COPY Cargo.toml Cargo.lock /usr/src/putioarr/

# Set the working directory
WORKDIR /usr/src/putioarr

## Install target platform (Cross-Compilation) --> Needed for Alpine
RUN rustup target add x86_64-unknown-linux-musl

# This is a dummy build to get the dependencies cached.
RUN RUSTFLAGS="-Ctarget-feature=-crt-static" cargo build --target x86_64-unknown-linux-musl --release

# Now copy in the rest of the sources
COPY src /usr/src/putioarr/src/

## Touch main.rs to prevent cached release build
RUN touch /usr/src/putioarr/src/main.rs

# This is the actual application build.
RUN RUSTFLAGS="-Ctarget-feature=-crt-static" cargo build --target x86_64-unknown-linux-musl --release

### Rutime

FROM ghcr.io/linuxserver/baseimage-alpine:edge
# set version label
ARG BUILD_DATE
ARG VERSION

LABEL maintainer="wouterdebie"

RUN apk add musl gcc

COPY --from=builder /usr/src/putioarr/target/x86_64-unknown-linux-musl/release/putioarr /usr/bin

# add local files
COPY root/ /

# ports and volumes
EXPOSE 9091
VOLUME /config
