#!/usr/bin/env bash
# Regenerates the test fixtures. The player does not use ffmpeg; it is only a
# convenient way to produce known-content files in every codec we test.
#
# Video content: 1 s red, 1 s green, 1 s blue, 320x240 at 25 fps.
# Audio content: 440 Hz sine.
set -euo pipefail
cd "$(dirname "$0")"
q=(-hide_banner -loglevel error -y)
V='color=c=red:s=320x240:r=25:d=1[a];color=c=lime:s=320x240:r=25:d=1[b];color=c=blue:s=320x240:r=25:d=1[c];[a][b][c]concat=n=3:v=1:a=0,format=yuv420p[v]'
V10=${V/yuv420p/yuv420p10le}

ffmpeg "${q[@]}" -f lavfi -i "sine=f=440:r=44100:d=3" -filter_complex "$V" -map "[v]" -map 0:a \
  -c:v libx264 -profile:v high -bf 2 -g 25 -c:a aac -ac 2 -b:a 64k -movflags +faststart h264_aac.mp4
ffmpeg "${q[@]}" -f lavfi -i "sine=f=440:r=48000:d=3" -filter_complex "$V" -map "[v]" -map 0:a \
  -c:v libsvtav1 -preset 10 -g 25 -c:a libopus -ac 2 -b:a 48k av1_opus.webm
ffmpeg "${q[@]}" -f lavfi -i "sine=f=440:r=48000:d=3" -filter_complex "$V10" -map "[v]" -map 0:a \
  -c:v libsvtav1 -preset 10 -g 25 -c:a libopus -af "pan=5.1|FL=c0|FR=c0|FC=c0|LFE=c0|BL=c0|BR=c0" \
  -b:a 96k av1_10bit_opus51.mkv
ffmpeg "${q[@]}" -f lavfi -i "sine=f=440:r=44100:d=3" -filter_complex "$V" -map "[v]" -map 0:a \
  -c:v libx264 -bf 2 -g 25 -c:a libmp3lame -b:a 64k h264_mp3.mkv
ffmpeg "${q[@]}" -f lavfi -i "sine=f=440:r=44100:d=3" -filter_complex "$V" -map "[v]" -map 0:a \
  -c:v libx265 -x265-params log-level=error -tag:v hvc1 -c:a aac -b:a 64k hevc_aac.mp4  # keeps x265's SEI: hvcC > 1 KiB
ffmpeg "${q[@]}" -filter_complex "$V" -map "[v]" -c:v libx264 -bf 2 -g 25 video_only.mp4

ffmpeg "${q[@]}" -f lavfi -i "sine=f=440:r=96000:d=2" -ac 1 -c:a flac mono_96k.flac
ffmpeg "${q[@]}" -f lavfi -i "sine=f=440:r=44100:d=2" -ac 2 -c:a libmp3lame -b:a 96k \
  -metadata title="Test Tone" -metadata artist="Fixture Band" -metadata album="Fixtures" tagged.mp3
ffmpeg "${q[@]}" -f lavfi -i "sine=f=440:r=48000:d=2" -ac 2 -c:a libopus -b:a 48k stereo.opus
ffmpeg "${q[@]}" -f lavfi -i "sine=f=440:r=44100:d=2" -ac 2 -c:a vorbis -strict -2 stereo.ogg
ffmpeg "${q[@]}" -f lavfi -i "sine=f=440:r=22050:d=2" -ac 2 -c:a pcm_s16le stereo_22k.wav

# Codecs decoded by OS hardware decoders, and one nothing here decodes.
ffmpeg "${q[@]}" -f lavfi -i "sine=f=440:r=48000:d=3" -filter_complex "$V" -map "[v]" -map 0:a \
  -c:v libvpx-vp9 -b:v 200k -g 25 -c:a libopus -b:a 48k vp9_opus.webm
ffmpeg "${q[@]}" -f lavfi -i "sine=f=440:r=48000:d=3" -filter_complex "$V10" -map "[v]" -map 0:a \
  -c:v libx265 -x265-params log-level=error:info=0 -profile:v main10 -c:a libopus -b:a 48k hevc_10bit_opus.mkv
ffmpeg "${q[@]}" -f lavfi -i "sine=f=440:r=48000:d=3" -filter_complex "$V" -map "[v]" -map 0:a \
  -c:v libvpx -b:v 200k -g 25 -c:a libopus -b:a 48k vp8_opus.webm

# Moving content with B-frames, to check decoders emit display order.
ffmpeg "${q[@]}" -f lavfi -i "testsrc2=s=160x120:r=25:d=2" -c:v libx264 -bf 3 -b_strategy 0 -g 25 -pix_fmt yuv420p bframes_h264.mp4
ffmpeg "${q[@]}" -f lavfi -i "testsrc2=s=160x120:r=25:d=2" -c:v libx265 -x265-params log-level=error:info=0:bframes=3:b-adapt=0:keyint=25 -pix_fmt yuv420p bframes_hevc.mkv

# Seeking: HEVC in MP4 with B-frames, a keyframe every 2 s and audio. The
# keyframes fall on whole seconds, where a seek lands one packet past them.
ffmpeg "${q[@]}" -f lavfi -i "testsrc2=s=160x120:r=25:d=10" -f lavfi -i "sine=f=440:r=44100:d=10" \
  -c:v libx265 -x265-params log-level=error:info=0:bframes=3:b-adapt=0:keyint=50:min-keyint=50:scenecut=0 \
  -tag:v hvc1 -pix_fmt yuv420p -c:a aac -b:a 32k seek_hevc.mp4
