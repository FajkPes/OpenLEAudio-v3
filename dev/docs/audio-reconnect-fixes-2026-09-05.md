# Audio and reconnect fixes — 2026-09-05

Implemented in the Rust core. No headset, adapter, driver binding, saved pairing,
user setting, channel allocation, swap setting, CIS topology or LC3 packet routing
was changed during development. The original two-CIS startup priming remains.

## Changes

- Propagate ISO USB write failures to the existing reconnect path instead of reporting a clean stop.
- Preserve failed Encryption Change status. Distinguish authentication/key rejection from timeout, USB errors and disconnection. Keep the old bond until replacement pairing succeeds.
- Remove the unconditional two-failure cutoff for custom profiles. Deterministic capability validation still stops invalid configurations; transient failures follow the reconnect policy.
- Screen the left and right PCM timelines separately without changing any samples or their ordering. Noise confined to one ear is still rejected.
- Replace per-block ceiling resampling with continuous rational positioning for playback and PC microphone monitoring. Equal sample rates retain a bit-exact bypass; underruns do not consume input or advance phase.
- Disable silence-triggered ISO teardown while the headset microphone is enabled. The existing explicit multipoint hand-back remains.
- Retain the wake-up frame and capture endpoint across stream rebuild instead of discarding them. Hardware buffer overruns during lengthy setup can still lose subsequent audio; seamless wake-up is not claimed.
- Consume matching ATT battery Error Responses; reconnect after an unanswered 30-second ATT transaction instead of permanently wedging the read slot. Defer a manual refresh received during another read.
- Remove the unreachable in-loop idle branch. Actual silence release continues to use the separate wait-for-sound path.

## Verification

`dev/scripts/cargo.cmd test --offline --lib --bins --tests --quiet`

221 passed: 206 core unit tests, 9 agent tests, 6 LC3 integration tests.
Includes existing left/right, swap, mono, dual-CIS and codec round-trip tests.
New coverage checks opposite-polarity stereo, noise in one ear, fractional
resampling drift, continuity across block sizes, exact equal-rate sample order,
underrun state retention, key-error classification, microphone silence policy,
and battery error response matching.

The 44.1-to-48 kHz test consumes exactly 132300 source frames for 3 seconds
of output, retaining one stereo look-ahead frame for interpolation. That is
fixed look-ahead, not a per-block sample-rate error.

Release client built successfully with:
`dev/scripts/cargo.cmd build --offline --release --bin OpenLEAudio_Client`

No live radio/audio test was performed. Battery life and actual end-to-end
latency have not been measured. UI reorganization and changes to negotiated
radio parameters are deferred; they are not required for these fixes.

## Recovery

Original source files: `dev/backup/audit-fixes-2026-09-05/`.
The currently installed client is backed up there before installation.

Protocol reference for authentication/key errors:
https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/architecture%2C-mixing%2C-and-conventions/controller-error-codes.html

Installed into release/OpenLEAudio/OpenLEAudio2.Client.exe after the user closed the application. SHA256 matches the release build: 829009E35E5A238C9204FB9B9E289FBA9B03C3604574C310735BE06DB7073F3B. Previous executable saved as dev/backup/audit-fixes-2026-09-05/OpenLEAudio2.Client.before.exe.
