use super::{get_btc_address::init_ecdsa_public_key, get_withdrawal_account::compute_subaccount};
use crate::logs::P0;
use crate::logs::P1;
use crate::management::fetch_withdrawal_alerts;
use crate::memo::{BurnMemo, Status};
use crate::tasks::{schedule_now, TaskType};
use crate::{
    address::{account_to_bitcoin_address, BitcoinAddress, ParseAddressError},
    guard::{retrieve_btc_guard, GuardError},
    state::{self, mutate_state, read_state, RetrieveBtcRequest},
};
use candid::{CandidType, Deserialize, Nat, Principal};
use ic_base_types::PrincipalId;
use ic_canister_log::log;
use ic_ckbtc_kyt::Error as KytError;
use ic_icrc1_client_cdk::{CdkRuntime, ICRC1Client};
use icrc_ledger_types::icrc1::account::Account;
use icrc_ledger_types::icrc1::transfer::Memo;
use icrc_ledger_types::icrc1::transfer::{TransferArg, TransferError};
use num_traits::cast::ToPrimitive;

const MAX_CONCURRENT_PENDING_REQUESTS: usize = 1000;

/// The arguments of the [retrieve_btc] endpoint.
#[derive(CandidType, Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RetrieveBtcArgs {
    // amount to retrieve in satoshi
    pub amount: u64,

    // address where to send bitcoins
    pub address: String,
}

#[derive(CandidType, Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RetrieveBtcOk {
    // the index of the burn block on the ckbtc ledger
    pub block_index: u64,
}

pub enum ErrorCode {
    // The retrieval address didn't pass the KYT check.
    TaintedAddress = 1,
}

#[derive(CandidType, Clone, Debug, Deserialize, PartialEq, Eq)]
pub enum RetrieveBtcError {
    /// There is another request for this principal.
    AlreadyProcessing,

    /// The withdrawal amount is too low.
    AmountTooLow(u64),

    /// The bitcoin address is not valid.
    MalformedAddress(String),

    /// The withdrawal account does not hold the requested ckBTC amount.
    InsufficientFunds { balance: u64 },

    /// There are too many concurrent requests, retry later.
    TemporarilyUnavailable(String),

    /// A generic error reserved for future extensions.
    GenericError {
        error_message: String,
        /// See the [ErrorCode] enum above for the list of possible values.
        error_code: u64,
    },
}

impl From<GuardError> for RetrieveBtcError {
    fn from(e: GuardError) -> Self {
        match e {
            GuardError::AlreadyProcessing => Self::AlreadyProcessing,
            GuardError::TooManyConcurrentRequests => {
                Self::TemporarilyUnavailable("too many concurrent requests".to_string())
            }
        }
    }
}

impl From<ParseAddressError> for RetrieveBtcError {
    fn from(e: ParseAddressError) -> Self {
        Self::MalformedAddress(e.to_string())
    }
}

