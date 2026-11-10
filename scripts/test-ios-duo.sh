#!/bin/sh
# Offline tests for the Duo features (SMS OTP, call history, RTP/G.711 recorder).
# Runs on macOS (Xcode toolchain) and Linux (swift:6.2 container); no device needed.
set -eu
cd "$(dirname "$0")/.."
test_dir=$(mktemp -d "${TMPDIR:-/tmp}/u60-duo-tests.XXXXXX")
trap 'rm -rf "$test_dir"' EXIT
call=mobile/ios/OpenU60/Features/Call
sms=mobile/ios/OpenU60/Features/SMS

swiftc -module-cache-path "$test_dir/cache" \
    "$sms/SMSModels.swift" mobile/ios/Tests/SMSVerificationCodeTests.swift -o "$test_dir/otp"
"$test_dir/otp"

swiftc -module-cache-path "$test_dir/cache" \
    "$sms/SMSModels.swift" mobile/ios/Tests/SMSArrivalTrackerTests.swift -o "$test_dir/arrival"
"$test_dir/arrival"

# File protection is Apple-only; strip it when building on Linux.
history="$call/CallHistory.swift"
if [ "$(uname)" != Darwin ]; then
    sed 's/, \.completeFileProtection//' "$history" > "$test_dir/CallHistory.swift"
    history="$test_dir/CallHistory.swift"
fi
cp mobile/ios/Tests/CallHistoryTests.swift "$test_dir/main.swift"
swiftc -module-cache-path "$test_dir/cache" "$history" "$test_dir/main.swift" -o "$test_dir/history"
"$test_dir/history"

cp mobile/ios/Tests/CallAudioTests.swift "$test_dir/main.swift"
swiftc -module-cache-path "$test_dir/cache" "$call/CallAudio.swift" "$test_dir/main.swift" -o "$test_dir/audio"
"$test_dir/audio"
cp mobile/ios/Tests/VoiceJitterTests.swift "$test_dir/main.swift"
swiftc -module-cache-path "$test_dir/cache" "$call/CallAudio.swift" "$call/VoiceJitter.swift" \
    "$test_dir/main.swift" -o "$test_dir/jitter"
"$test_dir/jitter"
echo "All Duo tests passed"
