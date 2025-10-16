#!/bin/sh

OUTPUT_DIR="target/llvm-cov"

mkdir -p "$OUTPUT_DIR"

cargo llvm-cov --workspace --all-features --include-build-script --lcov --output-path "$OUTPUT_DIR/lcov.info"
genhtml --prefix $(pwd) -o "$OUTPUT_DIR/html" "$OUTPUT_DIR/lcov.info"
