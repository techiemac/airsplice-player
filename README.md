# airsplice-player

A video and music player for macOS, Linux and the Raspberry Pi 5, written in
Rust. The interface uses [GPUI](https://crates.io/crates/gpui), the
GPU-rendered toolkit behind the Zed editor, with Zed's One Dark look.
Decoding is done by Rust libraries and the operating system's hardware
decoders, with no FFmpeg, GStreamer or libVLC underneath.

Status: early (0.1). Tested on macOS, Linux and the Pi 5. The Windows build
has not been tested.

## Features

- **Playlist.** Drop files or folders anywhere in the window. Folders show
  as a collapsible tree and stay in sync with the disk: new files join,
  deleted ones leave. Entries show tag titles and durations.
- **Tabs.** Open files in background tabs, each paused on its first frame,
  and cut between them instantly. Up to 8.
- **External outputs.** Show the picture full screen on any number of
  displays, including the one the controls are on. Outputs come back by
  themselves when their display is reconnected.
- **Hardware decoding** on macOS and the Pi, and optional
  [GPU upscaling](#upscaling) for video smaller than the screen.
- **Audio output** to the system default or a chosen device (on Linux, any
  PipeWire or PulseAudio output, Bluetooth included). Surround AAC and Opus.
  With no output connected, video plays silently.
- **Images.** Stills show for a set time, then the playlist moves on.
  Animated GIF and WebP play and scrub like video.
- **Keyboard first**, with a command palette and a fuzzy file finder. Media
  keys and the system Now Playing view work (macOS Control Center, the
  Windows media overlay, Linux MPRIS).
- **Session restore.** The playlist, tabs, positions, volume, repeat mode
  and layout come back, paused, on the next launch.

Seeks land on the exact time asked for. The next entry is cued in the
background, so moving on is instant. If playback drops frames, freezes, or
the audio drops out, a warning appears in the status bar; hover it for the
numbers.

## Formats

| | |
|---|---|
| Containers | MP4/MOV, Matroska/WebM, Ogg, WAV, AIFF, CAF, MP3, FLAC |
| Audio | AAC-LC (up to 5.1), MP3, FLAC, Opus (up to 7.1), Vorbis, ALAC, PCM, ADPCM. Not HE-AAC. |
| Images | PNG, JPEG, WebP; animated GIF and WebP |

| Video | macOS | Raspberry Pi 5 | Other Linux, Windows |
|---|---|---|---|
| H.264 | hardware | software | software |
| HEVC 8/10-bit | hardware | hardware | not supported |
| AV1 8/10-bit | hardware on M3 and later, otherwise software | software | software |
| VP9 | hardware (4:2:0 only) | not supported | not supported |
| VP8 | software | software | software |

When the video codec is not supported, the audio still plays and the player
says why. The info panel (Cmd/Ctrl+R) shows the decoder in use.

Stills show for 10 seconds by default (set in the settings; 0 waits until
skipped) and are decoded at the size they are shown, not their file size.
An animation plays once, or loops with repeat one; each frame shows for its
own delay, with 10 ms or less treated as 100 ms, as browsers do. Animations
keep every frame in memory for scrubbing, so ones over 1 GB decoded are
refused.

## Performance

Apple M5 Max, decode and colour conversion, multiples of real time:

| Stream | Hardware | Software |
|---|---|---|
| 4K30 H.264 | 5.9x | 1.1x |
| 4K30 HEVC 10-bit | 7.1x | |
| 1080p60 H.264 | 7.7x | 2.3x |
| 1080p30 AV1 | | 15.6x |
| 1080p30 VP8 | | 13.7x |

Raspberry Pi 5, frames per second shown on screen, in a window on a 1080p
display:

| Codec | 1080p30 | 1080p60 | 4K30 |
|---|---|---|---|
| HEVC 8/10-bit (hardware) | 30 | 60 | 30 |
| VP8 | 30 | 60 | about 20 |
| AV1 | 30 | about 57 | about 13 |
| H.264 | 30 | about 30 | about 13 |

Software-decoded video, and HEVC on the Pi, is converted at half or
quarter size when every window showing it is that much smaller, so 4K in a
1080p window costs a quarter of the conversion work. When
the CPU cannot keep up, the player skips frames nothing else depends on,
then jumps to the next keyframe; audio never waits for video.

## Upscaling

When a video is smaller than the screen (a 1080p file on a 4K display or
projector), the player can scale it up on the GPU instead of leaving it to
the display's bilinear stretch. It is off by default; switch it on in the
settings, which name the upscaler in use.

| GPU | Upscaler |
|---|---|
| Macs that support MetalFX, including all Apple silicon | MetalFX spatial upscaling |
| Any other (Vulkan, Direct3D 12 or Metal), including the Pi | AMD FSR 1 (EASU, then RCAS sharpening), as wgpu compute shaders |
| None, or a software renderer | none |

Both rebuild each frame from its own pixels and sharpen edges without
inventing detail. On an image halved and scaled back up, both score about
30 dB PSNR against the original, against 25.5 dB for bilinear.

Frames grow at most 2x per side, only when that gains at least 10%, up to
the largest window showing the video. If upscaling takes over half a frame's
time for 10 frames in a row, it turns off for that file and the info panel
says so.

Time per frame on an M5 Max, including copies to and from the GPU
(`cargo run --release --example bench_upscale`); not yet measured elsewhere:

| Size | MetalFX | FSR 1 (on Metal) |
|---|---|---|
| 720p to 1440p | 1.5 ms | 1.5 ms |
| 1080p to 2160p | 2.7 ms | 3.2 ms |

## Building

The Rust version is pinned in `rust-toolchain.toml` (with rustup, run
`rustup toolchain install` once in the checkout).

```sh
cargo run --release -- [files or folders...]
```

- **macOS:** Xcode command line tools (not the Metal Toolchain; shaders
  compile at launch).
- **Linux:** a C++ compiler, libclang, and development packages for ALSA,
  fontconfig, freetype, Wayland, X11/xcb, xkbcommon and Vulkan. On Debian
  and Ubuntu:

  ```sh
  sudo apt install build-essential libclang-dev libasound2-dev \
    libfontconfig-dev libfreetype-dev libwayland-dev libx11-xcb-dev \
    libxkbcommon-x11-dev libxcb1-dev libvulkan-dev
  ```

- **Windows:** Visual Studio Build Tools with the C++ workload.

### Raspberry Pi

The player runs on 64-bit Raspberry Pi OS with the desktop. Building on the
Pi takes close to an hour, so `scripts/pi.sh` builds in Docker on another
machine and copies the result over SSH:

```sh
export PI_HOST=user@raspberrypi            # default pi@raspberrypi
export PI_KEY=~/.ssh/id_ed25519            # default: ssh's own
export PI_DIR=/home/user/airsplice-player  # where it lives on the Pi

scripts/pi.sh release          # build and copy the release binary
scripts/pi.sh run [files...]   # build, copy and launch it
scripts/pi.sh test [filter]    # run the tests on the Pi
```

Hardware HEVC uses the Pi's V4L2 stateless decoder (`rpi-hevc-dec`). Only
the Pi 5 has been tested; the Pi 4 has the same decoder and may work.

## Using it

| Action | macOS | Linux, Windows |
|---|---|---|
| Play / pause | Space or K | Space or K |
| Seek 5 s back / forward | Left / Right, or J / L | Left / Right, or J / L |
| Volume | Up / Down | Up / Down |
| Mute | M | M |
| Previous / next | P / N, or Cmd+Left / Right | P / N, or Ctrl+Left / Right |
| Repeat off / all / one | R | R |
| Full screen | F, or double-click the video | F, or double-click the video |
| External outputs on / off | Cmd+Shift+F | Ctrl+Shift+F |
| Open files | Cmd+O | Ctrl+O |
| Find in playlist | Cmd+P | Ctrl+P |
| Command palette | Cmd+Shift+P | Ctrl+Shift+P |
| Playlist / info panel | Cmd+B / Cmd+R | Ctrl+B / Ctrl+R |
| Settings | Cmd+, | Ctrl+, |
| Stop (closes the tab) | Cmd+. | Ctrl+. |
| Open in a new tab | Cmd+click, or Cmd+Enter in the finder | Ctrl+click, or Ctrl+Enter |
| Next / previous tab | Ctrl+Tab / Ctrl+Shift+Tab, or Cmd+Shift+] / [ | Ctrl+Tab / Ctrl+Shift+Tab, or Ctrl+Shift+] / [ |
| Close tab | Cmd+W | Ctrl+W |
| Tab 1 to 8 / last tab | Cmd+1 to 8 / Cmd+9 | Ctrl+1 to 8 / Ctrl+9 |

The settings choose the audio output, the external displays, upscaling and
how long stills show. Autoplay, in the playlist pane, moves on to the next
entry when one ends; repeat all loops the playlist and repeat one the entry.
Drag the playlist's right edge to resize it (double-click to reset).

Playing an entry uses the current tab. Opening it in a new tab (or with its
+ button) loads it in the background, paused. Switching tabs while playing
keeps playing in the new tab and pauses the old one where it was.

Files given on the command line replace the saved playlist. The session is
kept in `airsplice-player/session.json` in the platform's config directory
(`~/Library/Application Support`, `~/.config` or `%APPDATA%`).

- `AIRSPLICE_PLAYER_SOFTWARE_VIDEO=1` turns off hardware video decoding.
- `RUST_LOG=airsplice_player=debug` logs why hardware decoding was skipped,
  decode errors, and the frame rate shown.

## Why not FFmpeg

FFmpeg plays nearly everything and is the usual base for a player. This one
uses Rust libraries instead. Demuxers and decoders parse untrusted input,
the code where C media libraries have had most of their memory-safety bugs;
here containers, audio, AV1 and HEVC headers are parsed in Rust. Every
dependency is a Cargo crate, so there is no system FFmpeg to match, and a
Pi build is one `cargo build` in a container. And the player drives the
hardware decoders itself (VideoToolbox, the V4L2 request API), so frame
timing, reordering and colour conversion are under its control.

| Role | Crate |
|---|---|
| UI | [gpui](https://crates.io/crates/gpui) |
| Containers, most audio codecs | [symphonia](https://github.com/pdeljanov/Symphonia) |
| Opus | [ropus](https://github.com/0x4D44/ropus), a Rust port of libopus |
| AV1 | [rav1d](https://github.com/memorysafety/rav1d), a Rust port of dav1d |
| Audio output, resampling | [cpal](https://github.com/RustAudio/cpal), [rubato](https://github.com/HEnquist/rubato) |
| macOS hardware video | VideoToolbox through [objc2](https://github.com/madsmtm/objc2) |
| Pi hardware video | own V4L2 request-API backend, with [v4l2r](https://github.com/Gnurou/v4l2r) and the [cros-codecs](https://github.com/chromeos/cros-codecs) HEVC parser |
| Images | [image](https://github.com/image-rs/image), [gif](https://github.com/image-rs/image-gif) |
| Media keys, Now Playing | [souvlaki](https://github.com/Sinono3/souvlaki) |
| Upscaling | MetalFX through objc2; [wgpu](https://github.com/gfx-rs/wgpu) running a WGSL port of FSR 1 |

Two parts are not Rust: software H.264 uses Cisco's
[OpenH264](https://github.com/cisco/openh264) (C++, built from bundled
source), as there is no production-ready Rust H.264 decoder, and hardware
decoding runs in the operating system's drivers.

The cost is coverage: FFmpeg decodes far more formats, and some of these
crates are young. The workarounds are listed under
[Upstream issues](#upstream-issues).

## Known limitations

- No hardware video decoding on Windows (Media Foundation), on Linux PCs
  (VA-API), or for H.264 on the Pi 4. HEVC and VP9 files play audio only
  there.
- 4K HEVC full screen on a 4K display ran at about 18 fps on the Pi 5 when
  last measured.
- Upscaled frames make a round trip through the GPU, because GPUI draws
  video from main memory: upload, upscale, read back, upload again. That is
  cheap with unified memory (Apple silicon, the Pi) and costs bus bandwidth
  on discrete GPUs.
- Through PulseAudio's ALSA plugin, used only when the app cannot reach the
  sound server directly, sound can start about 2 s late (video waits, so
  sync is kept).

## Development

```sh
cargo test                  # tests on this machine
cargo clippy --all-targets  # lints
scripts/test-linux.sh       # clippy and tests on Linux, in Docker
scripts/pi.sh test          # tests on a Raspberry Pi
```

The tests decode fixtures with known content through every backend the
machine has, check timestamps and display order, run the playback engine
against real audio devices (A/V sync, pause, seeking in every container),
and drive the UI through its key bindings on GPUI's test platform. Tests
that need an audio device or hardware decoder skip, with a message, when
there is none. Fixtures are in `tests/fixtures`; `generate.sh` there
recreates them.

Tools in `examples/`:

| Example | Does |
|---|---|
| `bench_decode`, `bench_playback` | decode throughput and real-time delivery for any file |
| `bench_seek` | how quickly playback resumes after a click or drag on the seek bar |
| `bench_upscale` | time per frame for each upscaler the machine has |
| `cmp_decoders` | software against hardware output, frame by frame |
| `dump_frames` | raw decoded frames to a file, for comparing across machines |

| Path | Contents |
|---|---|
| `src/ui/` | views, theme, key bindings, session store, OS media controls |
| `src/media/player.rs` | playback engine: demux, decode and audio threads, seeking |
| `src/media/audio.rs` | audio output, resampling, the playback clock |
| `src/media/video/` | decoder trait, reorder buffer, colour conversion, backends |
| `src/media/video/hevc/` | Raspberry Pi V4L2 HEVC decoder |
| `src/media/mp4_timing.rs` | MP4 composition offsets and edit lists |
| `src/media/picture.rs` | stills and animations: outlines, frame timing, decoding |
| `src/media/upscale/` | MetalFX, and the FSR 1 compute shaders (`fsr1.wgsl`) |
| `vendor/` | patched upstream crates, each with a `PATCHED.md` |

### Upstream issues

Each of these is worth fixing upstream, after which the workaround can go.

- Symphonia 0.6.1's MP4 reader ignores composition offsets and edit lists,
  so its timestamps are decode times. `src/media/mp4_timing.rs` corrects
  them, which also trims AAC encoder delay.
- Symphonia's MP4 reader rejects `avcC`/`hvcC` records over 1 KiB, which
  excludes most x265 output. Patched in `vendor/symphonia-format-isomp4`.
- Symphonia's MP4 reader ignores keyframes when seeking, its Matroska
  reader can land past the target, and neither can seek after the end. The
  engine reads forward from the last keyframe before the target and
  reopens the file after end of stream.
- Symphonia's AAC decoder rejects more than two channels. Patched in
  `vendor/symphonia-codec-aac`, which also maps AAC's channel order (centre
  first, LFE last) to the output's.
- cros-codecs 0.0.6 panics on HEVC slices with more than 32 entry points
  (common in 4K). Patched in `vendor/cros-codecs`.
- The published rav1d 1.1.0 crate lacks a header its ARM assembly needs.
  Patched in `vendor/rav1d`.
- xattr 0.2.3 (via GPUI) does not build against current libc on Linux.
  Patched in `vendor/xattr`.
- OpenH264's threaded mode deadlocks on streams with B-frames, so software
  H.264 decodes on one thread.
