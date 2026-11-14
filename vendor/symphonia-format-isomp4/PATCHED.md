# Patched copy of symphonia-format-isomp4 0.6.1

Unmodified upstream source (MPL-2.0, see LICENSE) except:

- `src/atoms/hvcc.rs`, `src/atoms/avcc.rs`: the maximum decoder
  configuration record size is raised from 1 KiB to 64 KiB. x265 writes its
  encoder settings as an SEI message inside `hvcC`, so most x265-encoded MP4
  files exceeded the old limit and failed to open.

Remove this directory and the `[patch.crates-io]` entry once upstream lifts
the limit.
