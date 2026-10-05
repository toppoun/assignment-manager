#!/bin/bash
set -euo pipefail

package_dir="$(cd "$(dirname "$0")" && pwd)"
repo_dir="$(cd "$package_dir/../.." && pwd)"
bridge_dir="$repo_dir/crates/macos-bridge"
rust_target="aarch64-apple-darwin"
export MACOSX_DEPLOYMENT_TARGET=14.0

if [[ "$(uname -s)" != Darwin || "$(uname -m)" != arm64 ]]; then
    echo "Phase 10A smoke requires an Apple Silicon Mac." >&2
    exit 1
fi

# First-time dependency fetch is explicit: cargo fetch --manifest-path .../Cargo.toml
# Subsequent builds use the committed lockfile and local cache only.
cargo build --manifest-path "$bridge_dir/Cargo.toml" --target-dir "$bridge_dir/target" --locked --offline \
    --target "$rust_target" --lib
cargo build --manifest-path "$bridge_dir/Cargo.toml" --target-dir "$bridge_dir/target" --locked --offline \
    --features bindgen --bin uniffi-bindgen --example prepare-smoke

native_dir="$package_dir/Native"
swift_dir="$package_dir/Sources/AssignmentManagerBridge"
ffi_dir="$package_dir/Sources/AssignmentManagerBridgeFFI"
mkdir -p "$native_dir/Bindings" "$swift_dir" "$ffi_dir"
"$bridge_dir/target/debug/uniffi-bindgen" generate \
    "$bridge_dir/target/$rust_target/debug/libassignment_manager_macos_bridge.dylib" \
    --config "$bridge_dir/bindgen.toml" --language swift --out-dir "$native_dir/Bindings"
cp "$native_dir/Bindings/AssignmentManagerBridge.swift" "$swift_dir/AssignmentManagerBridge.swift"
cp "$native_dir/Bindings/AssignmentManagerBridgeFFI.h" "$ffi_dir/AssignmentManagerBridgeFFI.h"
cp "$native_dir/Bindings/AssignmentManagerBridgeFFI.modulemap" "$ffi_dir/module.modulemap"
cp "$bridge_dir/target/$rust_target/debug/libassignment_manager_macos_bridge.a" "$native_dir/"
cp "$bridge_dir/target/debug/examples/prepare-smoke" "$native_dir/prepare-smoke"

if [[ "${1:-}" != --prepare-only ]]; then
    # Some Command Line Tools installations bundle Testing but SwiftPM does not
    # discover its framework/plugin search paths. Full Xcode uses its normal paths.
    testing_flags=(--disable-xctest --enable-swift-testing)
    developer_dir="$(xcode-select --print-path)"
    testing_frameworks="$developer_dir/Library/Developer/Frameworks"
    if [[ -d "$testing_frameworks/Testing.framework" ]]; then
        testing_flags+=(-Xswiftc -F -Xswiftc "$testing_frameworks")
        testing_flags+=(-Xswiftc -plugin-path -Xswiftc "$developer_dir/usr/lib/swift/host/plugins/testing")
        testing_flags+=(-Xlinker -rpath -Xlinker "$testing_frameworks")
    fi
    swift build --package-path "$package_dir" --scratch-path "$package_dir/.build" \
        -Xswiftc -strict-concurrency=complete -Xswiftc -warnings-as-errors
    swift test --package-path "$package_dir" --scratch-path "$package_dir/.build" \
        "${testing_flags[@]}" \
        -Xswiftc -strict-concurrency=complete -Xswiftc -warnings-as-errors
    swift run --package-path "$package_dir" --skip-build BridgeSmoke
fi
