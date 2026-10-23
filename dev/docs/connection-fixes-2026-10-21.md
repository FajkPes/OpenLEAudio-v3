# Connection and Reconnect audit — 2026-10-21

Reported symptoms: most connections succeed, but audio can sound wrong, and the
Reconnect button can leave the headset harder to connect than a normal Connect.
The defects below are verified in the host code; their contribution to the
reported headset failure rate still needs a radio/listening test.

## Changes

- Wait for ACL Disconnection Complete, up to three seconds, rather than assuming
  teardown succeeded after 800 ms. A rejected/expired teardown invalidates the
  controller; the next scan or connection opens it again and runs HCI Reset.
  Unknown Connection Identifier is accepted as proof the old ACL is gone.
- Drain old ACL replies before starting another connection. The controller may
  reuse the same connection handle, so the existing handle filter alone cannot
  distinguish a queued reply from the previous connection.
- An abandoned ATT transaction makes that Link unusable for further requests.
  ATT has no transaction ID, so a late reply cannot safely be attributed to a new
  request. Also check normal response opcodes and Error Response request opcodes.
  This includes the short timeouts used for ASE teardown: recovery must reopen
  Bluetooth if an outstanding transaction has been abandoned.
- Ignore a crossing SMP Security Request while the central is already running
  pairing/encryption. Previously it could be passed to the pairing state machine
  as the expected Pairing Response or identity-distribution message.
- Answer peer ATT requests during SMP too. MTU exchange, notifications and
  indication confirmations now share one handler across pairing, synchronous
  GATT requests and notification collection. The negotiated receive MTU is kept
  consistent when the peer starts the exchange during notification collection.
- Recognize failed LE Connection Complete events without comparing their
  undefined peer-address field. Preserve the actual create-connection opcode in
  diagnostics and use the same matcher when cancel races with completion.
- Read all chunks of long PACS/GATT values. The old loop stopped after two full
  chunks because it compared the total value length to MTU minus one. Handle
  Invalid Offset and first-blob Attribute Not Long from the actual wire Error
  Response; retain other errors. Stop at the 512-byte attribute limit.

No identity/OS spoofing or radio-profile change is required by these fixes.
The existing Android-derived codec selection and configured PHY remain in use.

## Validation

```powershell
$env:TEMP = Join-Path (Get-Location) 'dev/tests/.runtime-tmp'
$env:TMP = $env:TEMP
.\dev\scripts\cargo.cmd test --all-targets --quiet
.\dev\scripts\cargo.cmd build --release --bin OpenLEAudio_Client
python dev/tests/agent-protocol-regression.py --agent dev/core/target/release/OpenLEAudio_Client.exe
```

New regressions replay fragmented SMP interleaved with Security Request, MTU and
an indication; verify response matching, failed connection events with undefined
addresses, and multi-chunk/exact-boundary/512-byte GATT reads. These are host
tests, not an emulation of the USB controller or headset firmware.

Hardware follow-up: alternate normal Connect and the settings Reconnect button
at least 20 times, using a left/right identification recording. Check whether
failure happens before ACL connection, before both ASEs report Streaming, or
after streaming begins. With Debug enabled, look for discarded stale ACL
packets, unconfirmed ACL teardown, ATT timeout, and the confirmed per-ASE CIS and
channel-allocation logs. Do not delete pairing keys to work around a transient
failure; saved-key rejection remains limited to authentication/key-missing
errors. Compare failures with a capture of the same headset reconnecting to an
Android phone before claiming full Android behavioral equivalence.

## References

- [Bluetooth SMP, Vol 3 Part H 2.4.6](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/host/security-manager-specification.html): crossing Security Requests.
- [Bluetooth HCI, Vol 4 Part E 4.1.1, Table 4.1](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-60/out/en/host-controller-interface/host-controller-interface-functional-specification.html): undefined parameters on failed LE connection completion.
- [Bluetooth ATT, Vol 3 Part F](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/host/attribute-protocol--att-.html): MTU, request/response sequencing and long attribute reads.
- [AOSP SMP security-request handling](https://android.googlesource.com/platform/packages/modules/Bluetooth/+/e6130675c04752947ac4779c178ce70eb959a97f/system/stack/smp/smp_act.cc): reuse encryption or initiate pairing according to saved security information.
