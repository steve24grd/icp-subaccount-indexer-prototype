use candid::{CandidType, Deserialize, Principal};
use core::future::Future;
use ic_cdk::api::call::CallResult;
use ic_cdk_macros::*;
use ic_cdk_timers::TimerId;
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{hash_map::DefaultHasher, HashMap};
use std::hash::{Hash, Hasher};

mod memory;
mod tests;
mod types;

use ic_ledger_types::{
    AccountIdentifier, Subaccount,
};

use memory::{
    CUSTODIAN_PRINCIPAL, INTERVAL_IN_SECONDS, LAST_SUBACCOUNT_NONCE, NEXT_BLOCK, PRINCIPAL,
    TRANSACTIONS,
};
use types::{
    IcCdkSpawnManager, IcCdkSpawnManagerTrait, Icrc1TransferRequest, Icrc1TransferResponse,
    InterCanisterCallManager, InterCanisterCallManagerTrait, Operation, QueryBlocksRequest,
    QueryBlocksResponse, StoredPrincipal, StoredTransactions, SweepStatus, TimerManager,
    TimerManagerTrait, Timestamp, ToRecord,
};

thread_local! {
    static LIST_OF_SUBACCOUNTS: RefCell<HashMap<u64, Subaccount>> = RefCell::default();
    static TIMERS: RefCell<TimerId> = RefCell::default();
}

#[derive(Debug, CandidType, Deserialize, Serialize)]
struct Error {
    message: String,
}

trait ToU64Hash {
    fn to_u64_hash(&self) -> u64;
}

impl ToU64Hash for [u8; 32] {
    fn to_u64_hash(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

impl ToU64Hash for AccountIdentifier {
    fn to_u64_hash(&self) -> u64 {
        let bytes = from_hex(&self.to_hex()).unwrap();
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        hasher.finish()
    }
}

fn includes_hash(vec_to_check: &Vec<u8>) -> bool {
    match vec_to_check.len() {
        32 => {
            let slice = &vec_to_check[..];
            let array_ref: Option<&[u8; 32]> = slice.try_into().ok();

            match array_ref {
                Some(array_ref) => {
                    let data: [u8; 32] = *array_ref;
                    let hash_key = data.to_u64_hash();

                    LIST_OF_SUBACCOUNTS.with(|subaccounts| {
                        let subaccounts_borrow = subaccounts.borrow();

                        ic_cdk::println!("hash_key: {}", hash_key);
                        match subaccounts_borrow.get(&hash_key) {
                            Some(_) => true,
                            None => false,
                        }
                    })
                }
                None => false,
            }
        }
        other => {
            ic_cdk::println!("vec_to_check len: {}", other);
            false
        }
    }
}

#[update]
async fn set_next_block(block: u64) {
    NEXT_BLOCK.with(|next_block_ref| {
        let _ = next_block_ref.borrow_mut().set(block);
    });
}

#[query]
fn get_next_block() -> u64 {
    NEXT_BLOCK.with(|next_block_ref| *next_block_ref.borrow().get())
}

#[cfg(not(test))]
impl IcCdkSpawnManagerTrait for IcCdkSpawnManager {
    fn run<F: 'static + Future<Output = ()>>(future: F) {
        ic_cdk::spawn(future);
    }
}

#[cfg(not(test))]
impl InterCanisterCallManagerTrait for InterCanisterCallManager {
    async fn query_blocks(
        ledger_principal: Principal,
        req: QueryBlocksRequest,
    ) -> CallResult<(QueryBlocksResponse,)> {
        ic_cdk::call(ledger_principal, "query_blocks", (req,)).await
    }

