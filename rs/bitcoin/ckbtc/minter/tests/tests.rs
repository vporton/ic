use bitcoin::util::psbt::serialize::Deserialize;
use bitcoin::{Address as BtcAddress, Network as BtcNetwork};
use candid::{Decode, Encode, Nat, Principal};
use ic_base_types::{CanisterId, PrincipalId};
use ic_bitcoin_canister_mock::{OutPoint, PushUtxoToAddress, Utxo};
use ic_btc_interface::Network;
use ic_ckbtc_kyt::{InitArg as KytInitArg, KytMode, LifecycleArg, SetApiKeyArg};
use ic_ckbtc_minter::lifecycle::init::{InitArgs as CkbtcMinterInitArgs, MinterArg};
use ic_ckbtc_minter::lifecycle::upgrade::UpgradeArgs;
use ic_ckbtc_minter::queries::{EstimateFeeArg, RetrieveBtcStatusRequest, WithdrawalFee};
use ic_ckbtc_minter::state::{Mode, RetrieveBtcStatus};
use ic_ckbtc_minter::updates::get_btc_address::GetBtcAddressArgs;
use ic_ckbtc_minter::updates::retrieve_btc::{RetrieveBtcArgs, RetrieveBtcError, RetrieveBtcOk};
use ic_ckbtc_minter::updates::update_balance::{UpdateBalanceArgs, UpdateBalanceError, UtxoStatus};
use ic_ckbtc_minter::MinterInfo;
use ic_icrc1_ledger::{ArchiveOptions, InitArgs as LedgerInitArgs, LedgerArgument};
use ic_state_machine_tests::{Cycles, StateMachine, StateMachineBuilder, WasmResult};
use ic_test_utilities_load_wasm::load_wasm;
use icrc_ledger_types::icrc1::account::Account;
use icrc_ledger_types::icrc1::transfer::{TransferArg, TransferError};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

const KYT_FEE: u64 = 2_000;
const TRANSFER_FEE: u64 = 10;
const MIN_CONFIRMATIONS: u32 = 12;
const MAX_TIME_IN_QUEUE: Duration = Duration::from_secs(10);

fn ledger_wasm() -> Vec<u8> {
    let path = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("rosetta-api")
        .join("icrc1")
        .join("ledger");
    load_wasm(path, "ic-icrc1-ledger", &[])
}

fn minter_wasm() -> Vec<u8> {
    load_wasm(
        std::env::var("CARGO_MANIFEST_DIR").unwrap(),
        "ic-ckbtc-minter",
        &[],
    )
}

fn bitcoin_mock_wasm() -> Vec<u8> {
    load_wasm(
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .parent()
            .unwrap()
            .join("mock"),
        "ic-bitcoin-canister-mock",
        &[],
    )
}

fn kyt_wasm() -> Vec<u8> {
    load_wasm(
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .parent()
            .unwrap()
            .join("kyt"),
        "ic-ckbtc-kyt",
        &[],
    )
}

fn install_ledger(env: &StateMachine) -> CanisterId {
    let args = LedgerArgument::Init(LedgerInitArgs {
        minting_account: Account {
            owner: Principal::anonymous(),
            subaccount: None,
        },
        initial_balances: vec![],
        transfer_fee: 0,
        token_name: "Test Token".to_string(),
        token_symbol: "TST".to_string(),
        metadata: vec![],
        archive_options: ArchiveOptions {
            trigger_threshold: 0,
            num_blocks_to_archive: 0,
            node_max_memory_size_bytes: None,
            max_message_size_bytes: None,
            controller_id: Default::default(),
            cycles_for_archive_creation: None,
            max_transactions_per_response: None,
        },
        fee_collector_account: None,
        max_memo_length: None,
    });
    env.install_canister(ledger_wasm(), Encode!(&args).unwrap(), None)
        .unwrap()
}

