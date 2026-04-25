# miden-ntx-state-machine

A minimal demonstration of a **self-perpetuating Miden network account**.

## What it does

1. Deploys a network account (`AccountStorageMode::Network`, immutable code, no auth) using an empty tx script.
2. The user submits one initial `update_state` network note tagged for the account.
3. The network transaction builder consumes the note. Inside that consumption the account:
   - Increments a counter in storage slot 0.
   - Emits a fresh `update_state` note with `serial_num + 1`, tagged back to itself.
4. That output note becomes a network note in the next block, the network builder consumes it, and the loop continues forever — driven entirely by the network with no further user transactions.

This is the network-counter tutorial example
(`miden-tutorials/rust-client/src/bin/network_notes_counter_contract.rs`)
extended so the consumed note re-emits itself.

## Layout

```
.
├── Cargo.toml
├── src/main.rs                     # end-to-end demo binary
└── masm/
    ├── accounts/state_machine.masm # increment_state + get_state
    ├── notes/update_state.masm     # increment + re-emit identical note
    └── scripts/deploy.masm         # empty no-op deploy script
```

## Run

```sh
cargo run --release
```

The binary deploys to **testnet**, seeds with one note, then polls the account
until the counter reaches 5, printing every tick. After it exits, the chain
keeps ticking on testnet — re-run a quick state read to confirm.

MASM debugging is enabled (`in_debug_mode(true)`); `debug.stack` calls inside
the MASM print to stdout during transaction proving.
