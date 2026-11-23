# Patched copy of rav1d 1.1.0

Unmodified upstream source (BSD-2-Clause, see COPYING) except:

- `src/arm/asm-offsets.h` is added. The ARM assembly includes it, but the
  published crate leaves it out, so the crate cannot build with its `asm`
  feature on ARM. The file is upstream's, from the `v1.1.0` tag
  (https://github.com/memorysafety/rav1d/blob/v1.1.0/src/arm/asm-offsets.h).

- `lib.rs` allows three lints (function pointer comparisons, lifetime
  syntax, dead code). Cargo caps lints for registry crates but not for path
  dependencies, so without this every build prints 53 warnings about
  upstream code.

The header's constants are byte offsets into structs the assembly reads, so they must
match this source's layouts:

- `FGD_*`: `Dav1dFilmGrainData`, which is public; the player's tests check
  every offset with `offset_of!` (`src/media/video/av1.rs`).
- `RMVSF_*`: `AsmRefMvsFrame` in `src/refmvs.rs`, crate-private, so checked
  by hand from its `repr(C)` layout: `iw8` 16, `ih8` 20, `mfmv_ref` 53,
  `mfmv_ref2cur` 56, `mfmv_ref2ref` 68, `n_mfmvs` 152, `rp_ref` 168,
  `rp_proj` 176, `rp_stride` 184, `n_tile_threads` 200 (a leading pointer,
  six `i32`, 21 bytes of `u8` arrays, then `i32` and pointer alignment).

Remove this directory and the `[patch.crates-io]` entry once a published
release includes the header.