fn install_minter(env: &StateMachine, ledger_id: CanisterId) -> CanisterId {
    let args = CkbtcMinterInitArgs {
        btc_network: Network::Regtest,
        /// The name of the [EcdsaKeyId]. Use "dfx_test_key" for local replica and "test_key_1" for
        /// a testing key for testnet and mainnet
        ecdsa_key_name: "dfx_test_key".parse().unwrap(),
        retrieve_btc_min_amount: 0,
        ledger_id,
        max_time_in_queue_nanos: 0,
        min_confirmations: Some(1),
        mode: Mode::GeneralAvailability,
        kyt_fee: None,
        kyt_principal: Some(CanisterId::from(0)),
    };
    let minter_arg = MinterArg::Init(args);
    env.install_canister(minter_wasm(), Encode!(&minter_arg).unwrap(), None)
        .unwrap()
}

fn assert_reply(result: WasmResult) -> Vec<u8> {
    match result {
        WasmResult::Reply(bytes) => bytes,
        WasmResult::Reject(reject) => {
            panic!("Expected a successful reply, got a reject: {}", reject)
        }
    }
}

#[test]
fn test_install_ckbtc_minter_canister() {
    let env = StateMachine::new();
    let ledger_id = install_ledger(&env);
    install_minter(&env, ledger_id);
}

#[test]
fn test_upgrade_read_only() {
    let env = StateMachine::new();
    let ledger_id = install_ledger(&env);
    let minter_id = install_minter(&env, ledger_id);

    let authorized_principal =
        Principal::from_str("k2t6j-2nvnp-4zjm3-25dtz-6xhaa-c7boj-5gayf-oj3xs-i43lp-teztq-6ae")
            .unwrap();

    // upgrade
    let upgrade_args = UpgradeArgs {
        retrieve_btc_min_amount: Some(100),
        min_confirmations: None,
        max_time_in_queue_nanos: Some(100),
        mode: Some(Mode::ReadOnly),
        kyt_principal: Some(CanisterId::from(0)),
        kyt_fee: None,
    };
    let minter_arg = MinterArg::Upgrade(Some(upgrade_args));
    env.upgrade_canister(minter_id, minter_wasm(), Encode!(&minter_arg).unwrap())
        .expect("Failed to upgrade the minter canister");

    // when the mode is ReadOnly then the minter should reject all update calls.

    // 1. update_balance
    let update_balance_args = UpdateBalanceArgs {
        owner: None,
        subaccount: None,
    };
    let res = env
        .execute_ingress_as(
            authorized_principal.into(),
            minter_id,
            "update_balance",
            Encode!(&update_balance_args).unwrap(),
        )
        .expect("Failed to call update_balance");
    let res = Decode!(&res.bytes(), Result<Vec<UtxoStatus>, UpdateBalanceError>).unwrap();
    assert!(
        matches!(res, Err(UpdateBalanceError::TemporarilyUnavailable(_))),
        "unexpected result: {:?}",
        res
    );

    // 2. retrieve_btc
    let retrieve_btc_args = RetrieveBtcArgs {
        amount: 10,
        address: "".into(),
    };
    let res = env
        .execute_ingress_as(
            authorized_principal.into(),
            minter_id,
            "retrieve_btc",
            Encode!(&retrieve_btc_args).unwrap(),
        )
        .expect("Failed to call retrieve_btc");
    let res = Decode!(&res.bytes(), Result<RetrieveBtcOk, RetrieveBtcError>).unwrap();
    assert!(
        matches!(res, Err(RetrieveBtcError::TemporarilyUnavailable(_))),
        "unexpected result: {:?}",
        res
    );
}

