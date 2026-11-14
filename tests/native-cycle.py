"""Exercise the production binary under launchd, with no repository mutations."""
import datetime
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import time
import uuid


def main():
    if sys.platform != "darwin" or os.getuid() == 0:
        raise SystemExit("Run from a native macOS terminal or SSH session as the logged-in user, without sudo.")
    binary = sys.argv[1]
    config = json.loads(Path("/etc/jj-sync.json").read_text())
    if not config["peers"]:
        raise SystemExit("The installed configuration has no peers to check.")
    # Reuse only the installed SSH client configuration, whose IdentityFile and
    # known-hosts policy stay intact. The helper's state and pairing indicators
    # are temporary. No public key is enrolled on either machine.
    config["repositories"] = []
    config.pop("postFetchCommand", None)
    label = "se.kjekac.jj-sync-native-check-" + uuid.uuid4().hex
    domain = f"gui/{os.getuid()}"
    with tempfile.TemporaryDirectory(prefix="jj-native-cycle-") as directory:
        root = Path(directory)
        state = root / "state"
        (state / "peers").mkdir(parents=True, mode=0o700)
        config["state"] = str(state)
        for peer in config["peers"]:
            (state / "peers" / f"{peer}.pub").touch()
        settings = root / "config.json"
        settings.write_text(json.dumps(config))
        stdout, stderr = root / "stdout", root / "stderr"
        plist = root / "agent.plist"
        plist.write_bytes(plistlib.dumps({
            "Label": label, "ProgramArguments": [binary, "--config", str(settings), "cycle"],
            "EnvironmentVariables": {"HOME": config["home"]},
            "RunAtLoad": True, "StartInterval": 60,
            "StandardOutPath": str(stdout), "StandardErrorPath": str(stderr),
        }))
        print("Testing the production Rust binary with an empty repository allowlist.", flush=True)
        print("Approve JJ Sync in the machine's GUI if prompted. Waiting for three successful scheduled passes.", flush=True)
        subprocess.run(["/usr/bin/codesign", "--verify", "--strict", binary], check=True)
        subprocess.run(["/bin/launchctl", "bootstrap", domain, str(plist)], check=True)
        try:
            previous = None
            successes = 0
            deadline = time.monotonic() + 260
            while time.monotonic() < deadline:
                path = state / "status.json"
                if path.exists():
                    status = json.loads(path.read_text())
                    if status != previous:
                        print(datetime.datetime.now(datetime.timezone.utc).isoformat(), json.dumps(status), flush=True)
                        if all(status.get(peer, {}).get("lastSuccess") and not status[peer].get("error") for peer in config["peers"]):
                            successes += 1
                            if successes >= 3:
                                print("PASS: three successful scheduled passes; no manual cycle invocation.", flush=True)
                                return
                        else:
                            successes = 0
                        previous = status
                time.sleep(0.5)
            raise SystemExit("FAIL: three successful scheduled passes were not observed within 260 seconds.")
        finally:
            subprocess.run(["/bin/launchctl", "bootout", f"{domain}/{label}"], capture_output=True)
            if stderr.exists() and stderr.stat().st_size:
                print("Captured stderr (including any initial permission denial):\n" + stderr.read_text(), flush=True)


if __name__ == "__main__":
    main()
