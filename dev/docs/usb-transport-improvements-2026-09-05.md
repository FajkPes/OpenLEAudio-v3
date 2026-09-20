# USB transport and reconnect improvements — 2026-09-05

## Implemented

- WinUSB control/IN/OUT transfer timeout set to 1000 ms. Each bulk writer holds a mutex through completion, so requests do not accumulate in a WinUSB OUT queue. The OS completion is collected before memory or events are reused. This is a transport failure deadline, not an added audio buffer or pacing delay.
- Reuse the completed bulk write OVERLAPPED and its event instead of allocating/creating/closing them for every ACL/ISO packet. Reset only after the previous transfer has completed.
- Check actual byte counts for both bulk and control writes. Partial writes are reported as failures.
- Explicit HCI reader stop flag, abort and join. Read timeouts bound the race between checking the stop flag and submitting a new read. Shutdown no longer deliberately retries a canceled pipe.
- Recognize idle read timeout separately from a USB fault. Idle timeouts do not reset pipes or count as hardware failures.
- Recognize disconnected/invalid device handles and end the reader immediately instead of doing 200 recovery attempts.
- Reopen and initialize the adapter on the next connect attempt when its readers have stopped. Discard old controller-specific connection handles and cached service state during shutdown.
- Stop waiting immediately when the event reader has closed, avoiding a busy loop on a disconnected event channel until the full connection deadline.
- Startup priming retains exactly 20 silence frames, original channel routing and packet ordering. It uses the existing precise timer and observes cancellation/USB errors instead of ignoring them.
- Refresh reconnect policy after active playback ends and before further retries, so changing it during playback is honored.
- Apply battery polling and link-metric settings live. Reuse a single settings-to-live-state implementation.
- Rotate fallback battery reads between battery services rather than polling only the first. Scheduled reads skip notification-enabled services.

## Verification

226 tests passed: 210 library tests, 10 agent tests, 6 LC3 integration tests.
Includes Windows event reuse/reset exercised 1000 times, short-write validation,
USB error classification, battery service selection, live settings propagation,
and all existing channel routing and stereo codec tests.

Release build succeeded with:
`dev/scripts/cargo.cmd build --offline --release --bin OpenLEAudio_Client`

Real unplug/replug, sleep/wake, audible latency and battery consumption have not
been measured. No new backups were created. No change to the WinUSB INF,
headset channel allocation, LC3 codec configuration or dual-CIS topology.

## API references

- [WinUSB pipe policy and timeout semantics](https://learn.microsoft.com/en-us/windows-hardware/drivers/usbcon/winusb-functions-for-pipe-policy-modification)
- [Completion and handle requirements](https://learn.microsoft.com/en-us/windows/win32/api/winusb/nf-winusb-winusb_getoverlappedresult)

A pipe timeout begins when the host controller receives the transfer; it does
not cover time spent in a WinUSB queue. Keeping one operation per pipe and
waiting for its completion is intentional here. This implementation does not
introduce asynchronous batching of audio packets.

## Installation and hardware smoke test

Installed without creating a backup. SHA256: 0C6B0ABC6EF82496222EC919C6F7CEB3B53A027B02441D7C517CD01E56FABF7A.

After the user closed the app, the release client successfully opened and initialized the real adapter, idled for 1.2 seconds, and shut it down, twice. Open took 0.10 s and 0.03 s; shutdown rounded to 0.00 s. The process exited with code 0. This tests adapter lifecycle only, not headset connection speed or audio latency. Four targeted tests also passed in release mode.