#[test]
fn test_upgrade_restricted() {
    let env = StateMachine::new();
    let ledger_id = install_ledger(&env);
    let minter_id = install_minter(&env, ledger_id);

    let authorized_principal =
        Principal::from_str("k2t6j-2nvnp-4zjm3-25dtz-6xhaa-c7boj-5gayf-oj3xs-i43lp-teztq-6ae")
            .unwrap();

    let unauthorized_principal =
        Principal::from_str("gjfkw-yiolw-ncij7-yzhg2-gq6ec-xi6jy-feyni-g26f4-x7afk-thx6z-6ae")
            .unwrap();

    // upgrade
    let upgrade_args = UpgradeArgs {
        retrieve_btc_min_amount: Some(100),
        min_confirmations: None,
        max_time_in_queue_nanos: Some(100),
        mode: Some(Mode::RestrictedTo(vec![authorized_principal])),
        kyt_fee: None,
        kyt_principal: Some(CanisterId::from(0)),
    };
    let minter_arg = MinterArg::Upgrade(Some(upgrade_args));
    env.upgrade_canister(minter_id, minter_wasm(), Encode!(&minter_arg).unwrap())
        .expect("Failed to upgrade the minter canister");

    // Check that the unauthorized user cannot modify the state.

    // 1. update_balance
    let update_balance_args = UpdateBalanceArgs {
        owner: None,
        subaccount: None,
    };
    let res = env
        .execute_ingress_as(
            unauthorized_principal.into(),
            minter_id,
            "update_balance",
            Encode!(&update_balance_args).unwrap(),
        )
        .expect("Failed to call update_balance");
    let res = Decode!(&res.bytes(), Result<Vec<UtxoStatus>, UpdateBalanceError>).unwrap();
    assert!(
        matches!(res, Err(UpdateBalanceError::TemporarilyUnavailable(_))),
        "unexpected result: {:?}",
        res
    );

    // 2. retrieve_btc
    let retrieve_btc_args = RetrieveBtcArgs {
        amount: 10,
        address: "".into(),
    };
    let res = env
        .execute_ingress_as(
            unauthorized_principal.into(),
            minter_id,
            "retrieve_btc",
            Encode!(&retrieve_btc_args).unwrap(),
        )
        .expect("Failed to call retrieve_btc");
    let res = Decode!(&res.bytes(), Result<RetrieveBtcOk, RetrieveBtcError>).unwrap();
    assert!(
        matches!(res, Err(RetrieveBtcError::TemporarilyUnavailable(_))),
        "unexpected result: {:?}",
        res
    );

    // Test restricted BTC deposits.
    let upgrade_args = UpgradeArgs {
        retrieve_btc_min_amount: Some(100),
        min_confirmations: None,
        max_time_in_queue_nanos: Some(100),
        mode: Some(Mode::DepositsRestrictedTo(vec![authorized_principal])),
        kyt_principal: Some(CanisterId::from(0)),
        kyt_fee: None,
    };
    env.upgrade_canister(minter_id, minter_wasm(), Encode!(&upgrade_args).unwrap())
        .expect("Failed to upgrade the minter canister");

    let update_balance_args = UpdateBalanceArgs {
        owner: None,
        subaccount: None,
    };

    let res = env
        .execute_ingress_as(
            unauthorized_principal.into(),
            minter_id,
            "update_balance",
            Encode!(&update_balance_args).unwrap(),
        )
        .expect("Failed to call update_balance");
    let res = Decode!(&res.bytes(), Result<Vec<UtxoStatus>, UpdateBalanceError>).unwrap();
    assert!(
        matches!(res, Err(UpdateBalanceError::TemporarilyUnavailable(_))),
        "unexpected result: {:?}",
        res
    );
}

#[test]
fn test_illegal_caller() {
    let env = StateMachine::new();
    let ledger_id = install_ledger(&env);
    let minter_id = install_minter(&env, ledger_id);

    let authorized_principal =
        Principal::from_str("k2t6j-2nvnp-4zjm3-25dtz-6xhaa-c7boj-5gayf-oj3xs-i43lp-teztq-6ae")
            .unwrap();

    // update_balance with minter's principal as target
    let update_balance_args = UpdateBalanceArgs {
        owner: Some(Principal::from_str(&minter_id.get().to_string()).unwrap()),
        subaccount: None,
    };
    // This call should panick
    let res = env.execute_ingress_as(
        authorized_principal.into(),
        minter_id,
        "update_balance",
        Encode!(&update_balance_args).unwrap(),
    );
    assert!(res.is_err());
    // Anonynmous call should fail
    let res = env.execute_ingress(
        minter_id,
        "update_balance",
        Encode!(&update_balance_args).unwrap(),
    );
    assert!(res.is_err());
}

pub fn get_btc_address(
    env: &StateMachine,
    minter_id: CanisterId,
    arg: &GetBtcAddressArgs,
) -> String {
    Decode!(
        &env.execute_ingress_as(
            CanisterId::from_u64(100).into(),
            minter_id,
            "get_btc_address",
            Encode!(arg).unwrap()
        )
        .expect("failed to transfer funds")
        .bytes(),
        String
    )
    .expect("failed to decode String response")
}

