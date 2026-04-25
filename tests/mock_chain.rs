//! End-to-end mock-chain tests for the network state machine.
//!
//! Exercises the same MASM the testnet binary uses, but on `miden_testing`'s
//! `MockChain`. No network round-trips, no proving, runs as part of `cargo
//! test`.
//!
//! What we assert (the load-bearing properties of this repo):
//!   1. `tick` increments the counter by 1.
//!   2. `tick` emits exactly one output note.
//!   3. That output note has the same script root, the same (empty) storage
//!      commitment, and `serial_num + 1` relative to the consumed note - so
//!      its recipient digest matches what we'd build by hand. This is the
//!      "self-perpetuating" invariant: the network executor sees an output
//!      note structurally identical (modulo serial number) to the one just
//!      consumed.
//!   4. The output note's tag points back at the state machine, so the
//!      network transaction builder will pick it up in the next block.
//!   5. Feeding that emitted note back through a second tx ticks the
//!      counter again - the loop closes.

use std::sync::Arc;

use anyhow::Result;
use miden_protocol::account::component::AccountComponentMetadata;
use miden_protocol::account::{
    AccountBuilder, AccountComponent, AccountStorageMode, AccountType, StorageSlot, StorageSlotName,
};
use miden_protocol::assembly::{
    Assembler, DefaultSourceManager, Library, Module, ModuleKind, SourceManagerSync,
};
use miden_protocol::note::{
    Note, NoteAssets, NoteMetadata, NoteRecipient, NoteScript, NoteStorage, NoteTag, NoteType,
};
use miden_protocol::transaction::{RawOutputNote, TransactionKernel};
use miden_protocol::Felt;
use miden_protocol::Word;
use miden_standards::account::auth::NoAuth;
use miden_standards::note::{NetworkAccountTarget, NoteExecutionHint as StdNoteExecutionHint};
use miden_testing::{Auth, MockChain};

const STATE_MACHINE_LIB_PATH: &str = "external_contract::state_machine";
const COMPONENT_NAME: &str = "miden_ntx_state_machine::state_machine";
const COUNTER_SLOT_NAME: &str = "miden_ntx_state_machine::counter";

const ACCOUNT_MASM: &str = include_str!("../masm/accounts/state_machine.masm");
const NOTE_MASM: &str = include_str!("../masm/notes/update_state.masm");

// ---------------------------------------------------------------------------
// HELPERS
// ---------------------------------------------------------------------------

/// Assembles the state machine MASM as a Library at the path the note script
/// expects (`external_contract::state_machine`). The returned library is used
/// both to build the AccountComponent and to dynamically link the note script
/// against it.
fn assemble_state_machine_library() -> Result<Arc<Library>> {
    let source_manager: Arc<dyn SourceManagerSync> = Arc::new(DefaultSourceManager::default());
    let assembler: Assembler =
        TransactionKernel::assembler_with_source_manager(source_manager.clone())
            .with_dynamic_library(miden_standards::StandardsLib::default())
            .map_err(|e| anyhow::anyhow!("link standards lib: {e}"))?;
    let module = Module::parser(ModuleKind::Library)
        .parse_str(STATE_MACHINE_LIB_PATH, ACCOUNT_MASM, source_manager)
        .map_err(|e| anyhow::anyhow!("parse state_machine.masm:\n{e:?}"))?;
    let library = assembler
        .assemble_library([module])
        .map_err(|e| anyhow::anyhow!("assemble state_machine library:\n{e:?}"))?;
    Ok(library)
}

/// Compiles the update_state note script with the state machine library
/// dynamically linked.
fn compile_note_script(library: &Library) -> Result<NoteScript> {
    use miden_standards::code_builder::CodeBuilder;
    let script = CodeBuilder::new()
        .with_dynamically_linked_library(library)
        .map_err(|e| anyhow::anyhow!("link state_machine library: {e}"))?
        .compile_note_script(NOTE_MASM)
        .map_err(|e| anyhow::anyhow!("compile update_state.masm: {e}"))?;
    Ok(script)
}

/// Builds a state machine account with the given seed, ready to be added to a
/// MockChain via `add_account`.
fn build_state_machine(
    library: Arc<Library>,
    init_seed: [u8; 32],
) -> Result<miden_protocol::account::Account> {
    let counter_slot = StorageSlot::with_value(
        StorageSlotName::new(COUNTER_SLOT_NAME)?,
        Word::new([Felt::new(0); 4]),
    );
    let metadata = AccountComponentMetadata::new(COMPONENT_NAME, AccountType::all());
    let component = AccountComponent::new((*library).clone(), vec![counter_slot], metadata)?;

    let account = AccountBuilder::new(init_seed)
        .account_type(AccountType::RegularAccountImmutableCode)
        .storage_mode(AccountStorageMode::Network)
        .with_auth_component(NoAuth)
        .with_component(component)
        .build_existing()?;
    Ok(account)
}

