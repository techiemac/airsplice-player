# Patched copy of cros-codecs 0.0.6

Unmodified upstream source (BSD-3-Clause, see LICENSE) except `#![allow(warnings)]`
in `src/lib.rs` and `src/codec/h265/parser.rs`: slice headers with more than 32 entry point
offsets no longer index past `entry_point_offset_minus1` (a fixed 32-entry
array). A 4K stream using wavefront parallel processing has one entry point
per CTB row (33 or more), which panicked. The extra offsets are read and
discarded; `num_entry_point_offsets` still reports the real count.

Remove this directory and its `[patch.crates-io]` entry once upstream
handles large entry point counts.