    async fn icrc1_transfer(
        ledger_principal: Principal,
        req: Icrc1TransferRequest,
    ) -> CallResult<(Icrc1TransferResponse,)> {
        ic_cdk::call(ledger_principal, "icrc1_transfer", (req,)).await
    }
}

async fn call_query_blocks() {
    ic_cdk::println!("Calling query_blocks");
    let ledger_principal = PRINCIPAL.with(|stored_ref| stored_ref.borrow().get().clone());

    let next_block = NEXT_BLOCK.with(|next_block_ref| next_block_ref.borrow().get().clone());

    let ledger_principal = match ledger_principal.get_principal() {
        Some(result) => result,
        None => {
            ic_cdk::println!("Principal is not set");
            return;
        }
    };

    let req = QueryBlocksRequest {
        start: next_block,
        length: 100,
    };

    let call_result: CallResult<(QueryBlocksResponse,)> =
        InterCanisterCallManager::query_blocks(ledger_principal, req).await;

    let response = match call_result {
        Ok((response,)) => response,
        Err(_) => {
            ic_cdk::println!("query_blocks error occurred");
            return;
        }
    };

    ic_cdk::println!("Response: {:?}", response);

    let mut block_count = next_block;
    response.blocks.iter().for_each(|block| {
        block.transaction.operation.as_ref().map(|operation| {
            ic_cdk::println!("Operation: {:?}", operation);

            let subaccount_exist = match operation {
                Operation::Approve(data) => {
                    ic_cdk::println!("Approve detected");
                    let from = data.from.clone();
                    if includes_hash(&from) {
                        true
                    } else {
                        let spender = data.spender.clone();
                        includes_hash(&spender)
                    }
                }
                Operation::Burn(data) => {
                    ic_cdk::println!("Burn detected");
                    let from = data.from.clone();
                    if includes_hash(&from) {
                        true
                    } else {
                        match &data.spender {
                            Some(spender) => includes_hash(&spender),
                            None => false,
                        }
                    }
                }
                Operation::Mint(data) => {
                    ic_cdk::println!("Mint detected");
                    let to = data.to.clone();
                    includes_hash(&to)
                }
                Operation::Transfer(data) => {
                    ic_cdk::println!("Transfer detected");
                    let to = data.to.clone();
                    if includes_hash(&to) {
                        true
                    } else {
                        match &data.spender {
                            Some(spender) => includes_hash(&spender),
                            None => false,
                        }
                    }
                }
            };

            if subaccount_exist {
                ic_cdk::println!("Subaccount exists");
                TRANSACTIONS.with(|transactions_ref| {
                    let mut transactions = transactions_ref.borrow_mut();

                    let transaction =
                        StoredTransactions::new(block_count, block.transaction.clone());

                    if !transactions.contains_key(&block_count) {
                        // Filter keys that exist
                        ic_cdk::println!("Inserting transaction");
                        let _ = transactions.insert(block_count, transaction);
                    } else {
                        ic_cdk::println!("Transaction already exists");
                    }
                });
            }
        });
        block_count += 1;
    });

    let _ = NEXT_BLOCK.with(|next_block_ref| next_block_ref.borrow_mut().set(block_count));
}

async fn call_icrc1_transfer(ledger_principal: Principal, req: Icrc1TransferRequest) {
    ic_cdk::println!("Calling icrc1_transfer");

    let call_result: CallResult<(Icrc1TransferResponse,)> =
        InterCanisterCallManager::icrc1_transfer(ledger_principal, req.clone()).await;

    let response = match call_result {
        Ok((response,)) => response,
        Err(_) => {
            icrc1_transfer_error_handling(req);
            return;
        }
    };

    ic_cdk::println!("Response: {:?}", response);
}

fn vec_u8_to_u64(bytes: Vec<u8>) -> u64 {
    let bytes_array = <[u8; 8]>::try_from(bytes).expect("Slice with incorrect length");
    u64::from_be_bytes(bytes_array)
}

fn icrc1_transfer_error_handling(req: Icrc1TransferRequest) {
    ic_cdk::println!("An error occurred");
    let memo = match req.memo {
        Some(memo) => memo,
        None => {
            return;
        }
    };

    let _ = TRANSACTIONS.with(|transactions_ref| {
        let key = vec_u8_to_u64(memo);
        let mut transaction = match { transactions_ref.borrow().get(&key).clone() } {
            Some(transaction) => transaction,
            None => {
                return;
            }
        };

        transaction.sweep_status = SweepStatus::FailedToSweep;

        let mut transaction_borrow_mut = transactions_ref.borrow_mut();
        transaction_borrow_mut.remove(&key);
        transaction_borrow_mut.insert(key.clone(), transaction.clone());
    });
}

#[cfg(not(test))]
impl TimerManagerTrait for TimerManager {
    fn set_timer(interval: std::time::Duration) -> TimerId {
        ic_cdk::println!("Starting a periodic task with interval {:?}", interval);
        ic_cdk_timers::set_timer_interval(interval, || {
            IcCdkSpawnManager::run(call_query_blocks());
        })
    }

