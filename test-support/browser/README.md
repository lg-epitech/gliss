# Portable browser-test support

These helpers require no prepared guest, KVM state, research directory or retained executable:

- `probe-v2.js` is the independent historical marker decoder used by `tests/probe.test.mjs`.
- `tcp-proxy.mjs` supplies a deterministic ordered TCP delay/rate model for the metrics-reset regression. It does not
  perform real packet loss or change host networking.

`tests/helpers.mjs` selects an absolute `GLISS_BINARY` when supplied, otherwise the checkout's freshly built
`target/release/gliss-server`. Install the pinned Playwright Chromium, or explicitly set `CHROMIUM_PATH`.

`npm test` runs no prepared-VM suite.

The codec fixtures come from `test-support/rust/codec/fixtures.json`.
