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

## Experimental MLS dependency upgrade

Davey 0.1.4 is also copied from its published package, with the MIT license and
original file hashes. The documented session API adaptation replaces the old
`StageCommitError::OwnCommit` branch with `ProcessedMessageContent::OwnPendingCommit`.
OpenMLS now authenticates the echoed commit against the pending confirmation tag
before returning that result; mismatches remain errors. A separate test module
checks own-commit merge, Welcome, authenticated audio and external-sender rejection;
trace statements that printed media key seeds and derived secrets are removed from
the session, ratchet and KDF helpers. Cryptographic calculations remain unchanged.
The three OpenMLS requirements move to OpenMLS 0.9.2 / credentials 0.6.0 /
RustCrypto provider 0.6.0. Actual Discord interoperability remains unproven.
Do not deploy this experiment on the strength of a lower advisory count alone.
The crypto-bigint advisory remains; optional lockfile dependencies can retain
other matches. Keep this experiment separate from the reviewed ringbuf candidate.

The `davey/tests/legacy-peer` fixture is a separate workspace using the unmodified
published Davey 0.1.4 and OpenMLS 0.8.1. Its locked legacy dependencies are for
interoperability testing only, not the production dependency graph. CI builds it
before the Davey tests and sets `VOZEN_LEGACY_DAVE_PEER` to its executable. The
test exchanges Welcome, epoch authenticator and encrypted synthetic frames between
the two implementations over bounded local pipes; it uses no Discord credentials.
The test reaps its child on success or failure, with a five-minute CI step limit.
