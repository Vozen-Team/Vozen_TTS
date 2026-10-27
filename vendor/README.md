# Temporary Songbird dependency override

Songbird 0.6.0 is copied from its published crates.io package. Its implementation
is unchanged; only the ringbuf requirement in the normalized Cargo.toml changes
from 0.4 to 0.5.3. This selects the upstream repair for RUSTSEC-2026-0293 without
writing a local unsafe buffer implementation. Original file hashes and the
upstream revision are retained for review. The upstream ISC license is included.

Remove this override once a compatible Songbird release accepts patched ringbuf.
Do not edit vendored implementation files without a separate reviewed change.
The remaining crypto/MLS advisories are not resolved by this override. Passing
compilation and synthetic tests does not replace a production voice canary.
