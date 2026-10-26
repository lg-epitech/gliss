# Guarded fflate inflater

The browser uses a guarded fork of fflate 0.8.3, under its original MIT license. The production files remain
`web/tile-inflate.js`, `web/vendor/fflate-bounded-core.js` and `web/vendor/fflate-LICENSE`.

This directory retains the exact reconstruction inputs and sixteen malformed-input/provenance contracts:

- `vendor/esm/browser.js`, `vendor/index.ts`, `vendor/package.json` and `vendor/LICENSE` are unchanged upstream files.
- `vendor/provenance.json` is the unchanged original acquisition receipt. Its recorded repository paths refer to an
  earlier repository layout; the copies here keep the same vendor-relative layout.
- `fflate-patches.json` reconstructs `fflate-bounded-core.mjs` from that upstream ESM by exact ordered replacements.
- `inflate.mjs` retains the original wrapper source. The product test reconstructs the production-only wrapper from
  exact source ranges without changing inflater semantics.
- `inflate.test.mjs` retains the complete original safety suite, including malformed trees, unavailable history,
  reserved symbols, advertised-window bounds, checksums, exact extent and fixed output allocation.

Run `npm run test:vendor` for these contracts, or `npm test` for the complete no-VM suite including them. Historical timing
or performance evidence is not a release test pass.
