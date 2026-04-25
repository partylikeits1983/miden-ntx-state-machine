use std::{fs, path::Path as FsPath, sync::Arc};

use miden_client::account::component::BasicWallet;
use miden_client::account::{
    AccountBuilder, AccountComponent, AccountStorageMode, AccountType, NetworkId, StorageSlot,
    StorageSlotName,
};
use miden_client::auth::{AuthScheme, AuthSecretKey, NoAuth};
use miden_client::builder::ClientBuilder;
use miden_client::crypto::FeltRng;
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::note::{Note, NoteAssets, NoteMetadata, NoteRecipient, NoteStorage, NoteType};
use miden_client::rpc::{Endpoint, GrpcClient};
use miden_client::store::TransactionFilter;
use miden_client::transaction::{TransactionId, TransactionRequestBuilder, TransactionStatus};
use miden_client::{Client, ClientError, Felt, Word};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::account::component::AccountComponentMetadata;
use miden_protocol::assembly::{
    Assembler, DefaultSourceManager, Library, Module, ModuleKind, SourceManagerSync,
};
use miden_protocol::transaction::TransactionKernel;
use miden_standards::account::auth::AuthSingleSig;
use miden_standards::note::{NetworkAccountTarget, NoteExecutionHint as StdNoteExecutionHint};
use rand::RngCore;
use tokio::time::{sleep, Duration};

const TARGET_TICKS: u64 = 5;
const POLL_INTERVAL_SECS: u64 = 2;
const MAX_WAIT_SECS: u64 = 240;

const STATE_MACHINE_LIB_PATH: &str = "external_contract::state_machine";
const STATE_MACHINE_COMPONENT_NAME: &str = "miden_ntx_state_machine::state_machine";
const COUNTER_SLOT_NAME: &str = "miden_ntx_state_machine::counter";

async fn wait_for_tx(
    client: &mut Client<FilesystemKeyStore>,
    tx_id: TransactionId,
) -> Result<(), ClientError> {
    loop {
        client.sync_state().await?;
        let txs = client
            .get_transactions(TransactionFilter::Ids(vec![tx_id]))
            .await?;
        let committed = txs
            .first()
            .is_some_and(|t| matches!(t.status, TransactionStatus::Committed { .. }));
        if committed {
            println!("✅ tx {} committed", tx_id.to_hex());
            return Ok(());
        }
        println!("   waiting on tx {}...", tx_id.to_hex());
        sleep(Duration::from_secs(2)).await;
    }
}

fn assemble_state_machine_library(
    state_machine_code: &str,
) -> Result<Arc<Library>, Box<dyn std::error::Error>> {
    // Parse and assemble through the SAME source manager - otherwise the
    // assembler can't resolve spans the parser produced and panics with
    // "invalid source span: starting byte is out of bounds".
    let source_manager: Arc<dyn SourceManagerSync> = Arc::new(DefaultSourceManager::default());
    let assembler: Assembler =
        TransactionKernel::assembler_with_source_manager(source_manager.clone())
            .with_dynamic_library(miden_standards::StandardsLib::default())
            .map_err(|e| -> Box<dyn std::error::Error> {
                format!("link standards lib: {e}").into()
            })?;
    let module = Module::parser(ModuleKind::Library).parse_str(
        STATE_MACHINE_LIB_PATH,
        state_machine_code,
        source_manager,
    )?;
    let library = assembler.assemble_library([module])?;
    Ok(library)
}

