#!/bin/sh

cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target i686-unknown-linux-musl

cp target/x86_64-unknown-linux-musl/release/screen_test build/screen_test_x86_64
cp target/i686-unknown-linux-musl/release/screen_test build/screen_test_i686