"""Bounded vendor RTP smoke probe; patches a /tmp-only library copy, never firmware."""
import hashlib
import ipaddress
import json
from pathlib import Path
import socket
import subprocess
import time

import u60env

ROOT = u60env.WORK_DIR
ADB = u60env.ADB
REMOTE = '/tmp/u60-rtp-probe'

def adb(*args, timeout=15):
    return subprocess.run([ADB, *args], capture_output=True, timeout=timeout, check=True).stdout

def main(active_number=None):
    from qmi_dial import api, at, voice_or_unknown_calls
    startup = adb('exec-out', 'cat', '/data/local/tmp/start_zte_agent.sh').decode()
    password = next(line.split('=', 1)[1].strip().strip("'\"") for line in startup.splitlines() if 'ZTE_AGENT_PASSWORD=' in line)
    token = api('auth/login', {'password': password})['data']['token']
    state = at(token, 'AT+CLCC')
    complete = 'OK' in {line.strip() for line in state.splitlines()}
    if active_number is None:
        if voice_or_unknown_calls(state) or not complete:
            raise RuntimeError('Idle voice state not confirmed; skip standalone probe')
    else:
        import qmi_dial
        if active_number != qmi_dial.NUMBER or not complete or qmi_dial.listed(state) != 0:
            raise RuntimeError('Authorized active call not confirmed; skip RTP probe')
    route = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    route.connect((u60env.ROUTER_IP, 48894))
    host = route.getsockname()[0]
    route.close()
    if ipaddress.ip_address(host) not in ipaddress.ip_network(u60env.ROUTER_IP + '/24', strict=False):
        raise RuntimeError('Host must be directly connected to the router LAN')
    original = adb('exec-out', 'cat', '/usr/lib/libztertp.so')
    modified = original
    ROOT.mkdir(parents=True, exist_ok=True)
    for old, new in [(b'192.168.253.1', u60env.ROUTER_IP.encode()), (b'192.168.253.100', host.encode())]:
        if modified.count(old + b'\0') != 1 or len(new) > len(old):
            raise RuntimeError('Unexpected vendor library layout')
        modified = modified.replace(old + b'\0', new.ljust(len(old), b'\0') + b'\0')
    path = ROOT / 'rtp-probe-libztertp.so'
    path.write_bytes(modified)
    report = {'host': host, 'original_sha256': hashlib.sha256(original).hexdigest(),
              'test_sha256': hashlib.sha256(modified).hexdigest(), 'packets': []}
    listener = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    listener.bind((host, 48894))
    listener.settimeout(1)
    pid = None
    try:
        adb('shell', 'mkdir -p ' + REMOTE)
        adb('push', str(path), REMOTE + '/libztertp.so')
        output = adb('shell', 'trap "" HUP; LD_LIBRARY_PATH=' + REMOTE + ' /usr/bin/zte_route_hal_voice_test '
                     '-i 11C05000 -d 2,2147483664 -p dummy -r dummy -t 0 -l 5 '
                     '>' + REMOTE + '/output.log 2>&1 </dev/null & echo $!')
        pid = int(output.strip())
        report['pid'] = pid
        report['udp_during_probe'] = adb('shell', 'cat /proc/net/udp').decode(errors='replace')
        until = time.monotonic() + 8
        captured = False
        while time.monotonic() < until:
            if not captured and time.monotonic() > until - 5:
                captured = True
                report['process_maps'] = adb('shell', 'cat /proc/' + str(pid) + '/maps 2>/dev/null || true').decode(errors='replace')
                report['thread_waits'] = adb('shell', 'for t in /proc/' + str(pid) + '/task/*; do echo "$t"; cat "$t/comm" "$t/wchan"; done 2>/dev/null || true').decode(errors='replace')
                report['udp_after_start'] = adb('shell', 'cat /proc/net/udp').decode(errors='replace')
            try:
                packet, peer = listener.recvfrom(2048)
                report['packets'].append({'peer': peer, 'length': len(packet), 'header': packet[:12].hex()})
            except socket.timeout:
                pass
    finally:
        listener.close()
        if pid is not None:
            # Only signal the process if its command still belongs to this probe.
            cmd = adb('shell', 'cat /proc/' + str(pid) + '/cmdline 2>/dev/null || true')
            if b'zte_route_hal_voice_test' in cmd:
                adb('shell', 'kill -9 ' + str(pid) + ' 2>/dev/null || true')
        report['output'] = adb('shell', 'cat ' + REMOTE + '/output.log 2>/dev/null || true').decode(errors='replace')
        (ROOT / 'vendor-rtp-probe-result.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
        print(json.dumps({'host': host, 'packets': len(report['packets']),
                          'test_library_loaded': REMOTE + '/libztertp.so' in report.get('process_maps', ''),
                          'rtp_bound': ':BEFE' in report.get('udp_after_start', ''),
                          'hal_start_errors': report['output'].count('start error'),
                          'report': str(ROOT / 'vendor-rtp-probe-result.json')}, indent=2))

if __name__ == '__main__':
    main()
