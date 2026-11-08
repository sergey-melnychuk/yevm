#!/usr/bin/env bash
set -e

PROJECT_ROOT="$(pwd)"
CARGO_TOML="$PROJECT_ROOT/Cargo.toml"
CARGO_LOCK="$PROJECT_ROOT/Cargo.lock"
VENDOR_BASE="$HOME/.local/share/rocksdb-patched-vendors"

if [ ! -f "$CARGO_TOML" ]; then
    echo "ERROR: Run this script from the root of your Rust workspace/project."
    exit 1
fi

if [ ! -f "$CARGO_LOCK" ]; then
    echo "ERROR: Cargo.lock not found. Run 'cargo check' or 'cargo build' once first."
    exit 1
fi

echo "==> Detecting librocksdb-sys version from Cargo.lock..."

ROCKSDB_VERSION=$(grep -A 2 'name = "librocksdb-sys"' "$CARGO_LOCK" | grep 'version =' | head -n 1 | cut -d '"' -f 2)

if [ -z "$ROCKSDB_VERSION" ]; then
    echo "ERROR: librocksdb-sys not found in Cargo.lock."
    exit 1
fi

echo "==> Target librocksdb-sys version: $ROCKSDB_VERSION"

REGISTRY_MATCH=$(find "$HOME/.cargo/registry/src" -type d -name "librocksdb-sys-$ROCKSDB_VERSION" 2>/dev/null | head -n 1)

if [ -z "$REGISTRY_MATCH" ] || [ ! -d "$REGISTRY_MATCH" ]; then
    echo "ERROR: Could not find registry folder for librocksdb-sys-$ROCKSDB_VERSION in ~/.cargo/registry/src."
    exit 1
fi

TARGET_OUT=$(ls -td ./target/release/build/librocksdb-sys-*/out/librocksdb.a 2>/dev/null | head -n 1 | xargs -r dirname)

if [ -z "$TARGET_OUT" ] || [ ! -f "$TARGET_OUT/librocksdb.a" ] || [ ! -f "$TARGET_OUT/bindings.rs" ]; then
    echo "ERROR: No compiled librocksdb-sys artifacts found in ./target/release/build."
    echo "Please run 'cargo build --release' once to build RocksDB C++ artifacts."
    exit 1
fi

CRATE_NAME="librocksdb-sys-$ROCKSDB_VERSION"
VENDOR_DIR="$VENDOR_BASE/$CRATE_NAME"
PREBUILT_OUT="$VENDOR_DIR/prebuilt_out"

echo "==> Found build artifacts at: $TARGET_OUT"
echo "==> Creating local vendor patch at $VENDOR_DIR..."

rm -rf "$VENDOR_DIR"
mkdir -p "$VENDOR_DIR"
cp -r "$REGISTRY_MATCH"/* "$VENDOR_DIR/"
mkdir -p "$PREBUILT_OUT"

# Copy static archives and bindings in a macOS and Linux safe way
find "$TARGET_OUT" -maxdepth 1 \( -name "lib*.a" -o -name "bindings.rs" \) -exec cp {} "$PREBUILT_OUT/" \;

echo "    -> Saved static archives and bindings.rs to $PREBUILT_OUT"

cat << 'EOF' > "$VENDOR_DIR/build.rs"
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let prebuilt_dir = PathBuf::from(&manifest_dir).join("prebuilt_out");
    let out_dir = env::var("OUT_DIR").unwrap();

    let src_bindings = prebuilt_dir.join("bindings.rs");
    let dst_bindings = Path::new(&out_dir).join("bindings.rs");
    fs::copy(&src_bindings, &dst_bindings)
        .expect("Failed to copy prebuilt bindings.rs");

    println!("cargo:rustc-link-search=native={}", prebuilt_dir.display());

    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-lib=c++");
    #[cfg(not(target_os = "macos"))]
    println!("cargo:rustc-link-lib=stdc++");

    println!("cargo:rustc-link-lib=static=rocksdb");

    if let Ok(entries) = fs::read_dir(&prebuilt_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("a") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if let Some(lib_name) = stem.strip_prefix("lib") {
                        if lib_name != "rocksdb" {
                            println!("cargo:rustc-link-lib=static={}", lib_name);
                        }
                    }
                }
            }
        }
    }
}
EOF

PATCH_LINE="librocksdb-sys = { path = \"$VENDOR_DIR\" }"

if ! grep -q "\[patch.crates-io\]" "$CARGO_TOML"; then
    echo "" >> "$CARGO_TOML"
    echo "[patch.crates-io]" >> "$CARGO_TOML"
fi

if ! grep -q "$VENDOR_DIR" "$CARGO_TOML"; then
    echo "$PATCH_LINE" >> "$CARGO_TOML"
    echo "==> Patch added to Cargo.toml: $PATCH_LINE"
fi

echo ""
echo "==> SUCCESS! Testing patch link with 'cargo check'..."
cargo check

