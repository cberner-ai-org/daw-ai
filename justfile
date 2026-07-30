run port="8888":
  cargo run --locked -- --port {{port}}

pre:
  cargo fmt --check
  cargo clippy --locked --all-targets --all-features -- -D warnings
  node --check web/audio-engine.js
  node --check web/app.js
  node --check qa/browser-support.js
  node --check qa/browser.test.js

test: pre
  cargo build --locked
  cargo test --locked --all-targets --all-features
  node qa/browser.test.js

qa-browser-setup:
  node qa/browser.test.js --check-browser

format:
  cargo fmt
