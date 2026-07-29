run port="8888":
  cargo run -- --port {{port}}

pre:
  cargo fmt --check
  cargo clippy --all-targets --all-features -- -D warnings
  node --check web/audio-engine.js
  node --check web/app.js
  node --check qa/browser-support.js
  node --check qa/browser.test.js

test: pre
  cargo build
  cargo test --all-targets --all-features
  node qa/browser.test.js

qa-browser-setup:
  node qa/browser.test.js --check-browser

msrv-test:
  cargo +1.85.1 test --all-targets --all-features

format:
  cargo fmt