    fn clear_timer(timer_id: TimerId) {
        ic_cdk_timers::clear_timer(timer_id);
    }
}

#[ic_cdk::init]
async fn init(seconds: u64, nonce: u32, ledger_principal: String, custodian_principal: String) {
    INTERVAL_IN_SECONDS.with(|interval_ref| {
        let _ = interval_ref.borrow_mut().set(seconds);
    });

    LAST_SUBACCOUNT_NONCE.with(|nonce_ref| {
        let _ = nonce_ref.borrow_mut().set(nonce);
    });

    let principal = Principal::from_text(&ledger_principal).expect("Invalid ledger principal");

    PRINCIPAL.with(|principal_ref| {
        let stored_principal = StoredPrincipal::new(principal);
        let _ = principal_ref.borrow_mut().set(stored_principal);
    });

    let custodian_principal =
        Principal::from_text(&custodian_principal).expect("Invalid custodian principal");

    CUSTODIAN_PRINCIPAL.with(|principal_ref| {
        let stored_principal = StoredPrincipal::new(custodian_principal);
        let _ = principal_ref.borrow_mut().set(stored_principal);
    });

    let interval = std::time::Duration::from_secs(seconds);
    let timer_id = TimerManager::set_timer(interval);

    TIMERS.with(|timers_ref| {
        timers_ref.replace(timer_id);
    });

    reconstruct_subaccounts();
}

fn reconstruct_subaccounts() {
    let nonce: u32 = get_nonce();
    let account = CUSTODIAN_PRINCIPAL
        .with(|stored_ref| stored_ref.borrow().get().clone())
        .get_principal()
        .expect("Custodian principal is not set");

    ic_cdk::println!("Reconstructing subaccounts for account: {:?}", account);
    for i in 0..nonce {
        ic_cdk::println!("nonce: {}", i);
        let subaccount = to_subaccount(i);
        let subaccountid: AccountIdentifier = to_subaccount_id(subaccount.clone()); 
        let account_id_hash = subaccountid.to_u64_hash();
        LIST_OF_SUBACCOUNTS.with(|list_ref| {
            list_ref.borrow_mut().insert(account_id_hash, subaccount);
        });
    }
}

#[ic_cdk::post_upgrade]
async fn post_upgrade() {
    ic_cdk::println!("running post_upgrade...");
    reconstruct_subaccounts();
}

#[query]
fn get_interval() -> Result<u64, Error> {
    INTERVAL_IN_SECONDS.with(|interval_ref| Ok(*interval_ref.borrow().get()))
}

#[update]
fn set_interval(seconds: u64) -> Result<u64, Error> {
    TIMERS.with(|timers_ref| {
        TimerManager::clear_timer(timers_ref.borrow().clone());
    });

    let interval = std::time::Duration::from_secs(seconds);
    let new_timer_id = TimerManager::set_timer(interval);

    TIMERS.with(|timers_ref| {
        timers_ref.replace(new_timer_id);
    });

    INTERVAL_IN_SECONDS.with(|seconds_ref| {
        let _ = seconds_ref.borrow_mut().set(seconds);
    });

    Ok(seconds)
}

#[query]
fn get_nonce() -> u32 {
    LAST_SUBACCOUNT_NONCE.with(|nonce_ref| *nonce_ref.borrow().get())
}

fn to_subaccount(nonce: u32) -> Subaccount {
    let mut subaccount = Subaccount([0; 32]);
    let nonce_bytes = nonce.to_be_bytes(); // Converts u32 to an array of 4 bytes
    subaccount.0[32 - nonce_bytes.len()..].copy_from_slice(&nonce_bytes); // Aligns the bytes at the end of the array
    subaccount
}

fn to_subaccount_id(subaccount: Subaccount) -> AccountIdentifier {
    let account = CUSTODIAN_PRINCIPAL
        .with(|stored_ref| stored_ref.borrow().get().clone())
        .get_principal()
        .expect("Custodian principal is not set");
    AccountIdentifier::new(&account, &subaccount)
}

fn from_hex(hex: &str) -> Result<[u8; 32], Error> {
    
    let vec = hex::decode(hex).map_err(|_| Error {
        message: "string to vector conversion error".to_string(),
    })?;

    let arr = vec.as_slice().try_into().map_err(|_| Error {
        message: "vector to fix array conversion error".to_string(),
    })?;

    Ok(arr)
}

#[update]
fn add_subaccount() -> String {
    let nonce = get_nonce();
    let subaccount = to_subaccount(nonce); // needed for storing the subaccount
    let subaccountid: AccountIdentifier = to_subaccount_id(subaccount.clone()); // needed to get the hashkey & to return to user
    let account_id_hash = subaccountid.to_u64_hash();

    LIST_OF_SUBACCOUNTS.with(|list_ref| {
        list_ref.borrow_mut().insert(account_id_hash, subaccount);
    });

    LAST_SUBACCOUNT_NONCE.with(|nonce_ref| {
        let _ = nonce_ref.borrow_mut().set(nonce + 1);
    });

    subaccountid.to_hex()
}

#[query]
fn get_subaccountid(nonce: u32) -> Result<String, Error> {
    LIST_OF_SUBACCOUNTS.with(|subaccounts| {
        let subaccounts_borrow = subaccounts.borrow();

        if nonce as usize >= subaccounts_borrow.len() {
            return Err(Error {
                message: "Index out of bounds".to_string(),
            });
        }
       
        let subaccount = to_subaccount(nonce);
        let subaccountid: AccountIdentifier = to_subaccount_id(subaccount.clone()); 
        let account_id_hash = subaccountid.to_u64_hash();

        ic_cdk::println!("account_id_hash to search: {}", account_id_hash);

        // find matching hashkey
        match subaccounts_borrow.get(&account_id_hash) {
            Some(_) => Ok(subaccountid.to_hex()),
            None => Err(Error {
                message: "Account not found".to_string(),
            }),
        }
    })
}

#[query]
fn get_subaccount_count() -> u32 {
    LIST_OF_SUBACCOUNTS.with(|subaccounts| subaccounts.borrow().len() as u32)
}

#[query]
fn get_transactions_count() -> u32 {
    TRANSACTIONS.with(|transactions_ref| transactions_ref.borrow().len() as u32)
}

#[query]
fn get_oldest_block() -> Option<u64> {
    TRANSACTIONS.with(|transactions_ref| {
        let transactions_borrow = transactions_ref.borrow();
        transactions_borrow.iter().next().map(|(key, _value)| key)
    })
}

#[query]
fn list_transactions(up_to_count: Option<u64>) -> Vec<StoredTransactions> {
    // process argument
    let up_to_count = match up_to_count {
        Some(count) => count,
        None => 100, // Default is 100
    };

    // get earliest block
    // if there are no transactions, return empty `result`
    let mut result = Vec::new();

    TRANSACTIONS.with(|transactions_ref| {
        let transactions_borrow = transactions_ref.borrow();

        ic_cdk::println!("transactions_len: {}", transactions_borrow.len());

        // If transactions_borrow.len() is less than up_to_count, return all transactions
        let skip = if transactions_borrow.len() < up_to_count {
            0
        } else {
            transactions_borrow.len() - up_to_count
        };

        ic_cdk::println!("skip: {}", skip);
        transactions_borrow
            .iter()
            .skip(skip as usize)
            .take(up_to_count as usize)
            .for_each(|(_key, value)| {
                result.push(value.clone());
            });
        result
    })
}

#[update]
fn clear_transactions(
    up_to_index: Option<u64>,
    up_to_timestamp: Option<Timestamp>,
) -> Result<Vec<StoredTransactions>, Error> {
    // Get Data
    let up_to_index = match up_to_index {
        Some(index) => index,
        None => 0,
    };
    let up_to_timestamp = match up_to_timestamp {
        Some(timestamp) => timestamp,
        None => Timestamp::from_nanos(0),
    };

    TRANSACTIONS.with(|transactions_ref| {
        // Collect keys that are less than the cutoff
        let mut transactions_borrow = transactions_ref.borrow_mut();
        let keys_to_remove: Vec<u64> = transactions_borrow
            .iter()
            .filter(|transaction| {
                // If up_to_index is set then remove transactions with a index less than up_to_index
                // If up_to_timestamp is set then remove transactions with a timestamp less than up_to_timestamp
                (up_to_index != 0 && transaction.1.index <= up_to_index)
                    || (up_to_timestamp.timestamp_nanos != 0
                        && transaction.1.created_at_time.timestamp_nanos
                            <= up_to_timestamp.timestamp_nanos)
            })
            .map(|(k, _)| k)
            .collect();

        // Remove elements with those keys
        for key in keys_to_remove {
            transactions_borrow.remove(&key);
        }

        let mut result = Vec::new();
        transactions_borrow.iter().for_each(|(_key, value)| {
            result.push(value.clone());
        });
        Ok(result)
    })
}

#[update]
fn refund(transaction_index: u64) -> Result<String, Error> {
    let ledger_principal_opt = PRINCIPAL.with(|stored_ref| stored_ref.borrow().get().clone());

    let ledger_principal = match ledger_principal_opt.get_principal() {
        Some(result) => result,
        None => {
            return Err(Error {
                message: "Principal is not set".to_string(),
            });
        }
    };

    let transaction_opt = TRANSACTIONS
        .with(|transactions_ref| transactions_ref.borrow().get(&transaction_index).clone());

    let transaction = match transaction_opt {
        Some(value) => value,
        None => {
            return Err(Error {
                message: "Transaction index is not found".to_string(),
            });
        }
    };

    let subaccount = match transaction.operation {
        Some(Operation::Transfer(data)) => {
            let to = data.to.clone();
            match &data.spender {
                Some(spender) => (includes_hash(&to), to, spender.clone(), data.amount.e8s),
                None => (false, to, vec![], data.amount.e8s),
            }
        }
        _ => (false, vec![], vec![], 0),
    };

    if !subaccount.0 {
        return Err(Error {
            message: "Cannot confirm receiver and spender".to_string(),
        });
    }

    let to_record = ToRecord::new(Principal::from_slice(&subaccount.2), None);
    let req = Icrc1TransferRequest::new(to_record, None, None, Some(subaccount.1), None, 1000);

    IcCdkSpawnManager::run(call_icrc1_transfer(ledger_principal, req));

    Ok("Refund is being requested".to_string())
}

#[update]
fn sweep_user_vault() -> Result<String, Error> {
    let ledger_principal_opt = PRINCIPAL.with(|stored_ref| stored_ref.borrow().get().clone());

    let ledger_principal = match ledger_principal_opt.get_principal() {
        Some(result) => result,
        None => {
            return Err(Error {
                message: "Principal is not set".to_string(),
            });
        }
    };

    let custodian_principal_opt =
        CUSTODIAN_PRINCIPAL.with(|stored_ref| stored_ref.borrow().get().clone());

    let custodian_principal = match custodian_principal_opt.get_principal() {
        Some(result) => result,
        None => {
            return Err(Error {
                message: "Custodian principal is not set".to_string(),
            });
        }
    };

    let _ = TRANSACTIONS.with(|transactions_ref| {
        let mut transactions: Vec<(u64, StoredTransactions)> = {
            transactions_ref
                .borrow()
                .iter()
                .filter(|transaction| transaction.1.sweep_status == SweepStatus::NotSwept)
                .map(|(key, transaction)| (key.clone(), transaction.clone()))
                .collect()
        };

        let mut transaction_borrow_mut = transactions_ref.borrow_mut();

        transactions.iter_mut().for_each(|(key, transaction)| {
            let subaccount = match transaction.clone().operation {
                Some(Operation::Transfer(data)) => {
                    let to = data.to.clone();
                    (includes_hash(&to), to, data.amount.e8s)
                }
                _ => (false, vec![], 0),
            };

            if subaccount.0 {
                let to_record = ToRecord::new(custodian_principal, None);
                let req = Icrc1TransferRequest::new(
                    to_record,
                    None,
                    Some(transaction.index.to_be_bytes().to_vec()),
                    Some(subaccount.1),
                    None,
                    subaccount.2,
                );

                IcCdkSpawnManager::run(call_icrc1_transfer(ledger_principal, req));

                transaction.sweep_status = SweepStatus::Swept;

                transaction_borrow_mut.remove(&key);
                transaction_borrow_mut.insert(key.clone(), transaction.clone());
            }
        });
    });

    Ok("Subaccounts are swept to vault".to_string())
}

#[query]
fn canister_status() -> Result<String, Error> {
    // Stub implementation - Return a placeholder JSON response
    Ok(format!("{{\"message\": \"Canister is operational\"}}"))
}

// Enable Candid export
ic_cdk::export_candid!();
