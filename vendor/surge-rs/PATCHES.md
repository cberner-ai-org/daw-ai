# DAW-AI patches

This is `surge-rs` from the official `surge-synthesizer/surge-rs` repository at
commit `7bfeafc76d1c57860a177e9e076bed7ec764009a`.

DAW-AI uses the patched `git2` 0.21 build dependency with only HTTPS transport
enabled.

DAW-AI also wraps the parameter-semantic and native-choice queries exposed by
the patched bridge, including Surge XT's native modulation-target validation.
Parameter writes use the bridge's headless-state safety check.
The wrapper also exposes restoration of native tempo-sync and deactivation
flags for preset effect parameters.
It also exposes the native LFO start phase for project-time-aligned streaming.
