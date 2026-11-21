# VP8 test vectors

These are the VP8 conformance vectors from libvpx's test suite (the
`kVP8TestVectors` list in libvpx's `test/test_vectors.cc`), downloaded from

    https://storage.googleapis.com/downloads.webmproject.org/test_data/libvpx/<name>

Each `.ivf` holds a VP8 stream and each `.ivf.md5` the MD5 of every shown
frame as libvpx decodes it: the picture's Y rows, then U, then V, as 8-bit
4:2:0 without padding. `tests/vectors.rs` decodes every vector here and
compares every frame.

They are libvpx test data, distributed by the WebM project under its BSD
license (see https://www.webmproject.org/license/software/).

59 of the 62 vectors are committed (about 2.9 MB). The three largest
(`vp80-01-intra-1411`, `vp80-03-segmentation-04`,
`vp80-03-segmentation-1427`, about 0.8 MB together) are fetched by
`./fetch.sh`; every category is covered without them.

## Generated clips

The `gen-*.ivf` files are not from libvpx's suite. They were encoded here with
ffmpeg's libvpx encoder from synthetic sources (`testsrc2`, `mandelbrot`) to
cover what the suite barely touches, and their MD5s come from decoding them
with libvpx (`ffmpeg -c:v libvpx -i <file> -f framemd5 -`):

- `gen-profile1`, `gen-profile2`, `gen-profile3`: bitstream versions 1 to 3
  with motion (bilinear prediction, whole-pixel chroma vectors).
- `gen-altref`: two-pass encode with hidden alternate reference frames and
  sign bias.
- `gen-layers`: three temporal layers, so some frames update no reference
  (the `is_droppable` test uses it).
- `gen-odd-partitions`: 175x143 with four token partitions and
  `refresh_entropy_probs` off.
- `gen-threaded`, `gen-threaded-simple`: 705x401 (above the size that loop
  filters on a second thread), the first with four token partitions and
  sharpness 3 from noisy `mandelbrot`, the second bitstream version 1 (the
  simple filter).