/// Builds an `update_state` note with empty storage, tagged at the state
/// machine, and carrying a `NetworkAccountTarget` attachment so the network
/// transaction builder picks it up. This is both the shape the testnet
/// binary seeds the chain with and the shape the MASM re-emits on every tick.
fn build_update_state_note(
    sender: miden_protocol::account::AccountId,
    target: miden_protocol::account::AccountId,
    note_script: NoteScript,
    serial_num: Word,
) -> Result<Note> {
    let recipient = NoteRecipient::new(serial_num, note_script, NoteStorage::default());
    // Tag is left at default (0). For attachment-targeted network notes the
    // builder finds them via the NetworkAccountTarget attachment, not the
    // tag - mirroring `b2agg_note.rs::B2AggNote::create`.
    let attachment = NetworkAccountTarget::new(target, StdNoteExecutionHint::Always)?;
    let metadata = NoteMetadata::new(sender, NoteType::Public).with_attachment(attachment.into());
    Ok(Note::new(NoteAssets::default(), metadata, recipient))
}

/// Reads the state machine account's counter (storage slot named "counter").
fn read_counter(account: &miden_protocol::account::Account) -> u64 {
    let slot_name = StorageSlotName::new(COUNTER_SLOT_NAME).expect("counter slot name");
    let word: Word = account
        .storage()
        .get_item(&slot_name)
        .expect("counter slot must exist");
    word.get(0)
        .copied()
        .expect("word has 4 felts")
        .as_canonical_u64()
}

/// Returns the next-iteration serial number that the MASM will pick: only the
/// top felt of the word is bumped (`push.1 add` after `get_serial_number`).
fn next_serial(serial_num: Word) -> Word {
    Word::new([
        Felt::new(serial_num.get(0).unwrap().as_canonical_u64() + 1),
        serial_num.get(1).copied().unwrap(),
        serial_num.get(2).copied().unwrap(),
        serial_num.get(3).copied().unwrap(),
    ])
}

// ---------------------------------------------------------------------------
// TESTS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tick_increments_counter_and_re_emits_identical_note() -> Result<()> {
    let library = assemble_state_machine_library()?;
    let note_script = compile_note_script(&library)?;
    let script_root = note_script.root();

    let mut builder = MockChain::builder();
    let alice = builder.add_existing_wallet(Auth::IncrNonce)?;
    let state_machine = build_state_machine(library.clone(), [7u8; 32])?;
    builder.add_account(state_machine.clone())?;

    let seed_serial = Word::new([Felt::new(11), Felt::new(22), Felt::new(33), Felt::new(44)]);
    let seed_note = build_update_state_note(
        alice.id(),
        state_machine.id(),
        note_script.clone(),
        seed_serial,
    )?;

    builder.add_output_note(RawOutputNote::Full(seed_note.clone()));

    let mut chain = builder.build()?;

    // The MASM emits a public note whose recipient digest the kernel can
    // compute from `(serial+1, same script, empty storage)`. For PUBLIC
    // notes the executor needs the recipient *preimage* available before
    // tx execution; on testnet this comes from the chain's note transport,
    // here we pre-stage it via `extend_expected_output_notes`.
    let expected_emitted = build_update_state_note(
        state_machine.id(),
        state_machine.id(),
        note_script.clone(),
        next_serial(seed_serial),
    )?;

    // -------------------------------------------------------------------
    // Tick 1: state_machine consumes the seed note.
    // -------------------------------------------------------------------
    let executed = chain
        .build_tx_context(state_machine.id(), &[seed_note.id()], &[])?
        .extend_expected_output_notes(vec![RawOutputNote::Full(expected_emitted.clone())])
        .build()?
        .execute()
        .await?;

    chain.add_pending_executed_transaction(&executed)?;
    chain.prove_next_block()?;

    // (1) counter incremented to 1
    let after_tick1 = chain.committed_account(state_machine.id())?;
    assert_eq!(read_counter(after_tick1), 1, "counter must tick to 1");

    // (2) exactly one output note
    let output_notes = executed.output_notes();
    assert_eq!(
        output_notes.num_notes(),
        1,
        "tick must emit exactly one output note"
    );
    let emitted = output_notes.iter().next().unwrap();

    // (3) recipient digest matches what we'd build with serial_num+1
    let expected_recipient = NoteRecipient::new(
        next_serial(seed_serial),
        note_script.clone(),
        NoteStorage::default(),
    );
    let emitted_recipient = emitted
        .recipient()
        .expect("public output notes carry their recipient");
    assert_eq!(
        emitted_recipient.digest(),
        expected_recipient.digest(),
        "emitted note recipient must match (serial_num+1, same script, empty storage)"
    );
    assert_eq!(
        emitted_recipient.script().root(),
        script_root,
        "emitted note must reuse the update_state script"
    );

    // (4) tag is the default (0) - network notes target via the attachment,
    // not the tag.
    assert_eq!(
        emitted.metadata().tag(),
        NoteTag::default(),
        "emitted note must use DEFAULT_TAG (network notes target via attachment)"
    );

    // (4b) and the attachment carries the right network account target
    let expected_attachment =
        NetworkAccountTarget::new(state_machine.id(), StdNoteExecutionHint::Always)?;
    let actual_attachment = NetworkAccountTarget::try_from(emitted.metadata().attachment())
        .expect("emitted note must carry a NetworkAccountTarget attachment");
    assert_eq!(
        actual_attachment, expected_attachment,
        "emitted note's attachment must target the state machine with Always hint"
    );

    // (5) sender = state machine (so the network can identify the producer)
    assert_eq!(
        emitted.metadata().sender(),
        state_machine.id(),
        "emitted note's sender must be the state machine"
    );

    Ok(())
}

