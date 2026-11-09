FROM rust:latest AS builder
ARG TARGETPLATFORM
ARG TARGETARCH
WORKDIR /usr/src

RUN apt update && apt upgrade -y

# Set the right architecture
COPY docker/build.sh /usr/local/bin

# Create blank project
RUN USER=root cargo new putioarr

# We want dependencies cached, so copy those first.
COPY Cargo.toml Cargo.lock /usr/src/putioarr/

# Set the working directory
WORKDIR /usr/src/putioarr

# Cache dependency build
RUN TARGETPLATFORM=$TARGETPLATFORM TARGETARCH=$TARGETARCH /usr/local/bin/build.sh

# Now copy in the rest of the sources
COPY src /usr/src/putioarr/src/

## Touch main.rs to prevent cached release build
RUN touch /usr/src/putioarr/src/main.rs

# This is the actual application build.
RUN TARGETPLATFORM=$TARGETPLATFORM TARGETARCH=$TARGETARCH /usr/local/bin/build.sh -c /usr/bin

### Runtime

FROM ghcr.io/linuxserver/baseimage-alpine:3.24
ARG TARGETPLATFORM
ARG TARGETARCH

# set version label
ARG BUILD_DATE
ARG VERSION

LABEL maintainer="wouterdebie"
# The base image sets this to linuxserver's repo; GHCR uses it to link the package.
LABEL org.opencontainers.image.source="https://github.com/wouterdebie/putioarr"
LABEL org.opencontainers.image.architecture="${TARGETARCH}"

COPY --from=builder /usr/bin/putioarr /usr/bin

# add local files
COPY root/ /

# ports and volumes
EXPOSE 9091
VOLUME /config