#[test]
fn test_minter() {
    use bitcoin::Address;

    let env = StateMachine::new();
    let args = MinterArg::Init(CkbtcMinterInitArgs {
        btc_network: Network::Regtest,
        ecdsa_key_name: "master_ecdsa_public_key".into(),
        retrieve_btc_min_amount: 100_000,
        ledger_id: CanisterId::from_u64(0),
        max_time_in_queue_nanos: MAX_TIME_IN_QUEUE.as_nanos() as u64,
        min_confirmations: Some(6_u32),
        mode: Mode::GeneralAvailability,
        kyt_fee: Some(1001),
        kyt_principal: None,
    });
    let args = Encode!(&args).unwrap();
    let minter_id = env.install_canister(minter_wasm(), args, None).unwrap();

    let btc_address_1 = get_btc_address(
        &env,
        minter_id,
        &GetBtcAddressArgs {
            owner: None,
            subaccount: None,
        },
    );
    let address_1 = Address::from_str(&btc_address_1).expect("invalid bitcoin address");
    let btc_address_2 = get_btc_address(
        &env,
        minter_id,
        &GetBtcAddressArgs {
            owner: None,
            subaccount: Some([1; 32]),
        },
    );
    let address_2 = Address::from_str(&btc_address_2).expect("invalid bitcoin address");
    assert_ne!(address_1, address_2);
}

fn mainnet_bitcoin_canister_id() -> CanisterId {
    CanisterId::try_from(
        PrincipalId::from_str(ic_config::execution_environment::BITCOIN_MAINNET_CANISTER_ID)
            .unwrap(),
    )
    .unwrap()
}

fn install_bitcoin_mock_canister(env: &StateMachine) {
    let args = Network::Mainnet;
    let cid = mainnet_bitcoin_canister_id();
    env.create_canister_with_cycles(Some(cid.into()), Cycles::new(0), None);

    env.install_existing_canister(cid, bitcoin_mock_wasm(), Encode!(&args).unwrap())
        .unwrap();
}

struct CkBtcSetup {
    pub env: StateMachine,
    pub caller: PrincipalId,
    pub bitcoin_id: CanisterId,
    pub ledger_id: CanisterId,
    pub minter_id: CanisterId,
    pub _kyt_id: CanisterId,
}

impl CkBtcSetup {
    pub fn new() -> Self {
        let bitcoin_id = mainnet_bitcoin_canister_id();
        let env = StateMachineBuilder::new()
            .with_default_canister_range()
            .with_extra_canister_range(bitcoin_id..=bitcoin_id)
            .build();

        install_bitcoin_mock_canister(&env);
        let ledger_id = env.create_canister(None);
        let minter_id =
            env.create_canister_with_cycles(None, Cycles::new(100_000_000_000_000), None);
        let kyt_id = env.create_canister(None);

        env.install_existing_canister(
            ledger_id,
            ledger_wasm(),
            Encode!(&LedgerArgument::Init(LedgerInitArgs {
                minting_account: Account {
                    owner: minter_id.into(),
                    subaccount: None,
                },
                initial_balances: vec![],
                transfer_fee: TRANSFER_FEE,
                token_name: "ckBTC".to_string(),
                token_symbol: "ckBTC".to_string(),
                metadata: vec![],
                archive_options: ArchiveOptions {
                    trigger_threshold: 0,
                    num_blocks_to_archive: 0,
                    node_max_memory_size_bytes: None,
                    max_message_size_bytes: None,
                    controller_id: Default::default(),
                    cycles_for_archive_creation: None,
                    max_transactions_per_response: None,
                },
                fee_collector_account: None,
                max_memo_length: None,
            }))
            .unwrap(),
        )
        .expect("failed to install the ledger");

        env.install_existing_canister(
            minter_id,
            minter_wasm(),
            Encode!(&MinterArg::Init(CkbtcMinterInitArgs {
                btc_network: Network::Mainnet,
                ecdsa_key_name: "master_ecdsa_public_key".to_string(),
                retrieve_btc_min_amount: 100_000,
                ledger_id,
                max_time_in_queue_nanos: 100,
                min_confirmations: Some(MIN_CONFIRMATIONS),
                mode: Mode::GeneralAvailability,
                kyt_fee: Some(KYT_FEE),
                kyt_principal: kyt_id.into(),
            }))
            .unwrap(),
        )
        .expect("failed to install the minter");

        let caller = PrincipalId::new_user_test_id(1);

        env.install_existing_canister(
            kyt_id,
            kyt_wasm(),
            Encode!(&LifecycleArg::InitArg(KytInitArg {
                minter_id: minter_id.into(),
                maintainers: vec![caller.into()],
                mode: KytMode::AcceptAll,
            }))
            .unwrap(),
        )
        .expect("failed to install the KYT canister");

        env.execute_ingress(
            bitcoin_id,
            "set_fee_percentiles",
            Encode!(&(1..=100).map(|i| i * 100).collect::<Vec<u64>>()).unwrap(),
        )
        .expect("failed to set fee percentiles");

        env.execute_ingress_as(
            caller,
            kyt_id,
            "set_api_key",
            Encode!(&SetApiKeyArg {
                api_key: "api key".to_string(),
            })
            .unwrap(),
        )
        .expect("failed to set api key");

        Self {
            env,
            caller,
            bitcoin_id,
            ledger_id,
            minter_id,
            _kyt_id: kyt_id,
        }
    }

