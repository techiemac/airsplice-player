# Patched copy of xattr 0.2.3

Unmodified upstream source (MIT/Apache-2.0) except `#![allow(deprecated)]`
in `src/lib.rs` (libc alias deprecations) and `src/sys/mod.rs`: on
Linux and Android `ENOATTR` is defined as `libc::ENODATA`. libc 0.2.190
removed its deprecated Linux `ENOATTR` alias (which equalled `ENODATA`), so
xattr 0.2.3, pulled in by gpui 0.2.2 (gpui_http_client, zed-async-tar), no
longer builds on Linux.

Remove this directory and its `[patch.crates-io]` entry once gpui depends on
an xattr release that builds with current libc.
