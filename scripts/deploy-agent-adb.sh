#!/bin/sh
# Install dist/zte-agent into the inactive A/B slot over root ADB and switch to it.
# The previous slot stays untouched, so the boot supervisor can fall back to it.
set -eu
cd "$(dirname "$0")/.."
ADB="${ADB:-adb}"
BIN=dist/zte-agent
[ -f "$BIN" ] || { echo "Build first: scripts/build-agent-docker.sh" >&2; exit 1; }
SHA=$( (sha256sum "$BIN" 2>/dev/null || shasum -a 256 "$BIN") | cut -d' ' -f1)
"$ADB" push "$BIN" /data/local/tmp/zte-agent.new >/dev/null
# The router's adbd only offers a pty shell, so stdin scripts never terminate:
# push the remote script as a file and run it.
# Keep the temp file in the repo (dist/ is ignored): a native Windows adb.exe
# cannot read Git Bash's /tmp.
REMOTE=dist/deploy-agent.remote.sh
trap 'rm -f "$REMOTE"' EXIT
cat > "$REMOTE" <<'EOF'
R=/data/open-u60/update
NEW=/data/local/tmp/zte-agent.new
[ "$(sha256sum $NEW | cut -d' ' -f1)" = "$1" ] || { echo "upload hash mismatch"; exit 1; }
chmod 755 $NEW
CUR=$(cat $R/active); case $CUR in a) T=b;; b) T=a;; *) echo "unexpected active slot: $CUR"; exit 1;; esac
VER=$(sed -n 2p $R/slot-$CUR/bootstrap.sha256)
$NEW ota-probe "$VER" || { echo "ota-probe $VER failed; rebuild with ZTE_AGENT_VERSION=$VER"; exit 1; }
cp $NEW $R/slot-$T/zte-agent.tmp && mv -f $R/slot-$T/zte-agent.tmp $R/slot-$T/zte-agent
printf '%s\n%s\n' "$1" "$VER" > $R/slot-$T/bootstrap.sha256
printf '%s\n' $T > $R/active.tmp && chmod 600 $R/active.tmp && mv -f $R/active.tmp $R/active
kill $(pidof zte-agent) 2>/dev/null || true
sleep 2
sh /data/local/tmp/start_zte_agent.sh
sleep 6
echo "active slot: $(cat $R/active) (previous: $CUR)"
pidof zte-agent >/dev/null && echo "zte-agent running" || { echo "zte-agent NOT running"; exit 1; }
EOF
"$ADB" push "$REMOTE" /tmp/deploy-agent.sh >/dev/null
"$ADB" shell "sh -eu /tmp/deploy-agent.sh $SHA; rc=\$?; rm -f /tmp/deploy-agent.sh; exit \$rc"
