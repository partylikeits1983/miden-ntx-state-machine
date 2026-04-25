# miden-ntx-state-machine

A minimal demonstration of a **self-perpetuating Miden network account**.

## What it does

1. Deploys a network account (`AccountStorageMode::Network`, immutable code, `NoAuth`).
2. The user submits one initial `update_state` network note tagged for the account.
3. The network transaction builder consumes the note. Inside that consumption the account:
   - Increments a counter in storage (`miden_ntx_state_machine::counter`).
   - Emits a fresh `update_state` note with `serial_num + 1`, tagged back to itself.
4. That output note becomes a network note in the next block, the network builder consumes it, and the loop continues forever — driven entirely by the network with no further user transactions.

This is the network-counter tutorial example
(`miden-tutorials/rust-client/src/bin/network_notes_counter_contract.rs`)
extended so the consumed note re-emits itself.

## Layout

```
.
├── Cargo.toml
├── rust-toolchain.toml             # pinned to 1.95 (miden-client 0.14 requires >=1.93)
├── src/main.rs                     # end-to-end testnet demo binary
├── tests/mock_chain.rs             # MockChain integration tests (run with `cargo test`)
└── masm/
    ├── accounts/state_machine.masm # increment_count + tick (increment + re-emit)
    ├── notes/update_state.masm     # one-liner: call.state_machine::tick
    └── scripts/deploy.masm         # no-op deploy script (unused; main.rs deploys via increment_count)
```

Pinned to the **0.14** Miden series (matches testnet). MASM is loaded at runtime via
`fs::read_to_string("masm/...")` for the binary, and via `include_str!` for tests.

## Run

### Unit tests (no network)

```sh
cargo test
```

`tests/mock_chain.rs` builds the state machine on `miden_testing::MockChain`, has it consume one seeded `update_state` note, and asserts:
- the counter ticks from 0 → 1 (and 1 → 2 for the two-tick test),
- exactly one output note is emitted per tick,
- the emitted note's recipient digest matches `(serial_num + 1, same script, empty storage)`,
- the emitted note's tag points back at the state machine,
- feeding the emitted note back through a second tx ticks again — the chain closes on itself.

### Testnet demo binary

```sh
cargo run --release
```

Deploys to **testnet**, seeds with one note, then polls the account until the counter reaches 5, printing every tick. After it exits, the chain keeps ticking on testnet — re-run a quick state read to confirm.

MASM debugging is enabled (`in_debug_mode(true)`); `debug.stack` calls inside the MASM print to stdout during transaction proving.

## How `tick` re-emits the next note

The interesting MASM lives in `masm/accounts/state_machine.masm` under `pub proc tick`. It:
1. Calls `increment_count` (storage slot named `miden_ntx_state_machine::counter`).
2. Issues the `INPUT_NOTE_GET_STORAGE_INFO_OFFSET` kernel syscall directly to obtain the active note's storage commitment without consuming it (the public `active_note::get_storage` wrapper drops it).
3. Reuses the active note's script root and serial number (bumped by 1) to build the new recipient via `note::build_recipient_hash`.
4. Computes the self-tag `(account_id_prefix >> 32) & 0xFFFC0000` — the same thing `NoteTag::with_account_target` computes on the Rust side.
5. Calls `output_note::create` with `[tag, NoteType::Public, RECIPIENT]`.

The new note has the same script, same (empty) storage, same tag, and `serial_num + 1`, so its id is distinct from the consumed note's but its shape is identical. The network transaction builder picks it up by tag in the next block and the cycle repeats.