    pub fn set_fee_percentiles(&self, fees: &Vec<u64>) {
        self.env
            .execute_ingress(
                self.bitcoin_id,
                "set_fee_percentiles",
                Encode!(fees).unwrap(),
            )
            .expect("failed to set fee percentiles");
    }

    pub fn push_utxo(&self, address: String, utxo: Utxo) {
        assert_reply(
            self.env
                .execute_ingress(
                    self.bitcoin_id,
                    "push_utxo_to_address",
                    Encode!(&PushUtxoToAddress { address, utxo }).unwrap(),
                )
                .expect("failed to push a UTXO"),
        );
    }

    pub fn get_btc_address(&self, account: impl Into<Account>) -> String {
        let account = account.into();
        Decode!(
            &assert_reply(
                self.env
                    .execute_ingress_as(
                        self.caller,
                        self.minter_id,
                        "get_btc_address",
                        Encode!(&GetBtcAddressArgs {
                            owner: Some(account.owner),
                            subaccount: account.subaccount,
                        })
                        .unwrap(),
                    )
                    .expect("failed to get btc address")
            ),
            String
        )
        .unwrap()
    }

    pub fn get_minter_info(&self) -> MinterInfo {
        Decode!(
            &assert_reply(
                self.env
                    .execute_ingress(self.minter_id, "get_minter_info", Encode!().unwrap(),)
                    .expect("failed to get minter info")
            ),
            MinterInfo
        )
        .unwrap()
    }

    pub fn refresh_fee_percentiles(&self) {
        Decode!(
            &assert_reply(
                self.env
                    .execute_ingress_as(
                        self.caller,
                        self.minter_id,
                        "refresh_fee_percentiles",
                        Encode!().unwrap()
                    )
                    .expect("failed to refresh fee percentiles")
            ),
            ()
        )
        .unwrap();
    }

    pub fn estimate_withdrawal_fee(&self, amount: Option<u64>) -> WithdrawalFee {
        self.refresh_fee_percentiles();
        Decode!(
            &assert_reply(
                self.env
                    .query(
                        self.minter_id,
                        "estimate_withdrawal_fee",
                        Encode!(&EstimateFeeArg { amount }).unwrap()
                    )
                    .expect("failed to query minter fee estimate")
            ),
            WithdrawalFee
        )
        .unwrap()
    }

    pub fn deposit_utxo(&self, account: impl Into<Account>, utxo: Utxo) {
        let account = account.into();
        let deposit_address = self.get_btc_address(account);

        self.push_utxo(deposit_address, utxo.clone());

        let utxo_status = Decode!(
            &assert_reply(
                self.env
                    .execute_ingress_as(
                        self.caller,
                        self.minter_id,
                        "update_balance",
                        Encode!(&UpdateBalanceArgs {
                            owner: Some(account.owner),
                            subaccount: account.subaccount,
                        })
                        .unwrap()
                    )
                    .expect("failed to update balance")
            ),
            Result<Vec<UtxoStatus>, UpdateBalanceError>
        )
        .unwrap();

        assert_eq!(
            utxo_status.unwrap(),
            vec![UtxoStatus::Minted {
                block_index: 0,
                minted_amount: utxo.value - KYT_FEE,
                utxo,
            }]
        );
    }

