#!/bin/bash
set -euo pipefail
project_dir="$(cd "$(dirname "$0")" && pwd)"
export MACOSX_DEPLOYMENT_TARGET=14.0
developer_dir="$(xcode-select -p)"
if [[ ! -d "$developer_dir/Platforms/MacOSX.platform" ]]; then
    echo "Full Xcode with the macOS platform is required." >&2
    exit 1
fi
xcodebuild -version
bash "$project_dir/../BridgeSmoke/build.sh" --prepare-only
cargo build --manifest-path "$project_dir/../../crates/macos-bridge/Cargo.toml" \
    --locked --offline --example prepare-native-smoke
cp "$project_dir/../../crates/macos-bridge/target/debug/examples/prepare-native-smoke" \
    "$project_dir/../BridgeSmoke/Native/prepare-native-smoke"
swift test --package-path "$project_dir" --scratch-path "$project_dir/.build" \
    --disable-xctest --enable-swift-testing \
    -Xswiftc -strict-concurrency=complete -Xswiftc -warnings-as-errors
xcodebuild -project "$project_dir/AssignmentManager.xcodeproj" -scheme AssignmentManager \
    -configuration Debug -destination 'platform=macOS,arch=arm64' \
    -derivedDataPath "$project_dir/build" build
