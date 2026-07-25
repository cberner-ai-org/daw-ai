run port="8888":
  cargo run -- --port {{port}}

pre:
  cargo fmt --check
  cargo clippy --all-targets --all-features -- -D warnings
  node --check web/app.js
  node --check qa/browser.test.js

test: pre
  #!/usr/bin/env bash
  set -u
  cargo build
  cargo test --all-targets --all-features &
  rust_pid=$!
  node qa/browser.test.js &
  browser_pid=$!
  status=0
  wait "$rust_pid" || status=$?
  wait "$browser_pid" || status=$?
  exit "$status"

qa-browser-setup:
  node qa/browser.test.js --check-browser

msrv-test:
  cargo +1.85.1 test --all-targets --all-features

format:
  cargo fmt
