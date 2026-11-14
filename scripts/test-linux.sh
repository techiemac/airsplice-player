#!/usr/bin/env bash
# Builds and tests the Linux version in Docker: clippy, then the full test
# suite. Audio plays into a PulseAudio null sink, so the audio-clock playback
# tests run in real time. Build output is cached in Docker volumes. Debug info
# is off and link jobs are limited: full debug links of GPUI exceed Docker
# Desktop's default 8 GB VM and the linker gets OOM-killed.
set -euo pipefail
cd "$(dirname "$0")/.."
docker build -q -t airsplice-player-linux -f scripts/linux.Dockerfile scripts >/dev/null
docker run --rm \
  -v "$PWD":/src -w /src \
  -v airsplice-player-linux-target:/target -e CARGO_TARGET_DIR=/target \
  -v airsplice-player-linux-cargo:/usr/local/cargo/registry \
  -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_BUILD_JOBS=4 \
  airsplice-player-linux \
  bash -c 'pulseaudio --daemonize --exit-idle-time=-1 --load=module-null-sink >/dev/null 2>&1 \
    && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked -- --nocapture'
