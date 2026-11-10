#!/bin/sh
# Temporarily collect audio errors, restoring the observed syslog command on exit.
restore() {
    killall syslogd 2>/dev/null
    syslogd -O /logfs/syslog -l 1 -s 8192 -f /tmp/syslog.conf -S 0 -t
}
trap restore EXIT
trap 'exit 1' HUP INT TERM
killall syslogd || exit 1
syslogd -O /tmp/u60-audio-syslog -l 8 -s 2048 -b 1 -S -t || exit 1
hal_voice_test -i "${U60_PROBE_VSID:-11C05000}" -d 2,2147483664 -l "${U60_PROBE_SECONDS:-1}" -r /tmp/u60-audio-probe.wav -t 2 >/tmp/u60-audio-probe.out 2>&1
result=$?
logger -t u60-audio-probe "finished result=$result"
exit "$result"
