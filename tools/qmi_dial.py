#!/usr/bin/env python3
"""Dial one authorized number; verify CLCC and always attempt hangup."""
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import sys
import time
import urllib.request

import u60env

ROOT = Path(__file__).resolve().parent
ADB = u60env.ADB
NUMBER = u60env.TEST_NUMBER


def api(path, data, token=None):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(
        u60env.AGENT_URL + "/api/" + path,
        data=json.dumps(data).encode(), headers=headers,
    )
    with urllib.request.urlopen(request, timeout=15) as response:
        return json.loads(response.read())


def at(token, command):
    result = api("at/send", {"command": command, "timeout": 6}, token)
    text = (result.get("data") or {}).get("response") or ""
    print(command, text.strip(), flush=True)
    return text


def voice_ports(lookup_output):
    """Modem-side (node 3) QRTR ports for QMI service 9 (voice)."""
    return [int(f[4]) for f in (line.split() for line in lookup_output.splitlines())
            if len(f) >= 5 and f[0] == "9" and f[3] == "3" and f[4].isdigit()]


_voice_port = None


def voice_port():
    """QRTR ports are reassigned on every boot (seen 64 and 65), so look them up."""
    global _voice_port
    if _voice_port is None:
        out = subprocess.run(
            [ADB, "shell", "qrtr-lookup >/tmp/qrtr.txt 2>/dev/null & sleep 2; kill $! 2>/dev/null; cat /tmp/qrtr.txt"],
            capture_output=True, timeout=15, check=True,
        ).stdout.decode("utf-8", "replace")
        ports = voice_ports(out)
        if len(ports) != 1:
            raise RuntimeError(f"Expected one modem voice service, found {len(ports)}")
        _voice_port = ports[0]
        print(f"QMI voice service on QRTR port {_voice_port}", flush=True)
    return _voice_port


def qmi(message, body):
    result = subprocess.run(
        [sys.executable, str(ROOT / "nas_get.py"), str(voice_port()), message, body.hex()],
        capture_output=True, text=True, timeout=40, check=False,
    )
    print(result.stdout, flush=True)
    if result.returncode:
        print(result.stderr, flush=True)
        raise RuntimeError(f"QMI helper failed with exit code {result.returncode}")
    sent = re.search(r"^send (-?\d+)", result.stdout, re.M)
    if not sent or int(sent.group(1)) <= 0:
        raise RuntimeError("QMI request was not sent")
    return result.stdout


def dial_response(output):
    """Validate the QRTR QMI header, transaction and bounded TLVs."""
    for line in output.splitlines():
        if not line.startswith("response "):
            continue
        try:
            packet = bytes.fromhex(line.split(" ", 1)[1])
        except ValueError:
            continue
        if len(packet) < 7:
            continue
        flags, transaction, message, length = struct.unpack_from("<BHHH", packet)
        if (flags, transaction, message) != (2, 5, 0x20) or len(packet) != 7 + length:
            continue
        fields = {}
        offset = 7
        while offset < len(packet):
            if offset + 3 > len(packet):
                return False, None
            kind, size = struct.unpack_from("<BH", packet, offset)
            offset += 3
            if offset + size > len(packet) or kind in fields:
                return False, None
            fields[kind] = packet[offset:offset + size]
            offset += size
        accepted = fields.get(2) == b"\x00\x00\x00\x00"
        call = fields.get(0x10, b"")
        return accepted, call[0] if accepted and len(call) == 1 else None
    return False, None


def voice_or_unknown_calls(text):
    """Do not mistake CLCC data/fax bearers for a voice call."""
    for line in text.splitlines():
        if "+CLCC:" not in line:
            continue
        match = re.search(r"\+CLCC:\s*\d+,\s*[01],\s*\d+,\s*(\d+),", line)
        if not match or int(match.group(1)) not in (1, 2):
            return True
    return False


def listed(text):
    for line in text.splitlines():
        match = re.search(r'\+CLCC:\s*\d+,\s*0,\s*([0-5]),\s*0,\s*[01],\s*"([^"\r\n]+)"', line)
        if match and match.group(2) == NUMBER:
            return int(match.group(1))
    return None


def hangup_and_verify(token, call_id):
    """Try both transports independently; errors must not skip state verification."""
    try:
        at(token, "AT+CHUP")
    except Exception as error:
        print(f"AT hangup unavailable ({type(error).__name__}); trying fallback", flush=True)
    if call_id is not None:
        try:
            qmi("21", bytes([1, 1, 0, call_id]))
        except Exception as error:
            print(f"QMI hangup unavailable ({type(error).__name__}); verifying state", flush=True)
    for attempt in range(3):
        try:
            after = at(token, "AT+CLCC")
            complete = "OK" in {line.strip() for line in after.splitlines()}
            if complete and not voice_or_unknown_calls(after):
                return
        except Exception as error:
            print(f"Call state unavailable ({type(error).__name__})", flush=True)
        if attempt < 2:
            time.sleep(1)
    raise RuntimeError("Hangup could not be verified; inspect call state immediately")


