# koe

A privacy-first recording & transcription app.

## Features

- Offline-first
- On-device transcription
- System audio + microphone capture
- Simple local storage
- Featherlight UI
- Cross platform

## Verification

The meeting lifecycle is model checked: Stateright explores every reachable
state and every ordering of the modeled actions, and each one must satisfy the
session invariants. The transition invariants restate the expected outcome
independently of `Session::apply`, so they are the oracle, not the machine. Run
`cargo test -p koe-session-core` to check it, and see the
[`koe-session-core`](crates/koe-session-core/src/lib.rs) docs for the states,
transitions, and invariants.