fn read_counter(account: &miden_client::account::Account) -> u64 {
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // -------------------------------------------------------------------------
    // CLIENT
    // -------------------------------------------------------------------------
    let endpoint = Endpoint::testnet();
    let rpc_client = Arc::new(GrpcClient::new(&endpoint, 10_000));

    let keystore = Arc::new(FilesystemKeyStore::new(std::path::PathBuf::from(
        "./keystore",
    ))?);

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
    let key_pair = AuthSecretKey::new_falcon512_poseidon2();
    let pub_key_commitment = key_pair.public_key().to_commitment();

    let alice_account = AccountBuilder::new(init_seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthSingleSig::new(
            pub_key_commitment,
            AuthScheme::Falcon512Poseidon2,
        ))
        .with_component(BasicWallet)
        .build()?;

    client.add_account(&alice_account, false).await?;
    keystore.add_key(&key_pair, alice_account.id()).await?;
    println!(
        "   Alice: {}",
        alice_account.id().to_bech32(NetworkId::Testnet)
    );

    // -------------------------------------------------------------------------
    // STEP 2: Network state-machine account (one storage slot: "counter")
    // -------------------------------------------------------------------------
    println!("\n[STEP 2] Building network state-machine contract");
    let state_machine_code =
        fs::read_to_string(FsPath::new("masm/accounts/state_machine.masm")).unwrap();

    let library = assemble_state_machine_library(&state_machine_code)?;

    let counter_slot = StorageSlot::with_value(
        StorageSlotName::new(COUNTER_SLOT_NAME)?,
        Word::new([Felt::new(0); 4]),
    );
    let component_metadata =
        AccountComponentMetadata::new(STATE_MACHINE_COMPONENT_NAME, AccountType::all());

    let state_machine_component =
        AccountComponent::new((*library).clone(), vec![counter_slot], component_metadata)?;

    let mut init_seed = [0_u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let state_machine = AccountBuilder::new(init_seed)
        .account_type(AccountType::RegularAccountImmutableCode)
        .storage_mode(AccountStorageMode::Network)
        .with_auth_component(NoAuth)
        .with_component(state_machine_component)
        .build()?;

    client.add_account(&state_machine, false).await?;
    println!(
        "   State machine: {}",
        state_machine.id().to_bech32(NetworkId::Testnet)
    );

    // -------------------------------------------------------------------------
    // STEP 3: Deploy. NoAuth only bumps the nonce when account state changes,
    //         so a literal `begin nop end` script wouldn't deploy. We bump the
    //         counter once instead - that puts the counter at 1 after deploy.
    // -------------------------------------------------------------------------
    println!("\n[STEP 3] Deploying state machine");
    let deploy_script = client
        .code_builder()
        .with_dynamically_linked_library(&library)?
        .compile_tx_script(
            "use external_contract::state_machine\nbegin call.state_machine::increment_count end",
        )?;

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
    let note_code = fs::read_to_string(FsPath::new("masm/notes/update_state.masm")).unwrap();

    let note_script = client
        .code_builder()
        .with_dynamically_linked_library(&library)?
        .compile_note_script(&note_code)?;

    let serial_num = client.rng().draw_word();
    let recipient = NoteRecipient::new(serial_num, note_script.clone(), NoteStorage::default());

    // The attachment is what tells the network transaction builder "this
    // note is consumable by network account X with execution hint Y". The
    // tag is left at its default of 0 (DEFAULT_TAG) - for attachment-
    // targeted notes the network builder finds them via the attachment, NOT
    // the tag. See miden-base/.../note_tag/mod.masm DEFAULT_TAG comment.
    let target = NetworkAccountTarget::new(state_machine.id(), StdNoteExecutionHint::Always)?;
    let metadata =
        NoteMetadata::new(alice_account.id(), NoteType::Public).with_attachment(target.into());

    let seed_note = Note::new(NoteAssets::default(), metadata, recipient);

    // Register the note's script with the node BEFORE submitting the seed
    // tx. The network transaction builder needs the script in its registry
    // to be able to construct the consumption tx; without this step the
    // builder sees the seed note's recipient digest but can't fetch the
    // preimage, so the note sits on chain forever.
    //
    // `expected_ntx_scripts(...)` causes `submit_new_transaction` to call
    // `ensure_ntx_scripts_registered` first, which submits a registration
    // tx (one per missing script) and waits for it to commit.
    let seed_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![seed_note])
        .expected_ntx_scripts(vec![note_script.clone()])
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

        if let Some(account) = client.get_account(state_machine.id()).await? {
            let count = read_counter(&account);
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