def main():
    if not re.fullmatch(r"\+?[0-9]{3,15}", NUMBER):
        raise SystemExit("Set U60_TEST_NUMBER to the actual authorized number; placeholders are rejected.")
    raw = subprocess.run(
        [ADB, "exec-out", "cat", "/data/local/tmp/start_zte_agent.sh"],
        capture_output=True, check=True, timeout=10,
    ).stdout.decode("utf-8", "replace")
    password = next(
        line.split("=", 1)[1].strip().strip("'\"")
        for line in raw.splitlines() if "ZTE_AGENT_PASSWORD=" in line
    )
    token = api("auth/login", {"password": password})["data"]["token"]
    for attempt in range(3):
        before = at(token, "AT+CLCC")
        if voice_or_unknown_calls(before):
            raise SystemExit("Existing voice or unknown call; no dial sent.")
        if "OK" in {line.strip() for line in before.splitlines()}:
            break
        time.sleep(1)
    else:
        raise SystemExit("Cannot establish an idle call state; no dial sent.")
    number = NUMBER.encode("ascii")
    # The top-level QMI string has neither an internal length byte nor a NUL.
    body = struct.pack("<BH", 1, len(number)) + number
    call_type = u60env.get("U60_CALL_TYPE", "auto")
    if call_type == "ip":
        body += bytes.fromhex("10010002")
    elif call_type != "auto":
        raise SystemExit("U60_CALL_TYPE must be auto or ip")
    print(f"Call type: {call_type}", flush=True)
    call_id = None
    confirmed = False
    audio_probe = os.environ.get("U60_AUDIO_PROBE") == "1"
    rtp_probe = os.environ.get("U60_RTP_PROBE") == "1"
    audio_attempted = False

    def probe_if_active(state):
        nonlocal audio_attempted
        if not (audio_probe or rtp_probe) or audio_attempted or state != 0:
            return
        audio_attempted = True
        if rtp_probe:
            from probe_vendor_rtp import main as probe_rtp
            print("Active target call confirmed; starting bounded RTP probe", flush=True)
            probe_rtp(active_number=NUMBER)
            return
        print("Active target call confirmed; recording 5-second audio probe", flush=True)
        script = (ROOT / "probe_audio_logs.sh").read_text().replace("\r\n", "\n")
        script = "export U60_PROBE_SECONDS=5\n" + script
        subprocess.run(
            [ADB, "shell", "-T", "sh"], input=script.encode(),
            timeout=20, check=True,
        )
        print("Audio probe process finished; sample validation is still required", flush=True)

    try:
        output = qmi("20", body)
        accepted, call_id = dial_response(output)
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            state = listed(at(token, "AT+CLCC"))
            if state is not None:
                confirmed = True
                print(f"Target listed, state={state}; monitoring for 40 seconds", flush=True)
                hold_until = time.monotonic() + 40
                probe_if_active(state)
                while time.monotonic() < hold_until:
                    time.sleep(min(2, max(0, hold_until - time.monotonic())))
                    current = at(token, "AT+CLCC")
                    complete = "OK" in {line.strip() for line in current.splitlines()}
                    if not complete:
                        print("Incomplete CLCC response; call state unknown, continuing bounded observation", flush=True)
                        continue
                    current_state = listed(current)
                    if current_state is None:
                        print("Complete CLCC response confirms call ended before 40 seconds", flush=True)
                        break
                    probe_if_active(current_state)
                else:
                    print("40-second observation window elapsed; hanging up (missing responses do not prove continuity)", flush=True)
                break
            if not accepted:
                break
            time.sleep(1)
    finally:
        hangup_and_verify(token, call_id)
    print("Target call presence verified; now idle" if confirmed else "No target call verified", flush=True)
    if audio_probe:
        if audio_attempted:
            for name in ("u60-audio-probe.wav", "u60-audio-probe.out", "u60-audio-syslog"):
                u60env.WORK_DIR.mkdir(parents=True, exist_ok=True)
                subprocess.run([ADB, "pull", "/tmp/" + name, str(u60env.WORK_DIR / ("active-" + name))], check=True, timeout=20)
        else:
            print("No active target call observed; audio probe was not started", flush=True)


if __name__ == "__main__":
    main()
