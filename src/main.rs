use std::{fs, path::Path, sync::Arc};

use miden_client::account::component::BasicWallet;
use miden_client::{
    address::NetworkId,
    auth::AuthSecretKey,
    builder::ClientBuilder,
    crypto::FeltRng,
    keystore::FilesystemKeyStore,
    note::{
        Note, NoteAssets, NoteExecutionHint, NoteInputs, NoteMetadata, NoteRecipient, NoteTag,
        NoteType,
    },
    rpc::{Endpoint, GrpcClient},
    store::TransactionFilter,
    transaction::{OutputNote, TransactionId, TransactionRequestBuilder, TransactionStatus},
    Client, ClientError, Felt, Word,
};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_lib::account::auth::{self, AuthRpoFalcon512};
use miden_lib::transaction::TransactionKernel;
use miden_objects::{
    account::{AccountBuilder, AccountComponent, AccountStorageMode, AccountType, StorageSlot},
    assembly::{Assembler, DefaultSourceManager, Library, LibraryPath, Module, ModuleKind},
};
use rand::{rngs::StdRng, RngCore};
use tokio::time::{sleep, Duration};

const TARGET_TICKS: u64 = 5;
const POLL_INTERVAL_SECS: u64 = 2;
const MAX_WAIT_SECS: u64 = 240;

// Source: miden-tutorials/rust-client/src/bin/network_notes_counter_contract.rs (lines 30-59)
async fn wait_for_tx(
    client: &mut Client<FilesystemKeyStore<StdRng>>,
    tx_id: TransactionId,
) -> Result<(), ClientError> {
    loop {
        client.sync_state().await?;
        let txs = client
            .get_transactions(TransactionFilter::Ids(vec![tx_id]))
            .await?;
        let tx_committed = if !txs.is_empty() {
            matches!(txs[0].status, TransactionStatus::Committed { .. })
        } else {
            false
        };
        if tx_committed {
            println!("✅ tx {} committed", tx_id.to_hex());
            break;
        }
        println!("   waiting on tx {}...", tx_id.to_hex());
        sleep(Duration::from_secs(2)).await;
    }
    Ok(())
}

// Source: miden-tutorials/rust-client/src/bin/network_notes_counter_contract.rs (lines 62-75)
fn create_library(
    account_code: String,
    library_path: &str,
) -> Result<Library, Box<dyn std::error::Error>> {
    let assembler: Assembler = TransactionKernel::assembler().with_debug_mode(true);
    let source_manager = Arc::new(DefaultSourceManager::default());
    let module = Module::parser(ModuleKind::Library).parse_str(
        LibraryPath::new(library_path)?,
        account_code,
        &source_manager,
    )?;
    let library = assembler.clone().assemble_library([module])?;
    Ok(library)
}

