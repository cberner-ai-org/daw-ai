# DAW-AI patches

This is `surge-sys` from the official `surge-synthesizer/surge-rs` repository at
commit `7bfeafc76d1c57860a177e9e076bed7ec764009a`.

DAW-AI uses the system Git executable to fetch the pinned Surge XT revision and
its recursive submodules, avoiding a second embedded Git and TLS stack in the
Rust dependency graph.

DAW-AI exports every CMake `-D` definition to the bridge compilation. Upstream
only exported definitions beginning with `SURGE`, so the bridge and engine saw
different feature macros and compiled incompatible C++ class layouts.

DAW-AI pins the cloned Surge XT engine to commit
`3c64680043bf8ef65cfcc6019e847c3f655c14fc`, the engine revision current when
the Rust binding commit was published. Building the alpha binding against a
later nightly changes native C++ class layouts and causes memory corruption.

DAW-AI allows `improper_ctypes` only in the raw binding module. Bindgen models
opaque, 16-byte-aligned C++ storage with `u128`, whose Rust-to-C value ABI is
not stable. The bridge passes pointers to those C++-owned types rather than
passing `u128` values, and replacing the generated storage could break its
required size or alignment.

The `idForParameter` plumbing writes its C++ ID through an output pointer
instead of returning the opaque ID by value. This keeps the unstable `u128`
value ABI out of the C boundary covered by the lint allowance. The native
by-value member is blocklisted from generated bindings so raw consumers cannot
bypass that pointer wrapper.
