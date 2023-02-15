# syntax=docker/dockerfile:1.4

FROM rust:1-bullseye as builder
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y \
    libgstreamer-plugins-base1.0-dev \
    libgstreamer1.0-dev \
    build-essential \
    git \
    ffmpeg

WORKDIR /app
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    --mount=type=ssh \
    cargo build --release --bin videosrv \
    && cp target/release/videosrv /usr/local/bin/ 


# its important to use Ubuntu:focal here because libx264 was updated in newer versions and the may be
# incompatible with Broadway h264 decoder
FROM ubuntu:focal as gst-base
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y ca-certificates \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-libav \
    gstreamer1.0-tools \
    gstreamer1.0-plugins-base \
    && rm -rf /var/lib/apt/lists/*

FROM gst-base as videosrv
COPY --from=builder --link /usr/local/bin/videosrv /usr/local/bin/
COPY static /app/static/
WORKDIR /app
CMD videosrv --port ${VIDEO_SERVER_PORT}