fn read_counter(account: &miden_client::account::Account) -> u64 {
    let word: Word = account.storage().get_item(0).unwrap().into();
    word.get(3).unwrap().as_int()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // -------------------------------------------------------------------------
    // CLIENT
    // -------------------------------------------------------------------------
    let endpoint = Endpoint::testnet();
    let rpc_client = Arc::new(GrpcClient::new(&endpoint, 10_000));

    let keystore = Arc::new(
        FilesystemKeyStore::<StdRng>::new(std::path::PathBuf::from("./keystore")).unwrap(),
    );

    let mut client = ClientBuilder::new()
        .rpc(rpc_client)
        .sqlite_store(std::path::PathBuf::from("./store.sqlite3"))
        .authenticator(keystore.clone())
        .in_debug_mode(true.into())
        .build()
        .await?;

    let sync = client.sync_state().await?;
    println!("Latest block: {}", sync.block_num);

    // -------------------------------------------------------------------------
    // STEP 1: Alice (only used to submit the seed note)
    // -------------------------------------------------------------------------
    println!("\n[STEP 1] Creating Alice's account");
    let mut init_seed = [0_u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let key_pair = AuthSecretKey::new_rpo_falcon512();

    let alice_account = AccountBuilder::new(init_seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthRpoFalcon512::new(key_pair.public_key().to_commitment()))
        .with_component(BasicWallet)
        .build()?;

    client.add_account(&alice_account, false).await?;
    keystore.add_key(&key_pair).unwrap();
    println!(
        "   Alice: {}",
        alice_account.id().to_bech32(NetworkId::Testnet)
    );

    // -------------------------------------------------------------------------
    // STEP 2: Network state-machine account
    // -------------------------------------------------------------------------
    println!("\n[STEP 2] Building network state-machine contract");
    let state_machine_code =
        fs::read_to_string(Path::new("masm/accounts/state_machine.masm")).unwrap();

    let assembler: Assembler = TransactionKernel::assembler().with_debug_mode(true);
    let state_machine_component = AccountComponent::compile(
        &state_machine_code,
        assembler.clone(),
        vec![StorageSlot::Value([Felt::new(0); 4].into())],
    )?
    .with_supports_all_types();

    let mut init_seed = [0_u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let state_machine = AccountBuilder::new(init_seed)
        .account_type(AccountType::RegularAccountImmutableCode)
        .storage_mode(AccountStorageMode::Network)
        .with_auth_component(auth::NoAuth)
        .with_component(state_machine_component)
        .build()?;

    client.add_account(&state_machine, false).await?;
    println!(
        "   State machine: {}",
        state_machine.id().to_bech32(NetworkId::Testnet)
    );

    // -------------------------------------------------------------------------
    // STEP 3: Deploy with empty tx script
    // -------------------------------------------------------------------------
    println!("\n[STEP 3] Deploying state machine (empty tx script)");
    let deploy_code = fs::read_to_string(Path::new("masm/scripts/deploy.masm")).unwrap();
    let deploy_script = client.script_builder().compile_tx_script(&deploy_code)?;

    let deploy_req = TransactionRequestBuilder::new()
        .custom_script(deploy_script)
        .build()?;

    let deploy_tx_id = client
        .submit_new_transaction(state_machine.id(), deploy_req)
        .await?;
    println!(
        "   https://testnet.midenscan.com/tx/{}",
        deploy_tx_id.to_hex()
    );
    wait_for_tx(&mut client, deploy_tx_id).await?;

    // -------------------------------------------------------------------------
    // STEP 4: Seed update_state note (Alice -> network state machine)
    // -------------------------------------------------------------------------
    println!("\n[STEP 4] Seeding the chain with one update_state note");
    let note_code = fs::read_to_string(Path::new("masm/notes/update_state.masm")).unwrap();
    let library = create_library(
        state_machine_code.clone(),
        "external_contract::state_machine",
    )?;

    let note_script = client
        .script_builder()
        .with_dynamically_linked_library(&library)?
        .compile_note_script(&note_code)?;

    let tag = NoteTag::from_account_id(state_machine.id());

    // Inputs are exactly one word: [tag, 0, 0, 0]. The MASM reads the tag back
    // out and re-uses it for the note it emits, keeping the chain self-tagged.
    let note_inputs = NoteInputs::new(vec![tag.into(), Felt::new(0), Felt::new(0), Felt::new(0)])?;

    let serial_num = client.rng().draw_word();
    let recipient = NoteRecipient::new(serial_num, note_script, note_inputs);

    let metadata = NoteMetadata::new(
        alice_account.id(),
        NoteType::Public,
        tag,
        NoteExecutionHint::always(),
        Felt::new(0),
    )?;

    let seed_note = Note::new(NoteAssets::default(), metadata, recipient);

    let seed_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(seed_note)])
        .build()?;

    let seed_tx_id = client
        .submit_new_transaction(alice_account.id(), seed_req)
        .await?;
    println!(
        "   https://testnet.midenscan.com/tx/{}",
        seed_tx_id.to_hex()
    );
    wait_for_tx(&mut client, seed_tx_id).await?;

    // -------------------------------------------------------------------------
    // STEP 5: Watch the network drive the chain
    // -------------------------------------------------------------------------
    println!(
        "\n[STEP 5] Watching state machine ticks (target = {})...",
        TARGET_TICKS
    );

    let mut last_seen: u64 = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(MAX_WAIT_SECS);

    loop {
        sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
        client.sync_state().await?;

        let record = client.get_account(state_machine.id()).await?;
        if let Some(record) = record {
            let count = read_counter(record.account());
            if count != last_seen {
                println!("   tick: counter = {}", count);
                last_seen = count;
            }
            if count >= TARGET_TICKS {
                break;
            }
        }

        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out after {}s waiting for counter to reach {} (last seen: {})",
                MAX_WAIT_SECS, TARGET_TICKS, last_seen
            )
            .into());
        }
    }

    assert!(last_seen >= TARGET_TICKS, "counter never reached target");

    println!(
        "\n✅ Self-perpetuating chain confirmed. Final counter: {}",
        last_seen
    );
    println!(
        "   Account: https://testnet.midenscan.com/account/{}",
        state_machine.id().to_bech32(NetworkId::Testnet)
    );
    println!("   The chain keeps ticking on testnet after this binary exits.");

    Ok(())
}
