"""Exercise the real agent process without opening or changing a Bluetooth adapter."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--agent", type=Path,
                        default=Path(__file__).resolve().parents[1] / "core/target/debug/OpenLEAudio_Client.exe")
    args = parser.parse_args()
    # Isolate settings and bonds: a protocol test must not migrate a user's data.
    with tempfile.TemporaryDirectory(prefix="olea-agent-") as directory:
        environment = dict(os.environ, APPDATA=directory)
        scenarios = [
            ("quit", '{"cmd":"quit"}\n'),
            ("EOF", ""),
            ("malformed input followed by quit",
             '\n1\nfalse\n"text"\n[]\nnull\n{broken\n{"cmd":"settings"}\n{"cmd":"quit"}\n'),
            ("malformed input followed by EOF", '1\n[]\n{"cmd":"unknown"}\n'),
        ]
        for label, payload in scenarios:
            result = subprocess.run([str(args.agent.resolve())], input=payload, text=True,
                                    capture_output=True, timeout=30, env=environment)
            assert result.returncode == 0, (label, result.returncode, result.stderr)
            assert "panicked" not in result.stderr, (label, result.stderr)
            events = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
            assert any(event.get("event") == "ready" for event in events), label
            if "malformed" in label:
                assert sum(event.get("text") == "command must be a JSON object" for event in events) >= 2, (label, events)
            print(f"PASS: {label}")


if __name__ == "__main__":
    main()
