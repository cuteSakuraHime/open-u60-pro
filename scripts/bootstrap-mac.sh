#!/bin/bash
# One-shot development environment for macOS (Apple silicon or Intel).
# Safe to re-run: every step checks before installing.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)

ok()   { printf '\033[32m[+]\033[0m %s\n' "$1"; }
warn() { printf '\033[33m[!]\033[0m %s\n' "$1"; }
fail() { printf '\033[31m[-]\033[0m %s\n' "$1" >&2; exit 1; }

[ "$(uname)" = Darwin ] || fail "macOS only"

# 1. Xcode (App Store / developer.apple.com; cannot be scripted without an Apple ID)
XCODE_WANTED=$(cat .xcode-version)
if ! xcodebuild -version >/dev/null 2>&1; then
    fail "Install Xcode $XCODE_WANTED, open it once, then run: sudo xcode-select -s /Applications/Xcode.app"
fi
XCODE_HAVE=$(xcodebuild -version | awk 'NR==1{print $2}')
[ "$XCODE_HAVE" = "$XCODE_WANTED" ] || warn "Xcode $XCODE_HAVE found; project is pinned to $XCODE_WANTED"
xcodebuild -checkFirstLaunchStatus || sudo xcodebuild -runFirstLaunch
xcrun simctl list runtimes | grep -q iOS || warn "No iOS Simulator runtime: Xcode > Settings > Components"
ok "Xcode $XCODE_HAVE"

# 2. Homebrew packages (pinned in Brewfile)
command -v brew >/dev/null || fail "Install Homebrew first: https://brew.sh"
brew bundle --file="$ROOT/Brewfile"
ok "Homebrew packages"

XCODEGEN_WANTED=2.46.0
XCODEGEN_HAVE=$(xcodegen --version | awk '{print $2}')
[ "$XCODEGEN_HAVE" = "$XCODEGEN_WANTED" ] || warn "XcodeGen $XCODEGEN_HAVE; project was generated with $XCODEGEN_WANTED"

# 3. Rust toolchain from rust-toolchain.toml (version + musl target)
RUSTUP_PREFIX=$(brew --prefix rustup)
export PATH="$RUSTUP_PREFIX/bin:$HOME/.cargo/bin:$PATH"
rustup show active-toolchain >/dev/null 2>&1 || rustup-init -y --no-modify-path --default-toolchain none
rustup show >/dev/null   # installs the pinned toolchain and targets
ok "Rust $(rustc --version)"

# 4. Local, git-ignored configuration
if [ ! -f mobile/ios/OpenU60/.env ]; then
    cp mobile/ios/OpenU60/.env.example mobile/ios/OpenU60/.env
    warn "Edit mobile/ios/OpenU60/.env: DEVELOPMENT_TEAM and BUNDLE_ID_PREFIX"
fi
[ -f tools/u60.env ] || { cp tools/u60.env.example tools/u60.env; warn "Edit tools/u60.env for router scripts"; }

# 5. Generate the Xcode project (never committed)
set -a; . mobile/ios/OpenU60/.env; set +a
(cd mobile/ios/OpenU60 && xcodegen generate --quiet)
ok "Generated mobile/ios/OpenU60/OpenU60.xcodeproj"

# 6. Verify: offline tests, a Simulator build and agent unit tests
sh scripts/test-ios-duo.sh
sh scripts/test-sms-forward.sh
python3 -m unittest discover -s tools -p 'test_*.py'
SIM=$(xcrun simctl list devices available -j | python3 -c '
import json,sys
d=json.load(sys.stdin)["devices"]
print(next((x["udid"] for k,v in d.items() if "iOS" in k for x in v if x["name"].startswith("iPhone")),""))')
if [ -n "$SIM" ]; then
    xcodebuild -project mobile/ios/OpenU60/OpenU60.xcodeproj -scheme OpenU60 \
        -destination "platform=iOS Simulator,id=$SIM" -derivedDataPath mobile/ios/OpenU60/build \
        -quiet CODE_SIGNING_ALLOWED=NO build
    ok "Simulator build"
else
    warn "Skipped Simulator build: no iPhone simulator installed"
fi
cargo test -p zte-agent --quiet telephony
ok "Environment ready. Open mobile/ios/OpenU60/OpenU60.xcodeproj or run mobile/ios/scripts/build_and_run.sh --simulator"
