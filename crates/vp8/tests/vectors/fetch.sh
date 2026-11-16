#!/bin/sh
# Download the libvpx VP8 test vectors that are too large to commit. The
# conformance test picks up every .ivf in this directory.
set -eu
cd "$(dirname "$0")"
base=https://storage.googleapis.com/downloads.webmproject.org/test_data/libvpx
for name in vp80-01-intra-1411 vp80-03-segmentation-04 vp80-03-segmentation-1427; do
    for file in "$name.ivf" "$name.ivf.md5"; do
        [ -f "$file" ] || curl -sSfL -o "$file" "$base/$file"
    done
done
