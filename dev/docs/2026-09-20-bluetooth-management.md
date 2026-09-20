# Bluetooth management changes — 2026-09-20

## Behavior

- Validate peer L2CAP connection parameter requests, reject invalid timing, and submit accepted parameters to the controller. Also answer LE Remote Connection Parameter Request events. Report actual update results in diagnostics.
- Correct supervision timeout minimum: 10 units of 10 ms, not 100 units.
- Share HCI command credits across synchronous commands and queued peer replies. Background acknowledgements cannot satisfy a synchronous command with the same opcode.
- Recover audio on the retained ACL using live settings instead of three fixed retries. Default: enabled, two-second pause, unlimited recovery window. Setup takes additional time. Disconnect/Stop cancels waiting. Actual ACL loss returns to full Bluetooth reconnection. With multipoint yielding enabled, recovery waits while the peer reports media unavailable.
- Full Bluetooth reconnect remains separate. New installations default to a one-second pause and a 15-minute retry window. Existing saved settings are retained.
- Receive peer IRK and identity address after encrypted Secure Connections pairing. Store optional identity fields alongside the existing bond, resolve private advertisements on the host, and keep the original UI bond identifier stable. Existing bond files remain readable. Old bonds need fresh pairing to acquire an IRK; no bonds are automatically deleted.
- Add independent ACL PHY preference: Automatic (1M/2M), 1M, 2M. Audio CIS PHY remains a custom audio setting. Automatic delegates selection to the controller; it does not implement signal-driven CIS switching. Unsupported PHY requests are logged without failing an otherwise usable connection.
- Read ACL PHY for power telemetry instead of assuming it matches CIS PHY. Fix the unreachable power polling slot and diagnostic polling stuck after a missing response.
- Separate audio recovery controls in Czech and English settings. Clarify requested versus negotiated link timing and PHY.
- Redact SMP, ACL continuation payloads, and encryption-key HCI commands from packet logs.

## Validation

Automated checks cover address resolution against the Bluetooth Core D.7 vector, malformed identity messages, old/new bond serialization, supervision timing boundaries, remote parameter replies, HCI command acknowledgement ownership, recovery settings, and log redaction. Rust/C suites and WinUI compilation are run with results in `dev/tests/bluetooth-management-20260920.log` and `dev/tests/bluetooth-management-ui-20260920.log`.

No physical range or hardware recovery result is claimed by these tests. Check with the same adapter, headset, location, and audio settings before comparing PHY choices. Verify sustained playback, CIS-only failure/recovery, real ACL loss/reconnect, cancellation while waiting, live policy edits, and re-pairing/address rotation on a privacy-capable headset. Pairing information remains in the user's profile; do not share the bond file.

Protocol references: [SMP identity distribution and ah D.7](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/host/security-manager-specification.html), [HCI connection parameter and PHY procedures](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/host-controller-interface/host-controller-interface-functional-specification.html).
