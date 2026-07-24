# Surge XT vendor boundary

The three crates in this directory are a single patched dependency boundary:
`surge-rs`, `surge-bridge`, and `surge-sys`. They come from
`surge-synthesizer/surge-rs` commit
`7bfeafc76d1c57860a177e9e076bed7ec764009a`; `surge-sys` in turn pins Surge XT
commit `3c64680043bf8ef65cfcc6019e847c3f655c14fc`.

Each crate's `PATCHES.md` records the local behavior that must survive an
upstream update. Keep application code behind `src/surge.rs`; do not import
binding internals elsewhere.

To update this boundary:

1. Update all three crates from the same `surge-rs` revision.
2. Reapply and document every patch in the three `PATCHES.md` files.
3. Pin the matching Surge XT revision in `surge-sys`.
4. Update the revisions in this file and `Cargo.toml`.
5. Run `just test`. The vendor-boundary test checks the pins and patched API,
   while the Surge integration tests exercise multiple engines, factory
   patches, native effects, deterministic range rendering, and audio input.

The vendored crates and linked Surge XT engine are GPL-3.0-or-later. A
distributed combined binary must comply with that license.
