# Setup and discovered-device fixes — 2026-09-05

Deployed to release/OpenLEAudio, without creating backups.

- Setup refreshes PnP bindings and requirements every 3 seconds while visible, and on window activation. It preserves adapter selection and avoids overlapping enumeration or queued environment checks.
- Missing dependency, cable installation/configuration and adapter-switch actions receive red borders/text. Adapter status uses the fresh PnP service rather than an old environment report.
- A detected binding change shows a persistent restart banner outside the scroll area. Step 4 always contains a restart button. Launching an installer alone no longer asserts that it succeeded.
- Restart passes the old PID to the replacement instance; it waits for the old process to exit before acquiring the single-instance mutex and opening the audio core.
- Discovered names survive nameless advertisements and subsequent scans in the same app session. Named devices appear first, with stable discovery order within each group. RSSI changes update only signal text, preserving the row and button. LE Audio discovery is retained across advertisements that omit it.
- No Rust core, driver package, channel mapping or audio configuration changes in this update.

Validation:
- Release build/publish succeeded.
- python dev/tests/discovery-regression.py passed grouping, one-time promotion, 100 repeated advertisement pairs, case-insensitive addresses, sticky names/capabilities, RSSI notification and row reuse, rescan and paired-row scenarios using production methods.
- Production SetupAPI enumeration on this machine returned USB\VID_0B05&PID_1D70 / WINUSB.
- All published file hashes matched staging after deployment; existing audio core hash unchanged.
- Installed application started using --restart-after with an exiting parent process; GUI process remained responsive and its audio core started. This is a startup/handoff smoke check, not an end-to-end button interaction or another physical driver switch.