    pub fn balance_of(&self, account: impl Into<Account>) -> Nat {
        Decode!(
            &assert_reply(
                self.env
                    .query(
                        self.ledger_id,
                        "icrc1_balance_of",
                        Encode!(&account.into()).unwrap()
                    )
                    .expect("failed to query balance on the ledger")
            ),
            Nat
        )
        .unwrap()
    }

    pub fn withdrawal_account(&self, owner: PrincipalId) -> Account {
        Decode!(
            &assert_reply(
                self.env
                    .execute_ingress_as(
                        owner,
                        self.minter_id,
                        "get_withdrawal_account",
                        Encode!().unwrap()
                    )
                    .expect("failed to get ckbtc withdrawal account")
            ),
            Account
        )
        .unwrap()
    }

    pub fn transfer(&self, from: impl Into<Account>, to: impl Into<Account>, amount: u64) -> Nat {
        let from = from.into();
        let to = to.into();
        Decode!(&assert_reply(self.env.execute_ingress_as(
            PrincipalId::from(from.owner),
            self.ledger_id,
            "icrc1_transfer",
            Encode!(&TransferArg {
                from_subaccount: from.subaccount,
                to,
                fee: None,
                created_at_time: None,
                memo: None,
                amount: Nat::from(amount),
            }).unwrap()
            ).expect("failed to execute token transfer")),
            Result<Nat, TransferError>
        )
        .unwrap()
        .expect("token transfer failed")
    }

    pub fn retrieve_btc(
        &self,
        address: String,
        amount: u64,
    ) -> Result<RetrieveBtcOk, RetrieveBtcError> {
        Decode!(
            &assert_reply(
                self.env.execute_ingress_as(self.caller, self.minter_id, "retrieve_btc", Encode!(&RetrieveBtcArgs {
                    address,
                    amount,
                }).unwrap())
                .expect("failed to execute retrieve_btc request")
            ),
            Result<RetrieveBtcOk, RetrieveBtcError>
        ).unwrap()
    }

    pub fn retrieve_btc_status(&self, block_index: u64) -> RetrieveBtcStatus {
        Decode!(
            &assert_reply(
                self.env
                    .query(
                        self.minter_id,
                        "retrieve_btc_status",
                        Encode!(&RetrieveBtcStatusRequest { block_index }).unwrap()
                    )
                    .expect("failed to get ckbtc withdrawal account")
            ),
            RetrieveBtcStatus
        )
        .unwrap()
    }

    pub fn await_btc_transaction(&self, block_index: u64, max_ticks: usize) -> [u8; 32] {
        let mut last_status = None;
        for _ in 0..max_ticks {
            match self.retrieve_btc_status(block_index) {
                RetrieveBtcStatus::Submitted { txid } => {
                    return txid;
                }
                status => {
                    last_status = Some(status);
                    self.env.tick();
                }
            }
        }
        panic!(
            "the minter did not submit a transaction in {} ticks; last status {:?}",
            max_ticks, last_status
        )
    }

    pub fn await_finalization(&self, block_index: u64, max_ticks: usize) -> [u8; 32] {
        let mut last_status = None;
        for _ in 0..max_ticks {
            match self.retrieve_btc_status(block_index) {
                RetrieveBtcStatus::Confirmed { txid } => {
                    return txid;
                }
                status => {
                    last_status = Some(status);
                    self.env.tick();
                }
            }
        }
        panic!(
            "the minter did not finalize the transaction in {} ticks; last status: {:?}",
            max_ticks, last_status
        )
    }

    pub fn mempool(&self) -> Vec<Vec<u8>> {
        Decode!(
            &assert_reply(
                self.env
                    .execute_ingress(self.bitcoin_id, "get_mempool", Encode!().unwrap())
                    .expect("failed to call get_mempool on the bitcoin mock")
            ),
            Vec<Vec<u8>>
        )
        .unwrap()
    }
}

