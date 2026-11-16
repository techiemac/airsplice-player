# airsplice-player

A desktop video and music player written in Rust. The interface is built
with [GPUI](https://crates.io/crates/gpui), the GPU-rendered UI framework
behind the Zed editor, and uses Zed's One Dark look. Decoding uses Rust
libraries and the operating system's hardware decoders; there is no
FFmpeg, GStreamer or libVLC underneath.

**Status:** early (0.1). Developed and tested on macOS and Linux, including
the Raspberry Pi 5. The Windows build has not been tested yet.

## Features

- Playlist with tag titles, durations and total length; drop files or
  folders anywhere in the window. Added folders stay in sync: files copied
  into them later join the playlist, deleted ones leave it. Folders and
  subfolders show as a collapsible tree, each folder's own files first; a
  folder opens when playback reaches an entry inside it
- Tabs: open files in background tabs, each loaded and paused on its first
  frame, then cut between them instantly
- External outputs: the picture full screen on any number of displays,
  including the one the controls are on. Choose them in the settings and
  switch them on and off from the status bar; the choice is remembered, and
  each output comes back by itself when its display is reconnected
- Audio output: the system default, or a device chosen in the settings
- Real-time GPU upscaling (optional) for video smaller than the screen:
  MetalFX on Apple GPUs, AMD FSR 1 on other GPUs; the settings name the
  upscaler in use (see [Upscaling](#upscaling))
- Images: PNG, JPEG and WebP stills, and animated GIF and WebP that play
  and scrub like video
- Command palette and fuzzy file finder, keyboard-first controls
- Hardware media keys and the system Now Playing view (macOS Control
  Center, Windows media overlay, Linux MPRIS)
- Session restore: playlist, open tabs and their positions, volume, repeat
  mode and panel layout come back (paused) on the next launch
- Hardware video decoding on macOS and Raspberry Pi
- Surround Opus; AAC encoder delay trimmed using MP4 edit lists
- Seeks land on the requested time, decoding forward from the previous
  keyframe

## Why not FFmpeg

FFmpeg plays nearly everything and is the usual foundation for a player.
This project builds on Rust libraries instead, for three reasons:

- **Memory safety in the parsing code.** Demuxers and decoders read
  untrusted input, and media libraries written in C have a long history of
  memory-safety bugs in exactly that code. Here, container parsing, audio
  decoding, AV1 decoding and HEVC header parsing are written in Rust.
- **One build.** Every dependency is a Cargo crate. There is no system
  FFmpeg to install or match versions against, and cross-compiling for a
  Raspberry Pi is a single `cargo build` in a container.
- **Direct hardware access.** The player talks to the platform's decoder
  itself (VideoToolbox on macOS, the V4L2 request API on Linux), so frame
  timing, reordering and colour conversion are under its control.

The stack:

| Role | Crate |
|---|---|
| UI | [gpui](https://crates.io/crates/gpui) |
| Containers and most audio codecs | [symphonia](https://github.com/pdeljanov/Symphonia) |
| Opus | [ropus](https://github.com/0x4D44/ropus), a Rust port of libopus |
| AV1 | [rav1d](https://github.com/memorysafety/rav1d), a Rust port of dav1d |
| Audio output and resampling | [cpal](https://github.com/RustAudio/cpal), [rubato](https://github.com/HEnquist/rubato) |
| macOS hardware video | VideoToolbox through the [objc2](https://github.com/madsmtm/objc2) bindings |
| Raspberry Pi hardware video | own V4L2 request-API backend, with [v4l2r](https://github.com/Gnurou/v4l2r) and the [cros-codecs](https://github.com/chromeos/cros-codecs) HEVC parser |
| Images | [image](https://github.com/image-rs/image) and [gif](https://github.com/image-rs/image-gif) |
| Media keys and Now Playing | [souvlaki](https://github.com/Sinono3/souvlaki) |
| GPU upscaling | MetalFX through objc2; [wgpu](https://github.com/gfx-rs/wgpu) running a WGSL port of AMD FSR 1 |

Two parts are not Rust: H.264 software decoding uses Cisco's
[OpenH264](https://github.com/cisco/openh264) (C++, compiled from bundled
source), since there is no production-ready Rust H.264 decoder yet; and
hardware decoding is done by the operating system's drivers.

The cost is coverage. FFmpeg decodes far more formats, and some of these
crates are young; the workarounds this project carries are listed under
[Upstream issues worked around](#upstream-issues-worked-around).

## Format support

**Containers:** MP4/MOV, Matroska/WebM, Ogg, WAV, AIFF, CAF, MP3, FLAC.

**Audio:** AAC-LC (up to 5.1), MP3, FLAC, Opus (up to 7.1), Vorbis, ALAC,
PCM, ADPCM, on every platform. HE-AAC is not supported yet.

**Video:**

| Codec | macOS | Raspberry Pi 5 | Other Linux, Windows |
|---|---|---|---|
| H.264 | hardware | software | software |
| HEVC 8/10-bit | hardware | hardware | not supported |
| AV1 8/10-bit | hardware on M3 and later, otherwise software | software | software |
| VP9 | hardware | not supported | not supported |
| VP8 | not supported | not supported | not supported |

When a file's video codec is not supported, its audio still plays and the
player says why. The info panel (Cmd/Ctrl+R) shows which decoder is in use.

**Images:** PNG, JPEG and WebP are stills. Each shows for the number of
seconds set in the settings (10 by default; 0 keeps it up until skipped),
with a seek bar like a clip of that length, then the playlist moves on. Animated GIF and WebP play
like video: they have a duration, play
and pause, and scrub on the seek bar, with each frame shown for its delay
(10 ms or less counts as 100 ms, as in browsers). An animation plays once;
repeat one loops it. Stills are decoded no larger than the biggest window
showing them, so a large photo costs memory for its display size, not its
file size; animations keep every decoded frame for instant scrubbing, so
ones over 1 GB decoded are refused.

### Performance

Apple M5 Max, release build, decode and colour conversion, multiples of
real time:

| Stream | Hardware | Software |
|---|---|---|
| 4K30 H.264 | 5.9x | 1.1x |
| 4K30 HEVC 10-bit | 7.1x | |
| 1080p60 H.264 | 7.7x | 2.3x |
| 1080p30 AV1 | | 7.3x |

Raspberry Pi 5, frames shown on screen by the app:

| Stream | Result |
|---|---|
| 1080p60 HEVC | 60 fps |
| 1080p60 H.264 (software), full screen | about 30 fps |
| 4K30 HEVC 10-bit, window up to 1920x1080 | 30 fps |
| 4K30 HEVC 10-bit, full screen on a 4K display | about 18 fps |

Video the CPU cannot decode in time first skips frames that no other frame
refers to, then jumps to the next keyframe if it is still behind; audio
never waits for it.

## Upscaling

When a video is smaller than the screen showing it (a 1080p file on a 4K
display or projector), the player can scale each frame up on the GPU instead
of leaving it to the display's plain bilinear stretch. Switch it on in the
settings; it is off by default.

The upscaler depends on the hardware, and the settings and the info panel
name the one in use:

| Hardware | Upscaler |
|---|---|
| Apple silicon, and other Macs whose GPU MetalFX supports | MetalFX spatial upscaling, Apple's real-time upscaler |
| Any other GPU: Linux and Windows PCs (Vulkan or Direct3D 12), the Raspberry Pi (Vulkan), Macs without MetalFX (Metal) | AMD FidelityFX Super Resolution 1 (EASU upscaling, then RCAS sharpening) as wgpu compute shaders |
| No GPU, or only a software renderer | None; the setting is unavailable |

Both are spatial upscalers: each frame is rebuilt from its own pixels,
reconstructing and sharpening edges without inventing detail, so the picture
stays faithful to the source. Generative (diffusion) upscalers can recover
more apparent detail, but they alter content and are far from real time.

- Frames grow at most 2x per side (the display's scaling does the rest) and
  only when that gains at least 10%.
- The target size is the largest window showing the video, external outputs
  included.
- If upscaling takes more than half of a frame's display time for 10 frames
  in a row, it stops for that file and the info panel says so, so it never
  slows playback down.
- On a test image that was halved and scaled back up, both score about 30 dB
  PSNR against the original, against 25.5 dB for bilinear scaling.

Time per frame on an Apple M5 Max, including the copies to and from the GPU
(`cargo run --release --example bench_upscale`):

| Size | MetalFX | FSR 1 (on Metal) |
|---|---|---|
| 720p to 1440p | 1.5 ms | 1.5 ms |
| 1080p to 2160p | 2.7 ms | 3.2 ms |

Not yet measured on the Raspberry Pi, Linux or Windows.

## Building

The Rust version is pinned in `rust-toolchain.toml`; with rustup, run
`rustup toolchain install` once in the checkout to get it.

```sh
cargo run --release -- [files or folders...]
```

- **macOS:** Xcode command line tools. The separate Metal Toolchain is not
  needed (shaders compile at launch).
- **Linux:** a C++ compiler, libclang, and the development packages for
  ALSA, fontconfig, freetype, Wayland, X11/xcb, xkbcommon and Vulkan. On
  Debian and Ubuntu:

  ```sh
  sudo apt install build-essential libclang-dev libasound2-dev \
    libfontconfig-dev libfreetype-dev libwayland-dev libx11-xcb-dev \
    libxkbcommon-x11-dev libxcb1-dev libvulkan-dev
  ```

- **Windows:** Visual Studio Build Tools with the C++ workload.

### Raspberry Pi

The Pi 5 runs the player on Raspberry Pi OS (64-bit, desktop). Building on
the Pi itself works but takes close to an hour, so `scripts/pi.sh`
cross-builds in Docker on another machine and copies the result over SSH:

```sh
export PI_HOST=user@raspberrypi      # optional: default pi@raspberrypi
export PI_KEY=~/.ssh/id_ed25519      # optional: SSH key to use
export PI_DIR=/home/user/airsplice-player  # optional: install directory on the Pi

scripts/pi.sh release                # build and copy the release binary
scripts/pi.sh run [files...]         # build, copy and launch it
scripts/pi.sh test [filter]          # run the test suite on the Pi
```

Hardware HEVC uses the Pi's stateless V4L2 decoder (`rpi-hevc-dec`) through
the kernel's request API. Only the Pi 5 has been tested; the Pi 4 has the
same decoder and may work.

## Usage

| Action | macOS | Linux / Windows |
|---|---|---|
| Play / pause | Space or K | Space or K |
| Seek back / forward 5 s | Left / Right, or J / L | Left / Right, or J / L |
| Volume | Up / Down, or scroll on the slider | Up / Down, or scroll on the slider |
| Mute | M | M |
| Previous / next track | P / N, Cmd+Left / Cmd+Right | P / N, Ctrl+Left / Ctrl+Right |
| Repeat off / all / one | R | R |
| Full screen | F, or double-click the video | F, or double-click the video |
| Upscaling on / off | Settings | Settings |
| How long stills show | Settings: type the seconds, or Left / Right | Settings: type the seconds, or Left / Right |
| Autoplay (move on to the next entry) | Autoplay in the playlist pane | Autoplay in the playlist pane |
| Choose output displays | Settings | Settings |
| Choose the audio output | Settings | Settings |
| Outputs on / off | Cmd+Shift+F, or the monitor button in the status bar | Ctrl+Shift+F, or the monitor button |
| Settings | Cmd+, the gear in the status bar, or the app menu | Ctrl+, or the gear in the status bar |
| Open files | Cmd+O | Ctrl+O |
| Command palette | Cmd+Shift+P | Ctrl+Shift+P |
| Find in playlist | Cmd+P | Ctrl+P |
| Toggle playlist / info panel | Cmd+B / Cmd+R | Ctrl+B / Ctrl+R |
| Resize the playlist | Drag its right edge (double-click to reset) | Same |
| Stop (closes the tab) | Cmd+. | Ctrl+. |
| Open in new tab | Cmd+click a playlist entry, or Cmd+Enter in the finder | Ctrl+click, or Ctrl+Enter |
| Next / previous tab | Cmd+Shift+] / [, or Ctrl+Tab / Ctrl+Shift+Tab | Ctrl+Shift+] / [, or Ctrl+Tab / Ctrl+Shift+Tab |
| Close tab | Cmd+W | Ctrl+W |
| Go to tab 1 to 8 / last tab | Cmd+1 to Cmd+8 / Cmd+9 | Ctrl+1 to Ctrl+8 / Ctrl+9 |

When an entry ends, Autoplay moves on to the next one; with it off,
playback stops there. Repeat all loops the playlist, repeat one loops the
entry. The next entry is opened and cued in the background while the
current one plays, so moving on is instant.

Opening a file from the playlist plays it in the current tab. Opening it in
a new tab (also the + button on each entry) loads it in the background,
paused on its first frame; up to 8 tabs can be open. Switching tabs while
playing cuts to the new tab and keeps playing; the tab you leave pauses where
it was.

Files passed on the command line replace the saved playlist. The session is
stored in `airsplice-player/session.json` under the platform's config directory
(`~/Library/Application Support` on macOS, `~/.config` on Linux,
`%APPDATA%` on Windows).

Environment variables:

- `AIRSPLICE_PLAYER_SOFTWARE_VIDEO=1` disables hardware video decoding.
- `RUST_LOG=airsplice_player=debug` logs why hardware decoding was skipped, decode
  errors, and the presented frame rate.

## Development

```sh
cargo test                  # tests on this machine
cargo clippy --all-targets  # lints
scripts/test-linux.sh       # clippy and tests on Linux, in Docker
scripts/pi.sh test          # tests on a Raspberry Pi (see above)
```

The tests decode small fixtures with known content through every backend
available on the machine, check timestamps and display order, run the full
playback engine against real audio devices (A/V sync, pause, seeking in
every container), and drive the UI through its real key bindings on GPUI's
test platform. Tests that need an audio device or a hardware decoder skip,
with a message, when there is none. Fixtures live in `tests/fixtures`;
`generate.sh` there recreates them.

`examples/` has tools for working on decoders: `bench_decode` and
`bench_playback` measure throughput and real-time delivery for any file,
`bench_seek` times how quickly playback resumes after clicking or dragging
the seek bar, `bench_upscale` times each upscaler the machine offers,
`cmp_decoders` compares software and hardware output frame by frame, and
`dump_frames` writes raw decoded frames to a file for comparing decoders
across machines.

### Layout

| Path | Contents |
|---|---|
| `src/ui/` | GPUI views, theme, key bindings, session store, OS media controls |
| `src/media/player.rs` | playback engine: demux, decode and audio threads, clock, seeking |
| `src/media/audio.rs` | audio output, resampling, the playback clock |
| `src/media/video/` | video decoder trait, reorder buffer, colour conversion, backends |
| `src/media/video/hevc/` | Raspberry Pi V4L2 HEVC decoder |
| `src/media/mp4_timing.rs` | MP4 composition offsets and edit lists |
| `src/media/picture.rs` | PNG, JPEG, GIF and WebP: outlines, frame timing, decoding |
| `src/media/upscale/` | GPU upscaling: MetalFX, and the FSR 1 compute shaders (`fsr1.wgsl`) |
| `vendor/` | patched copies of upstream crates (each has a `PATCHED.md`) |

### Upstream issues worked around

These are worth fixing upstream; each workaround can go once they are.

- Symphonia 0.6.1's MP4 reader ignores composition offsets and edit lists,
  so its timestamps are decode times. `src/media/mp4_timing.rs` corrects
  them.
- Symphonia's MP4 reader rejects `avcC`/`hvcC` records over 1 KiB, which
  excludes most x265 output. Patched in `vendor/symphonia-format-isomp4`.
- Symphonia's MP4 reader ignores keyframes when seeking (it lands on the
  first packet at or after the requested time), its Matroska reader can
  land past the target, and neither can seek after reaching the end. The
  engine reads forward to the latest keyframe before the target, backing
  off when there is none, and reopens the file after end of stream.
- cros-codecs 0.0.6 panics on HEVC slices with more than 32 entry points
  (common in 4K). Patched in `vendor/cros-codecs`.
- Symphonia's AAC decoder rejects more than two channels. Patched in
  `vendor/symphonia-codec-aac`, which maps AAC's channel order (centre
  first, LFE last) to the output's.
- xattr 0.2.3 (via GPUI) does not build against current libc on Linux.
  Patched in `vendor/xattr`.
- The published rav1d 1.1.0 crate is missing a header its ARM assembly
  needs, so it is built without assembly.
- OpenH264 deadlocks in threaded mode with the low-latency API, so software
  H.264 decodes on one thread.

## Known limitations

- No hardware video decoding on Windows (Media Foundation), on Linux PCs
  (VA-API), or for H.264 on the Raspberry Pi 4. HEVC and VP9 files play
  audio only on those systems.
- On the Pi 5, 4K HEVC full screen on a 4K display runs at about 18 fps.
- Upscaled frames make a round trip through the GPU, because the UI toolkit
  draws video from main memory: each frame is uploaded, upscaled, read back
  and uploaded again for display. That is cheap on unified-memory machines
  (Apple silicon, the Pi) and costs bus bandwidth on discrete GPUs.
- With PulseAudio's ALSA plugin, sound can start about 2 s late (video
  waits for it, so sync is kept). PipeWire does not show this.
