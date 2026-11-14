# Patched copy of symphonia-codec-aac 0.6.1

Unmodified upstream source (MPL-2.0, see LICENSE) except:

- `src/aac/mod.rs`, `src/aac/cpe.rs`: AAC-LC with more than two channels
  (channel configurations 3 to 7, up to 7.1) is decoded instead of being
  rejected as "aac too complex". The element decoding already handled any
  number of channels; the patch adds the mapping from the bitstream's
  channel order (ISO/IEC 14496-3 table 1.19: centre first, LFE last) to
  the output buffer's plane order. HE-AAC (SBR), 960-sample frames and
  layouts given by a program config element (channel configuration 0)
  are still unsupported. 5.1 is covered by a test; the 7.1 mapping follows
  the same table but has no fixture (ffmpeg writes 7.1 as configuration 0).

Remove this directory and the `[patch.crates-io]` entry once upstream
supports multichannel AAC.
