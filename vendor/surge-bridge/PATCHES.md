# DAW-AI patches

This is `surge-bridge` from the official `surge-synthesizer/surge-rs` repository
at commit `7bfeafc76d1c57860a177e9e076bed7ec764009a`.

It is licensed GPL-3.0-or-later; see `../surge-sys/LICENSE` for a copy.

DAW-AI constructs headless synthesizers with Surge XT's
`skipPatchLoadDataPathSentinel` and one process-lifetime plugin layer, matching
Surge XT's own headless test runner. The upstream bridge passes an empty data
path and allocates a different leaked plugin layer for every engine.

DAW-AI seeds the headless engine deterministically after construction so the
same project range produces identical PCM across byte-range playback renders.

DAW-AI exposes Surge XT's own parameter semantics used by compact model-facing
discovery: integer and Boolean choices with formatted labels, bipolar range,
tempo sync, and deactivation.

Headless parameter writes reject oscillator types whose required wavetable data
is absent from the current patch, preventing invalid native oscillator state.