#[test]
fn test_transaction_finalization() {
    let ckbtc = CkBtcSetup::new();

    // Step 1: deposit ckBTC

    let deposit_value = 100_000_000;
    let utxo = Utxo {
        height: 0,
        outpoint: OutPoint {
            txid: (1..=32).collect::<Vec<u8>>(),
            vout: 1,
        },
        value: deposit_value,
    };

    let user = Principal::from(ckbtc.caller);

    ckbtc.deposit_utxo(user, utxo);

    assert_eq!(ckbtc.balance_of(user), Nat::from(deposit_value - KYT_FEE));

    // Step 2: request a withdrawal

    let withdrawal_amount = 50_000_000;
    let withdrawal_account = ckbtc.withdrawal_account(user.into());
    let fee_estimate = ckbtc.estimate_withdrawal_fee(Some(withdrawal_amount));
    ckbtc.transfer(user, withdrawal_account, withdrawal_amount);

    let RetrieveBtcOk { block_index } = ckbtc
        .retrieve_btc(
            "bc1q34aq5drpuwy3wgl9lhup9892qp6svr8ldzyy7c".to_string(),
            withdrawal_amount,
        )
        .expect("retrieve_btc failed");

    ckbtc.env.advance_time(MAX_TIME_IN_QUEUE);

    // Step 3: wait for the transaction to be submitted

    let txid = ckbtc.await_btc_transaction(block_index, 10);
    let mempool = ckbtc.mempool();
    assert_eq!(
        mempool.len(),
        1,
        "ckbtc transaction did not appear in the mempool"
    );
    let tx =
        bitcoin::Transaction::deserialize(&mempool[0]).expect("failed to decode ckbtc transaction");

    assert_eq!(txid, &*tx.txid());
    assert_eq!(2, tx.output.len());
    assert_eq!(
        tx.output[0].value,
        withdrawal_amount - fee_estimate.minter_fee - fee_estimate.bitcoin_fee
    );

    let change_utxo = &tx.output[1];
    let change_address =
        BtcAddress::from_script(&change_utxo.script_pubkey, BtcNetwork::Bitcoin).unwrap();

    let main_address = ckbtc.get_btc_address(Principal::from(ckbtc.minter_id));
    assert_eq!(change_address.to_string(), main_address);

    ckbtc
        .env
        .advance_time(MIN_CONFIRMATIONS * Duration::from_secs(600) + Duration::from_secs(1));

    // Step 4: confirm the transaction

    ckbtc.push_utxo(
        change_address.to_string(),
        Utxo {
            value: change_utxo.value,
            height: 0,
            outpoint: OutPoint {
                txid: txid.to_vec(),
                vout: 1,
            },
        },
    );

    assert_eq!(ckbtc.await_finalization(block_index, 10), txid);
}

#[test]
fn test_min_retrieval_amount() {
    let ckbtc = CkBtcSetup::new();

    ckbtc.refresh_fee_percentiles();
    let retrieve_btc_min_amount = ckbtc.get_minter_info().retrieve_btc_min_amount;
    assert_eq!(retrieve_btc_min_amount, 100_000);

    // The numbers used in this test have been re-computed using a python script using integers.
    ckbtc.set_fee_percentiles(&vec![0; 100]);
    ckbtc.refresh_fee_percentiles();
    let retrieve_btc_min_amount = ckbtc.get_minter_info().retrieve_btc_min_amount;
    assert_eq!(retrieve_btc_min_amount, 100_000);

    ckbtc.set_fee_percentiles(&vec![116_000; 100]);
    ckbtc.refresh_fee_percentiles();
    let retrieve_btc_min_amount = ckbtc.get_minter_info().retrieve_btc_min_amount;
    assert_eq!(retrieve_btc_min_amount, 150_000);

    ckbtc.set_fee_percentiles(&vec![342_000; 100]);
    ckbtc.refresh_fee_percentiles();
    let retrieve_btc_min_amount = ckbtc.get_minter_info().retrieve_btc_min_amount;
    assert_eq!(retrieve_btc_min_amount, 150_000);

    ckbtc.set_fee_percentiles(&vec![343_000; 100]);
    ckbtc.refresh_fee_percentiles();
    let retrieve_btc_min_amount = ckbtc.get_minter_info().retrieve_btc_min_amount;
    assert_eq!(retrieve_btc_min_amount, 200_000);
}