pub async fn retrieve_btc(args: RetrieveBtcArgs) -> Result<RetrieveBtcOk, RetrieveBtcError> {
    let caller = ic_cdk::caller();

    state::read_state(|s| s.mode.is_withdrawal_available_for(&caller))
        .map_err(RetrieveBtcError::TemporarilyUnavailable)?;

    if crate::blocklist::BTC_ADDRESS_BLOCKLIST
        .binary_search(&args.address.trim())
        .is_ok()
    {
        ic_cdk::trap("attempted to retrieve BTC to a blocked address");
    }

    init_ecdsa_public_key().await;

    let main_account = Account {
        owner: ic_cdk::id(),
        subaccount: None,
    };

    let main_address = match state::read_state(|s| {
        s.ecdsa_public_key
            .clone()
            .map(|key| account_to_bitcoin_address(&key, &main_account))
    }) {
        Some(address) => address,
        None => {
            ic_cdk::trap(
                "unreachable: have retrieve BTC requests but the ECDSA key is not initialized",
            );
        }
    };

    if args.address == main_address.display(state::read_state(|s| s.btc_network)) {
        ic_cdk::trap("illegal retrieve_btc target");
    }

    let _guard = retrieve_btc_guard(caller)?;
    let (min_amount, btc_network) = read_state(|s| (s.retrieve_btc_min_amount, s.btc_network));
    if args.amount < min_amount {
        return Err(RetrieveBtcError::AmountTooLow(min_amount));
    }
    let parsed_address = BitcoinAddress::parse(&args.address, btc_network)?;
    if read_state(|s| s.count_incomplete_retrieve_btc_requests() >= MAX_CONCURRENT_PENDING_REQUESTS)
    {
        return Err(RetrieveBtcError::TemporarilyUnavailable(
            "too many pending retrieve_btc requests".to_string(),
        ));
    }

    let balance = balance_of(caller).await?;
    if args.amount > balance {
        return Err(RetrieveBtcError::InsufficientFunds { balance });
    }

    let (uuid, status, kyt_provider) =
        kyt_check_address(caller, args.address.clone(), args.amount).await?;

    let kyt_fee = read_state(|s| s.kyt_fee);

    match status {
        BtcAddressCheckStatus::Tainted => {
            let burn_memo = BurnMemo::Convert {
                address: Some(&args.address),
                kyt_fee: Some(kyt_fee),
                status: Some(Status::Rejected),
            };
            let block_index =
                burn_ckbtcs(caller, kyt_fee, crate::memo::encode(&burn_memo).into()).await?;
            log!(
                P1,
                "rejected an attempt to withdraw {} BTC to address {} due to failed KYT check (burnt {} ckBTC in block {})",
                crate::tx::DisplayAmount(args.amount),
                args.address,
                crate::tx::DisplayAmount(kyt_fee),
                block_index
            );
            mutate_state(|s| {
                state::audit::retrieve_btc_kyt_failed(
                    s,
                    caller,
                    args.address,
                    args.amount,
                    kyt_provider,
                    uuid,
                    block_index,
                )
            });
            return Err(RetrieveBtcError::GenericError {
                error_message: format!(
                    "Destination address is tainted, KYT check fee deducted: {}",
                    crate::tx::DisplayAmount(kyt_fee),
                ),
                error_code: ErrorCode::TaintedAddress as u64,
            });
        }
        BtcAddressCheckStatus::Clean => {}
    }
    let burn_memo = BurnMemo::Convert {
        address: Some(&args.address),
        kyt_fee: Some(kyt_fee),
        status: Some(Status::Accepted),
    };
    let block_index =
        burn_ckbtcs(caller, args.amount, crate::memo::encode(&burn_memo).into()).await?;
    let request = RetrieveBtcRequest {
        // NB. We charge the KYT fee from the retrieve amount.
        amount: args.amount - kyt_fee,
        address: parsed_address,
        block_index,
        received_at: ic_cdk::api::time(),
        kyt_provider: Some(kyt_provider),
    };

    log!(
        P1,
        "accepted a retrieve btc request for {} BTC to address {} (block_index = {})",
        crate::tx::DisplayAmount(request.amount),
        args.address,
        request.block_index
    );

    mutate_state(|s| state::audit::accept_retrieve_btc_request(s, request));

    assert_eq!(
        crate::state::RetrieveBtcStatus::Pending,
        read_state(|s| s.retrieve_btc_status(block_index))
    );

    schedule_now(TaskType::ProcessLogic);

    Ok(RetrieveBtcOk { block_index })
}

async fn balance_of(user: Principal) -> Result<u64, RetrieveBtcError> {
    let client = ICRC1Client {
        runtime: CdkRuntime,
        ledger_canister_id: read_state(|s| s.ledger_id.get().into()),
    };
    let minter = ic_cdk::id();
    let subaccount = compute_subaccount(PrincipalId(user), 0);
    let result = client
        .balance_of(Account {
            owner: minter,
            subaccount: Some(subaccount),
        })
        .await
        .map_err(|(code, msg)| {
            RetrieveBtcError::TemporarilyUnavailable(format!(
                "cannot enqueue a balance_of request: {} (reject_code = {})",
                msg, code
            ))
        })?;
    Ok(result)
}