#[tokio::test]
async fn chain_self_perpetuates_through_two_ticks() -> Result<()> {
    let library = assemble_state_machine_library()?;
    let note_script = compile_note_script(&library)?;

    let mut builder = MockChain::builder();
    let alice = builder.add_existing_wallet(Auth::IncrNonce)?;
    let state_machine = build_state_machine(library.clone(), [11u8; 32])?;
    builder.add_account(state_machine.clone())?;

    let seed_serial = Word::new([
        Felt::new(101),
        Felt::new(202),
        Felt::new(303),
        Felt::new(404),
    ]);
    let seed_note = build_update_state_note(
        alice.id(),
        state_machine.id(),
        note_script.clone(),
        seed_serial,
    )?;
    builder.add_output_note(RawOutputNote::Full(seed_note.clone()));

    let mut chain = builder.build()?;

    // Pre-compute the note we expect the MASM to emit (see test 1 for why).
    let next_note_serial = next_serial(seed_serial);
    let next_note_after_tick1 = build_update_state_note(
        state_machine.id(),
        state_machine.id(),
        note_script.clone(),
        next_note_serial,
    )?;

    // ---- Tick 1 ----
    let exec1 = chain
        .build_tx_context(state_machine.id(), &[seed_note.id()], &[])?
        .extend_expected_output_notes(vec![RawOutputNote::Full(next_note_after_tick1.clone())])
        .build()?
        .execute()
        .await?;
    chain.add_pending_executed_transaction(&exec1)?;
    chain.prove_next_block()?;
    assert_eq!(
        read_counter(chain.committed_account(state_machine.id())?),
        1,
        "counter after tick 1"
    );

    // Sanity: the note we reconstructed is the same one the executor produced.
    let emitted = exec1.output_notes().iter().next().unwrap();
    assert_eq!(
        emitted.id(),
        next_note_after_tick1.id(),
        "reconstructed note id must match the emitted output note id"
    );

    // The next-next note (after tick 2 consumes next_note_after_tick1) has
    // serial = next_serial(next_serial(seed_serial)).
    let next_note_after_tick2 = build_update_state_note(
        state_machine.id(),
        state_machine.id(),
        note_script.clone(),
        next_serial(next_note_serial),
    )?;

    // ---- Tick 2 ----
    // The note from tick 1 is not in the committed chain (the kernel only kept
    // its header), so feed the full Note via the unauthenticated_notes slot.
    let exec2 = chain
        .build_tx_context(state_machine.id(), &[], &[next_note_after_tick1.clone()])?
        .extend_expected_output_notes(vec![RawOutputNote::Full(next_note_after_tick2)])
        .build()?
        .execute()
        .await?;
    chain.add_pending_executed_transaction(&exec2)?;
    chain.prove_next_block()?;
    assert_eq!(
        read_counter(chain.committed_account(state_machine.id())?),
        2,
        "counter after tick 2"
    );

    // Tick 2 must also emit exactly one note (continuing the chain).
    assert_eq!(
        exec2.output_notes().num_notes(),
        1,
        "tick 2 must also emit exactly one output note"
    );

    Ok(())
}
