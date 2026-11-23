# Linux build and test environment (same packages as the CI job).
# Keep in step with rust-toolchain.toml.
FROM rust:1.100-trixie
# mesa-vulkan-drivers: lavapipe, a software Vulkan GPU, so the FSR upscaler
# tests run without GPU hardware.
RUN apt-get update && apt-get install -y --no-install-recommends \
      libasound2-dev libfontconfig-dev libfreetype-dev libwayland-dev \
      libx11-xcb-dev libxkbcommon-x11-dev libxkbcommon-dev libxcb1-dev \
      libvulkan-dev pkg-config libclang-dev \
      pulseaudio libasound2-plugins \
      mesa-vulkan-drivers \
    && rm -rf /var/lib/apt/lists/*
RUN rustup component add clippy
# Route ALSA's default device to PulseAudio, which `cargo xtask linux` starts with
# a null sink: a real-time audio device without sound hardware.
RUN printf 'pcm.!default { type pulse }\nctl.!default { type pulse }\n' > /etc/asound.conf