async fn burn_ckbtcs(user: Principal, amount: u64, memo: Memo) -> Result<u64, RetrieveBtcError> {
    debug_assert!(memo.0.len() <= crate::CKBTC_LEDGER_MEMO_SIZE as usize);

    let client = ICRC1Client {
        runtime: CdkRuntime,
        ledger_canister_id: read_state(|s| s.ledger_id.get().into()),
    };
    let minter = ic_cdk::id();
    let from_subaccount = compute_subaccount(PrincipalId(user), 0);
    let result = client
        .transfer(TransferArg {
            from_subaccount: Some(from_subaccount),
            to: Account {
                owner: minter,
                subaccount: None,
            },
            fee: None,
            created_at_time: None,
            memo: Some(memo),
            amount: Nat::from(amount),
        })
        .await
        .map_err(|(code, msg)| {
            RetrieveBtcError::TemporarilyUnavailable(format!(
                "cannot enqueue a burn transaction: {} (reject_code = {})",
                msg, code
            ))
        })?;

    match result {
        Ok(block_index) => Ok(block_index),
        Err(TransferError::InsufficientFunds { balance }) => Err(RetrieveBtcError::InsufficientFunds {
            balance: balance.0.to_u64().expect("unreachable: ledger balance does not fit into u64")
        }),
        Err(TransferError::TemporarilyUnavailable) => {
            Err(RetrieveBtcError::TemporarilyUnavailable(
                "cannot burn ckBTC: the ledger is busy".to_string(),
            ))
        }
        Err(TransferError::GenericError { error_code, message }) => {
            Err(RetrieveBtcError::TemporarilyUnavailable(format!(
                "cannot burn ckBTC: the ledger fails with: {} (error code {})", message, error_code
            )))
        }
        Err(TransferError::BadFee { expected_fee }) => ic_cdk::trap(&format!(
            "unreachable: the ledger demands the fee of {} even though the fee field is unset",
            expected_fee
        )),
        Err(TransferError::Duplicate{ duplicate_of }) => ic_cdk::trap(&format!(
            "unreachable: the ledger reports duplicate ({}) even though the create_at_time field is unset",
            duplicate_of
        )),
        Err(TransferError::CreatedInFuture{..}) => ic_cdk::trap(
            "unreachable: the ledger reports CreatedInFuture even though the create_at_time field is unset"
        ),
        Err(TransferError::TooOld) => ic_cdk::trap(
            "unreachable: the ledger reports TooOld even though the create_at_time field is unset"
        ),
        Err(TransferError::BadBurn { min_burn_amount }) => ic_cdk::trap(&format!(
            "the minter is misconfigured: retrieve_btc_min_amount {} is less than ledger's min_burn_amount {}",
            read_state(|s| s.retrieve_btc_min_amount),
            min_burn_amount
        )),
    }
}

/// The outcome of an address KYT check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum BtcAddressCheckStatus {
    /// The KYT check did not find any issues with the address.
    Clean,
    /// The KYT check found issues with the address in question.
    Tainted,
}

async fn kyt_check_address(
    caller: Principal,
    address: String,
    amount: u64,
) -> Result<(String, BtcAddressCheckStatus, Principal), RetrieveBtcError> {
    let kyt_principal = read_state(|s| {
        s.kyt_principal
            .expect("BUG: upgrade procedure must ensure that the KYT principal is set")
            .get()
            .into()
    });

    match fetch_withdrawal_alerts(kyt_principal, caller, address.clone(), amount)
        .await
        .map_err(|call_err| {
            RetrieveBtcError::TemporarilyUnavailable(format!(
                "Failed to call KYT canister: {}",
                call_err
            ))
        })? {
        Ok(response) => {
            if !response.alerts.is_empty() {
                log!(
                    P0,
                    "Discovered a tainted btc address {} (external id {})",
                    address,
                    response.external_id
                );
                Ok((
                    response.external_id,
                    BtcAddressCheckStatus::Tainted,
                    response.provider,
                ))
            } else {
                Ok((
                    response.external_id,
                    BtcAddressCheckStatus::Clean,
                    response.provider,
                ))
            }
        }
        Err(KytError::TemporarilyUnavailable(reason)) => {
            log!(
                P1,
                "The KYT provider is temporarily unavailable: {}",
                reason
            );
            Err(RetrieveBtcError::TemporarilyUnavailable(format!(
                "The KYT provider is temporarily unavailable: {}",
                reason
            )))
        }
    }
}
