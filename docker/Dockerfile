
FROM --platform=$BUILDPLATFORM rust:1.72-alpine as builder
ARG TARGETARCH
WORKDIR /usr/src

# Install build dependencies
RUN apk update
RUN apk add --no-cache pkgconfig musl musl-dev gcc openssl openssl-dev bash

# Set the right architecture
COPY docker/platform.sh /usr/local/bin
RUN /usr/local/bin/platform.sh

RUN echo $(ls /usr/bin/*gcc)

# Create blank project
RUN USER=root cargo new putioarr

# We want dependencies cached, so copy those first.
COPY Cargo.toml Cargo.lock /usr/src/putioarr/

# Set the working directory
WORKDIR /usr/src/putioarr

## Install target platform (Cross-Compilation) --> Needed for Alpine
RUN rustup target add $(cat /.platform)

# This is a dummy build to get the dependencies cached.
RUN RUSTFLAGS="-Ctarget-feature=-crt-static" cargo build --target $(cat /.platform) --release

# Now copy in the rest of the sources
COPY src /usr/src/putioarr/src/

## Touch main.rs to prevent cached release build
RUN touch /usr/src/putioarr/src/main.rs

# This is the actual application build.
RUN RUSTFLAGS="-Ctarget-feature=-crt-static" cargo build --target $(cat /.platform) --release

RUN cp /usr/src/putioarr/target/$(cat /.platform)/release/putioarr /usr/bin

### Rutime

FROM --platform=$BUILDPLATFORM ghcr.io/linuxserver/baseimage-alpine:edge
ARG TARGETARCH

# set version label
ARG BUILD_DATE
ARG VERSION

LABEL maintainer="wouterdebie"

RUN apk add musl gcc
COPY --from=builder /usr/bin/putioarr /usr/bin

# add local files
COPY root/ /

# ports and volumes
EXPOSE 9091
VOLUME /config
