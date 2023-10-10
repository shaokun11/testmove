use std::{collections::HashMap, fs, io::{self, Error, ErrorKind}, sync::Arc};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use avalanche_types::{
    choices, ids,
    subnet::{self, rpc::snow},
};
use avalanche_types::subnet::rpc::database::manager::{DatabaseManager, Manager};
use avalanche_types::subnet::rpc::health::Checkable;
use avalanche_types::subnet::rpc::snow::engine::common::appsender::AppSender;
use avalanche_types::subnet::rpc::snow::engine::common::appsender::client::AppSenderClient;
use avalanche_types::subnet::rpc::snow::engine::common::engine::{AppHandler, CrossChainAppHandler, NetworkAppHandler};
use avalanche_types::subnet::rpc::snow::engine::common::http_handler::{HttpHandler, LockOptions};
use avalanche_types::subnet::rpc::snow::engine::common::message::Message::PendingTxs;
use avalanche_types::subnet::rpc::snow::engine::common::vm::{CommonVm, Connector};
use avalanche_types::subnet::rpc::snow::validators::client::ValidatorStateClient;
use avalanche_types::subnet::rpc::snowman::block::{BatchedChainVm, ChainVm, Getter, Parser};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{channel::mpsc as futures_mpsc, StreamExt};
use hex;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc::Sender, RwLock};

use aptos_api::{Context, get_raw_api_service, RawApi};
use aptos_api::accept_type::AcceptType;
use aptos_api::response::{AptosResponseContent, BasicResponse};
use aptos_api::transactions::{SubmitTransactionPost, SubmitTransactionResponse, SubmitTransactionsBatchPost, SubmitTransactionsBatchResponse};
use aptos_api_types::{Address, EncodeSubmissionRequest, IdentifierWrapper, MoveStructTag, RawTableItemRequest, StateKeyWrapper, TableItemRequest, ViewRequest};
use aptos_config::config::NodeConfig;
use aptos_crypto::{HashValue, ValidCryptoMaterialStringExt};
use aptos_crypto::ed25519::Ed25519PublicKey;
use aptos_db::AptosDB;
use aptos_executor::block_executor::BlockExecutor;
use aptos_executor::db_bootstrapper::{generate_waypoint, maybe_bootstrap};
use aptos_executor_types::BlockExecutorTrait;
use aptos_mempool::{MempoolClientRequest, MempoolClientSender, SubmissionStatus};
use aptos_mempool::core_mempool::{CoreMempool, TimelineState};
use aptos_sdk::rest_client::aptos_api_types::MAX_RECURSIVE_TYPES_ALLOWED;
use aptos_sdk::transaction_builder::TransactionFactory;
use aptos_sdk::types::{AccountKey, LocalAccount};
use aptos_state_view::account_with_state_view::AsAccountWithStateView;
use aptos_storage_interface::DbReaderWriter;
use aptos_storage_interface::state_view::DbStateViewAtVersion;
use aptos_types::account_address::AccountAddress;
use aptos_types::account_config::aptos_test_root_address;
use aptos_types::account_view::AccountView;
use aptos_types::block_info::BlockInfo;
use aptos_types::block_metadata::BlockMetadata;
use aptos_types::chain_id::ChainId;
use aptos_types::ledger_info::{generate_ledger_info_with_sig, LedgerInfo};
use aptos_types::mempool_status::{MempoolStatus, MempoolStatusCode};
use aptos_types::transaction::{SignedTransaction, Transaction, WriteSetPayload};
use aptos_types::transaction::Transaction::UserTransaction;
use aptos_types::validator_signer::ValidatorSigner;
use aptos_vm::AptosVM;
use aptos_vm_genesis::{GENESIS_KEYPAIR, test_genesis_change_set_and_validators};

use crate::{block::Block, state};
use crate::api::chain_handlers::{AccountStateArgs, BlockArgs, ChainHandler, ChainService, GetTransactionByVersionArgs, PageArgs, RpcEventHandleReq, RpcEventNumReq, RpcReq, RpcRes, RpcTableReq};
use crate::api::static_handlers::{StaticHandler, StaticService};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MOVE_DB_DIR: &str = ".move-chain-data";

#[derive(Serialize, Deserialize, Clone)]
pub struct AptosData(
    pub Vec<u8>, // block info
    pub HashValue, // block id
    pub HashValue,
    pub u64,
    pub u64,
);

#[derive(Serialize, Deserialize, Clone)]
pub struct AptosHeader {
    chain_id: u8,
    ledger_version: u64,
    ledger_oldest_version: u64,
    ledger_timestamp_usec: u64,
    epoch: u64,
    block_height: u64,
    oldest_block_height: u64,
    cursor: Option<String>,
}


/// Represents VM-specific states.
/// Defined in a separate struct, for interior mutability in [`Vm`](Vm).
/// To be protected with `Arc` and `RwLock`.
pub struct VmState {
    pub ctx: Option<subnet::rpc::context::Context<ValidatorStateClient>>,

    /// Represents persistent Vm state.
    pub state: Option<state::State>,
    /// Currently preferred block Id.
    pub preferred: ids::Id,

    /// Set "true" to indicate that the Vm has finished bootstrapping
    /// for the chain.
    pub bootstrapped: bool,
}

impl Default for VmState {
    fn default() -> Self {
        Self {
            ctx: None,
            state: None,
            preferred: ids::Id::empty(),
            bootstrapped: false,
        }
    }
}

/// Implements [`snowman.block.ChainVM`](https://pkg.go.dev/github.com/ava-labs/avalanchego/snow/engine/snowman/block#ChainVM) interface.
#[derive(Clone)]
pub struct Vm {
    pub state: Arc<RwLock<VmState>>,

    pub app_sender: Option<AppSenderClient>,

    pub api_service: Option<RawApi>,

    pub api_context: Option<Context>,

    pub core_mempool: Option<Arc<RwLock<CoreMempool>>>,

    /// Channel to send messages to the snowman consensus engine.
    pub to_engine: Option<Arc<RwLock<Sender<snow::engine::common::message::Message>>>>,

    pub db: Option<Arc<RwLock<DbReaderWriter>>>,

    pub signer: Option<ValidatorSigner>,

    pub executor: Option<Arc<RwLock<BlockExecutor<AptosVM, Transaction>>>>,

    pub build_status: Arc<RwLock<u8>>,
    // 0 done 1 building
    pub has_pending_tx: Arc<RwLock<bool>>,

}


impl Default for Vm

{
    fn default() -> Self {
        Self::new()
    }
}

impl Vm {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(VmState::default())),
            app_sender: None,
            api_service: None,
            api_context: None,
            core_mempool: None,
            to_engine: None,
            signer: None,
            executor: None,
            db: None,
            build_status: Arc::new(RwLock::new(0)),
            has_pending_tx: Arc::new(RwLock::new(false)),
        }
    }

    pub async fn is_bootstrapped(&self) -> bool {
        let vm_state = self.state.read().await;
        vm_state.bootstrapped
    }

    pub async fn get_transactions(&self, args: PageArgs) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let api = self.api_service.as_ref().unwrap();
        let ret = api.0.get_transactions_raw(accept, args.start, args.limit).await;
        let ret = ret.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn get_block_by_height(&self, args: BlockArgs) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let api = self.api_service.as_ref().unwrap();
        let ret = api.5.get_block_by_height_raw(accept, args.height_or_version, args.with_transactions).await;
        let ret = ret.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn get_block_by_version(&self, args: BlockArgs) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let api = self.api_service.as_ref().unwrap();
        let ret = api.5.get_block_by_version_raw(accept, args.height_or_version, args.with_transactions).await;
        let ret = ret.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn get_accounts_transactions(&self, args: RpcReq) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let account = args.data.as_str();
        let api = self.api_service.as_ref().unwrap();
        let start = match args.start {
            None => None,
            Some(_) => Some(StateKeyWrapper::from_str(args.start.unwrap().as_str()).unwrap())
        };
        let ret = api.3.get_account_resources_raw(
            accept,
            Address::from_str(account).unwrap(),
            args.ledger_version,
            start,
            args.limit,
        ).await.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn get_account_resources(&self, args: RpcReq) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let account = args.data.as_str();
        let api = self.api_service.as_ref().unwrap();
        let start = match args.start {
            None => None,
            Some(_) => Some(StateKeyWrapper::from_str(args.start.unwrap().as_str()).unwrap())
        };
        let ret = api.3.get_account_resources_raw(
            accept,
            Address::from_str(account).unwrap(),
            args.ledger_version,
            start,
            args.limit,
        ).await.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn get_account(&self, args: RpcReq) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let account = args.data.as_str();
        let api = self.api_service.as_ref().unwrap();
        let ret = api.3.get_account_raw(accept,
                                        Address::from_str(account).unwrap(), args.ledger_version).await.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }
    pub async fn get_account_modules_state(&self, args: AccountStateArgs) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let account = args.account.as_str();
        let module_name = args.resource.as_str();
        let module_name = IdentifierWrapper::from_str(module_name).unwrap();
        let api = self.api_service.as_ref().unwrap();
        let ret = api.4.get_account_module_raw(
            accept,
            Address::from_str(account).unwrap(),
            module_name, args.ledger_version).await.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn get_account_resources_state(&self, args: AccountStateArgs) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let account = args.account.as_str();
        let resource = args.resource.as_str();
        let api = self.api_service.as_ref().unwrap();
        let ret = api.4.get_account_resource_raw(accept,
                                                 Address::from_str(account).unwrap(),
                                                 MoveStructTag::from_str(resource).unwrap(),
                                                 args.ledger_version).await.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn get_account_modules(&self, args: RpcReq) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let account = args.data.as_str();
        let start = match args.start {
            None => None,
            Some(_) => Some(StateKeyWrapper::from_str(args.start.unwrap().as_str()).unwrap())
        };
        let api = self.api_service.as_ref().unwrap();
        let address = Address::from_str(account).unwrap();
        let ret = api.3.get_account_modules_raw(accept,
                                                address,
                                                args.ledger_version,
                                                start,
                                                args.limit).await.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn get_ledger_info(&self) -> RpcRes {
        let api = self.api_service.as_ref().unwrap();
        let ret = api.2.get_ledger_info_raw(AcceptType::Json).await.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn view_function(&self, args: RpcReq) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let api = self.api_service.as_ref().unwrap();
        let req = serde_json::from_str::<ViewRequest>(args.data.as_str()).unwrap();
        let ret = api.1.view_function_raw(
            accept,
            req,
            args.ledger_version,
        ).await;
        let ret = ret.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn get_transaction_by_hash(&self, args: RpcReq) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let h = args.data.as_str();
        let h1 = HashValue::from_hex(h).unwrap();
        let hash = aptos_api_types::hash::HashValue::from(h1);
        let api = self.api_service.as_ref().unwrap();
        let ret = api.0.get_transaction_by_hash_raw(accept,
                                                    hash).await;
        let ret = ret.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn get_transaction_by_version(&self, args: GetTransactionByVersionArgs) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let api = self.api_service.as_ref().unwrap();
        let ret = api.0.get_transaction_by_version_raw(accept,
                                                       args.version).await;
        let ret = ret.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn encode_submission(&self, data: &str) -> RpcRes {
        let service = self.api_service.as_ref().unwrap();
        let payload = serde_json::from_str::<EncodeSubmissionRequest>(data).unwrap();
        let ret =
            service.0.encode_submission_raw(AcceptType::Json, payload).await;
        let ret = ret.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn submit_transaction(&self, data: Vec<u8>, accept: AcceptType) -> RpcRes {
        log::info!("submit_transaction length {}",{data.len()});
        let service = self.api_service.as_ref().unwrap();
        let payload = SubmitTransactionPost::Bcs(aptos_api::bcs_payload::Bcs(data.clone()));
        let ret =
            service.0.submit_transaction_raw(accept, payload).await;
        let ret = ret.unwrap();
        let header;
        let ret = match ret {
            SubmitTransactionResponse::Accepted(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        let signed_transaction: SignedTransaction =
            bcs::from_bytes_with_limit(&data,
                                       MAX_RECURSIVE_TYPES_ALLOWED as usize).unwrap();
        let sender = self.app_sender.as_ref().unwrap();
        sender.send_app_gossip(serde_json::to_vec(&signed_transaction.clone()).unwrap()).await.unwrap();
        self.add_pool(signed_transaction).await;
        self.notify_block_ready().await;
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn submit_transaction_batch(&self, data: Vec<u8>, accept: AcceptType) -> RpcRes {
        log::info!("submit_transaction_batch length {}",{data.len()});
        let service = self.api_service.as_ref().unwrap();
        let payload = SubmitTransactionsBatchPost::Bcs(aptos_api::bcs_payload::Bcs(data.clone()));
        let ret = service.0.submit_transactions_batch_raw(accept,
                                                          payload).await;
        let ret = ret.unwrap();
        let mut failed_index = vec![];
        let header;
        let ret = match ret {
            SubmitTransactionsBatchResponse::Accepted(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
            SubmitTransactionsBatchResponse::AcceptedPartial(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        for x in &json.transaction_failures {
                            failed_index.push(x.transaction_index.clone());
                        }
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        let signed_transactions: Vec<SignedTransaction> =
            bcs::from_bytes(&data).unwrap();
        let sender = self.app_sender.as_ref().unwrap();
        let mut exist_count = 0;
        for (i, signed_transaction) in signed_transactions.iter().enumerate() {
            if !failed_index.contains(&i) {
                sender.send_app_gossip(serde_json::to_vec(signed_transaction).unwrap()).await.unwrap();
                self.add_pool(signed_transaction.clone()).await;
            } else {
                exist_count += 1;
            }
        }
        if exist_count > 0 {
            self.notify_block_ready().await;
        }
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }
    pub async fn get_table_item(&self, args: RpcTableReq) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let account = args.query;
        let body = args.body;
        let payload = serde_json::from_str::<TableItemRequest>(body.as_str()).unwrap();
        let api = self.api_service.as_ref().unwrap();
        let ret = api.4.get_table_item_raw(
            accept,
            Address::from_str(account.as_str()).unwrap(),
            payload,
            args.ledger_version).await;
        let ret = ret.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn get_raw_table_item(&self, args: RpcTableReq) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let account = args.query;
        let body = args.body;
        let payload = serde_json::from_str::<RawTableItemRequest>(body.as_str()).unwrap();
        let api = self.api_service.as_ref().unwrap();
        let ret = api.4.get_raw_table_item_raw(
            accept,
            Address::from_str(account.as_str()).unwrap(),
            payload,
            args.ledger_version).await;
        let ret = ret.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn get_events_by_creation_number(&self, args: RpcEventNumReq) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let api = self.api_service.as_ref().unwrap();
        let ret = api.6.get_events_by_creation_number_raw(
            accept,
            Address::from_str(args.address.as_str()).unwrap(),
            args.creation_number,
            args.start, args.limit).await;
        let ret = ret.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }
    pub async fn get_events_by_event_handle(&self, args: RpcEventHandleReq) -> RpcRes {
        let accept = if args.is_bsc_format.unwrap_or(false) {
            AcceptType::Bcs
        } else {
            AcceptType::Json
        };
        let event_handle = MoveStructTag::from_str(args.event_handle.as_str()).unwrap();
        let field_name = IdentifierWrapper::from_str(args.field_name.as_str()).unwrap();
        let api = self.api_service.as_ref().unwrap();
        let ret = api.6.get_events_by_event_handle_raw(
            accept,
            Address::from_str(args.address.as_str()).unwrap(),
            event_handle,
            field_name, args.start, args.limit).await;
        let ret = ret.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    async fn add_pool(&self, signed_transaction: SignedTransaction) {
        let mut core_pool = self.core_mempool.as_ref().unwrap().write().await;
        core_pool.add_txn(signed_transaction.clone(),
                          0,
                          signed_transaction.clone().sequence_number(),
                          TimelineState::NonQualified);
        drop(core_pool);
    }
    async fn get_pending_tx(&self, count: u64) -> Vec<SignedTransaction> {
        let core_pool = self.core_mempool.as_ref().unwrap().read().await;
        core_pool.get_batch(count,
                            1024 * 5 * 1000,
                            true,
                            true, vec![])
    }

    async fn check_pending_tx(&self) {
        let shared_self = Arc::new(self.clone());
        let check_duration = Duration::from_millis(2000);
        tokio::spawn(async move {
            let mut last_check_build_time = get_current_time_seconds();
            loop {
                _ = tokio::time::sleep(check_duration).await;
                let status = shared_self.build_status.try_read();
                match status {
                    Ok(s_) => {
                        let s = s_.clone();
                        drop(s_);
                        if s == 0 { // build finished
                            last_check_build_time = get_current_time_seconds();
                            let more = shared_self.has_pending_tx.try_read();
                            match more {
                                Ok(t_) => {
                                    let t = t_.clone();
                                    drop(t_);
                                    if t == true {
                                        // build finished but the memory pool still has pending transactions
                                        shared_self.update_pending_tx_flag(false).await;
                                        shared_self.notify_block_ready2().await;
                                    }
                                }
                                _ => {}
                            }
                        } else { // still pending
                            let now = get_current_time_seconds();
                            if (now - last_check_build_time) > 120 { // 120s
                                // timeout for build, we can send more pending tx to the engine
                                shared_self.update_build_block_status(0).await;
                                last_check_build_time = get_current_time_seconds();
                            }
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    async fn update_build_block_status(&self, s: u8) {
        let mut status = self.build_status.write().await;
        if *status != s {
            *status = s;
        }
    }

    async fn update_pending_tx_flag(&self, n: bool) {
        let mut tx = self.has_pending_tx.write().await;
        if *tx != n {
            *tx = n;
        }
    }

    async fn notify_block_ready2(&self) {
        if let Some(to_engine) = &self.to_engine {
            let send_result = {
                let to_engine = to_engine.read().await;
                to_engine.send(PendingTxs).await
            };
            if send_result.is_ok() {
                self.update_build_block_status(1).await;
                log::info!("notify_block_ready:success");
            } else {
                log::info!("send tx to_engine error ")
            }
        } else {
            log::info!("send tx to_engine error ")
        }
    }

    async fn notify_block_ready(&self) {
        let status_ = self.build_status.read().await;
        let tx_ = self.has_pending_tx.read().await;
        let status = status_.clone();
        let tx = tx_.clone();
        drop(tx_);
        drop(status_);
        match status {
            1 => { // building
                if tx == false {
                    self.update_pending_tx_flag(true).await;
                } else {}
                log::info!("notify_block_ready ignore");
            }
            0 => {// done
                self.notify_block_ready2().await;
            }
            _ => {}
        }
    }

    pub async fn simulate_transaction(&self, data: Vec<u8>, accept: AcceptType) -> RpcRes {
        let service = self.api_service.as_ref().unwrap();
        let ret = service.0.simulate_transaction_raw(
            accept,
            Some(true),
            Some(false),
            Some(true),
            SubmitTransactionPost::Bcs(aptos_api::bcs_payload::Bcs(data))).await;
        let ret = ret.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn estimate_gas_price(&self) -> RpcRes {
        let service = self.api_service.as_ref().unwrap();
        let ret = service.0.estimate_gas_price_raw(
            AcceptType::Json).await;
        let ret = ret.unwrap();
        let header;
        let ret = match ret {
            BasicResponse::Ok(c, a, b, d, e, f, g, h, k) => {
                header = AptosHeader {
                    chain_id: a,
                    ledger_version: b,
                    ledger_oldest_version: d,
                    ledger_timestamp_usec: e,
                    epoch: f,
                    block_height: g,
                    oldest_block_height: h,
                    cursor: k,
                };
                match c {
                    AptosResponseContent::Json(json) => {
                        serde_json::to_string(&json.0).unwrap()
                    }
                    AptosResponseContent::Bcs(bytes) => {
                        format!("{}", hex::encode(bytes.0))
                    }
                }
            }
        };
        RpcRes { data: ret, header: serde_json::to_string(&header).unwrap() }
    }

    pub async fn facet_apt(&self, acc: Vec<u8>, accept: AcceptType) -> RpcRes {
        let to = AccountAddress::from_bytes(acc).unwrap();
        let db = self.db.as_ref().unwrap().read().await;
        let mut core_account = self.get_core_account(&db).await;
        let tx_factory = TransactionFactory::new(ChainId::test());
        let tx_acc_mint = core_account
            .sign_with_transaction_builder(
                tx_factory.mint(to, 10 * 100_000_000)
            );
        return self.submit_transaction(bcs::to_bytes(&tx_acc_mint).unwrap(), accept).await;
    }

    pub async fn create_account(&self, key: &str, accept: AcceptType) -> RpcRes {
        let to = Ed25519PublicKey::from_encoded_string(key).unwrap();
        let db = self.db.as_ref().unwrap().read().await;
        let mut core_account = self.get_core_account(&db).await;
        let tx_factory = TransactionFactory::new(ChainId::test());
        let tx_acc_create = core_account
            .sign_with_transaction_builder(
                tx_factory.create_user_account(&to)
            );
        return self.submit_transaction(bcs::to_bytes(&tx_acc_create).unwrap(), accept).await;
    }

    /// Sets the state of the Vm.
    /// # Errors
    /// Will fail if the `snow::State` is syncing
    pub async fn set_state(&self, snow_state: snow::State) -> io::Result<()> {
        let mut vm_state = self.state.write().await;
        match snow_state {
            // called by chains manager when it is creating the chain.
            snow::State::Initializing => {
                log::info!("set_state: initializing");
                vm_state.bootstrapped = false;
                Ok(())
            }

            snow::State::StateSyncing => {
                log::info!("set_state: state syncing");
                Err(Error::new(ErrorKind::Other, "state sync is not supported"))
            }

            // called by the bootstrapper to signal bootstrapping has started.
            snow::State::Bootstrapping => {
                log::info!("set_state: bootstrapping");
                vm_state.bootstrapped = false;
                Ok(())
            }

            // called when consensus has started signalling bootstrap phase is complete.
            snow::State::NormalOp => {
                log::info!("set_state: normal op");
                vm_state.bootstrapped = true;
                Ok(())
            }
        }
    }


    /// Sets the container preference of the Vm.
    pub async fn set_preference(&self, id: ids::Id) -> io::Result<()> {
        let mut vm_state = self.state.write().await;
        vm_state.preferred = id;

        Ok(())
    }

    /// Returns the last accepted block Id.
    pub async fn last_accepted(&self) -> io::Result<ids::Id> {
        let vm_state = self.state.read().await;
        if let Some(state) = &vm_state.state {
            let blk_id = state.get_last_accepted_block_id().await?;
            return Ok(blk_id);
        }
        Err(Error::new(ErrorKind::NotFound, "state manager not found"))
    }

    pub async fn get_core_account(&self, db: &DbReaderWriter) -> LocalAccount {
        let acc = aptos_test_root_address();
        let state_proof = db.reader.get_latest_ledger_info().unwrap();
        let current_version = state_proof.ledger_info().version();
        let db_state_view = db.reader.state_view_at_version(Some(current_version)).unwrap();
        let view = db_state_view.as_account_with_state_view(&acc);
        let av = view.get_account_resource().unwrap();
        let sn = av.unwrap().sequence_number();
        LocalAccount::new(
            aptos_test_root_address(),
            AccountKey::from_private_key(GENESIS_KEYPAIR.0.clone()),
            sn,
        )
    }


    pub async fn inner_build_block(&self, data: Vec<u8>) -> io::Result<()> {
        let executor = self.executor.as_ref().unwrap().read().await;
        let aptos_data = serde_json::from_slice::<AptosData>(&data).unwrap();
        let block_tx = serde_json::from_slice::<Vec<Transaction>>(&aptos_data.0).unwrap();
        let block_meta = block_tx.get(0).unwrap().try_as_block_metadata().unwrap();
        let block_id_now = block_meta.id();
        let block_id = aptos_data.1;

        if block_id_now.ne(&block_id) {
            return Err(Error::new(
                ErrorKind::Interrupted,
                "block format error",
            ));
        }
        let parent_block_id = aptos_data.2;
        let parent_block_id_now = executor.committed_block_id();
        if parent_block_id.ne(&parent_block_id_now) {
            return Err(Error::new(
                ErrorKind::Interrupted,
                "block error,maybe not sync ",
            ));
        }
        let next_epoch = aptos_data.3;
        let ts = aptos_data.4;
        match executor
            .execute_block((block_id, block_tx.clone()), parent_block_id) {
            Ok(output) => {
                let ledger_info = LedgerInfo::new(
                    BlockInfo::new(
                        next_epoch,
                        0,
                        block_id,
                        output.root_hash(),
                        output.version(),
                        ts,
                        output.epoch_state().clone(),
                    ),
                    HashValue::zero(),
                );
                let li = generate_ledger_info_with_sig(&[self.signer.as_ref().unwrap().clone()], ledger_info);
                executor.commit_blocks(vec![block_id], li.clone()).unwrap();
            }
            Err(err) => {
                log::info!("inner build error {}", err);
            }
        }
        let mut core_pool = self.core_mempool.as_ref().unwrap().write().await;
        for t in block_tx.iter() {
            match t {
                UserTransaction(t) => {
                    let sender = t.sender();
                    let sequence_number = t.sequence_number();
                    core_pool.commit_transaction(&AccountAddress::from(sender), sequence_number);
                }
                _ => {}
            }
        }
        drop(core_pool);
        self.update_build_block_status(0).await;
        Ok(())
    }

    async fn init_aptos(&mut self) {
        let (genesis, validators) = test_genesis_change_set_and_validators(Some(1));
        let signer = ValidatorSigner::new(
            validators[0].data.owner_address,
            validators[0].consensus_key.clone(),
        );
        self.signer = Some(signer.clone());
        let genesis_txn = Transaction::GenesisTransaction(WriteSetPayload::Direct(genesis));
        let p = format!("{}/{}",
                        dirs::home_dir().unwrap().to_str().unwrap(),
                        MOVE_DB_DIR);
        if !fs::metadata(p.clone().as_str()).is_ok() {
            fs::create_dir_all(p.as_str()).unwrap();
        }
        let db = DbReaderWriter::wrap(
            AptosDB::new_for_test(p.as_str()));
        let waypoint = generate_waypoint::<AptosVM>(&db.1, &genesis_txn);
        match waypoint {
            Ok(w) => {
                maybe_bootstrap::<AptosVM>(&db.1, &genesis_txn, w).unwrap();
            }
            _ => {}
        }
        // BLOCK-STM
        // AptosVM::set_concurrency_level_once(2);
        self.db = Some(Arc::new(RwLock::new(db.1.clone())));
        let executor = BlockExecutor::new(db.1.clone());
        self.executor = Some(Arc::new(RwLock::new(executor)));

        let (mempool_client_sender,
            mut mempool_client_receiver) = futures_mpsc::channel::<MempoolClientRequest>(10);
        let sender = MempoolClientSender::from(mempool_client_sender);
        let node_config = NodeConfig::default();
        let context = Context::new(ChainId::test(),
                                   db.1.reader.clone(),
                                   sender, node_config.clone());
        self.api_context = Some(context.clone());
        let service = get_raw_api_service(Arc::new(context));
        self.api_service = Some(service);
        self.core_mempool = Some(Arc::new(RwLock::new(CoreMempool::new(&node_config))));
        self.check_pending_tx().await;
        tokio::task::spawn(async move {
            while let Some(request) = mempool_client_receiver.next().await {
                match request {
                    MempoolClientRequest::SubmitTransaction(_t, callback) => {
                        // accept all the transaction
                        let ms = MempoolStatus::new(MempoolStatusCode::Accepted);
                        let status: SubmissionStatus = (ms, None);
                        callback.send(
                            Ok(status)
                        ).unwrap();
                    }
                    MempoolClientRequest::GetTransactionByHash(_, _) => {}
                }
            }
        });
    }
}

#[tonic::async_trait]
impl BatchedChainVm for Vm {
    type Block = Block;

    async fn get_ancestors(
        &self,
        _block_id: ids::Id,
        _max_block_num: i32,
        _max_block_size: i32,
        _max_block_retrival_time: Duration,
    ) -> io::Result<Vec<Bytes>> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "get_ancestors not implemented",
        ))
    }

    async fn batched_parse_block(&self, _blocks: &[Vec<u8>]) -> io::Result<Vec<Self::Block>> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "batched_parse_block not implemented",
        ))
    }
}

#[tonic::async_trait]
impl ChainVm for Vm
{
    type Block = Block;

    async fn build_block(
        &self,
    ) -> io::Result<<Self as ChainVm>::Block> {
        let vm_state = self.state.read().await;
        if let Some(state_b) = vm_state.state.as_ref() {
            let prnt_blk = state_b.get_block(&vm_state.preferred).await.unwrap();
            let unix_now = Utc::now().timestamp() as u64;
            let tx_arr = self.get_pending_tx(10000).await;
            let len = tx_arr.clone().len();
            log::info!("build_block pool tx count {}",len );
            // now we allow to build empty block
            // if len == 0 {
            //     self.update_build_block_status(0).await;
            //     self.update_pending_tx_flag(false).await;
            //     return Err(Error::new(
            //         ErrorKind::Other,
            //         "no pending transaction found",
            //     ));
            // }
            let executor = self.executor.as_ref().unwrap().read().await;
            let signer = self.signer.as_ref().unwrap();
            let db = self.db.as_ref().unwrap().read().await;
            let latest_ledger_info = db.reader.get_latest_ledger_info().unwrap();
            let next_epoch = latest_ledger_info.ledger_info().next_block_epoch();
            let block_id = HashValue::random();
            let block_meta = Transaction::BlockMetadata(BlockMetadata::new(
                block_id,
                next_epoch,
                0,
                signer.author(),
                vec![],
                vec![],
                unix_now,
            ));
            let mut txs = vec![];
            for tx in tx_arr.iter() {
                txs.push(UserTransaction(tx.clone()));
            }
            let mut block_tx: Vec<_> = vec![];
            block_tx.push(block_meta);
            block_tx.append(&mut txs);
            block_tx.push(Transaction::StateCheckpoint(HashValue::random()));
            let parent_block_id = executor.committed_block_id();
            let block_tx_bytes = serde_json::to_vec(&block_tx).unwrap();
            let data = AptosData(block_tx_bytes,
                                 block_id.clone(),
                                 parent_block_id,
                                 next_epoch,
                                 unix_now);

            let mut block_ = Block::new(
                prnt_blk.id(),
                prnt_blk.height() + 1,
                unix_now,
                serde_json::to_vec(&data).unwrap(),
                choices::status::Status::Processing,
            ).unwrap();
            block_.set_state(state_b.clone());
            block_.verify().await.unwrap();
            return Ok(block_);
        }
        Err(Error::new(
            ErrorKind::Other,
            "not implement",
        ))
    }

    async fn issue_tx(
        &self,
    ) -> io::Result<<Self as ChainVm>::Block> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "issue_tx not implemented",
        ))
    }
    async fn verify_height_index(&self) -> io::Result<()> {
        Ok(())
    }

    // Returns an error as a no-op for now.
    async fn get_block_id_at_height(&self, _height: u64) -> io::Result<ids::Id> {
        Err(Error::new(ErrorKind::NotFound, "block id not found"))
    }

    async fn state_sync_enabled(&self) -> io::Result<bool> {
        Ok(false)
    }

    async fn set_preference(&self, id: ids::Id) -> io::Result<()> {
        self.set_preference(id).await
    }

    async fn last_accepted(&self) -> io::Result<ids::Id> {
        self.last_accepted().await
    }
}

#[tonic::async_trait]
impl NetworkAppHandler for Vm
{
    /// Currently, no app-specific messages, so returning Ok.
    async fn app_request(
        &self,
        _node_id: &ids::node::Id,
        _request_id: u32,
        _deadline: DateTime<Utc>,
        _request: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    /// Currently, no app-specific messages, so returning Ok.
    async fn app_request_failed(
        &self,
        _node_id: &ids::node::Id,
        _request_id: u32,
    ) -> io::Result<()> {
        Ok(())
    }

    /// Currently, no app-specific messages, so returning Ok.
    async fn app_response(
        &self,
        _node_id: &ids::node::Id,
        _request_id: u32,
        _response: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    async fn app_gossip(&self, _node_id: &ids::node::Id, msg: &[u8]) -> io::Result<()> {
        match serde_json::from_slice::<SignedTransaction>(msg) {
            Ok(s) => {
                self.add_pool(s).await;
            }
            Err(_) => {}
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl CrossChainAppHandler for Vm
{
    /// Currently, no cross chain specific messages, so returning Ok.
    async fn cross_chain_app_request(
        &self,
        _chain_id: &ids::Id,
        _request_id: u32,
        _deadline: DateTime<Utc>,
        _request: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }

    /// Currently, no cross chain specific messages, so returning Ok.
    async fn cross_chain_app_request_failed(
        &self,
        _chain_id: &ids::Id,
        _request_id: u32,
    ) -> io::Result<()> {
        Ok(())
    }

    /// Currently, no cross chain specific messages, so returning Ok.
    async fn cross_chain_app_response(
        &self,
        _chain_id: &ids::Id,
        _request_id: u32,
        _response: &[u8],
    ) -> io::Result<()> {
        Ok(())
    }
}

impl AppHandler for Vm {}

#[tonic::async_trait]
impl Connector for Vm

{
    async fn connected(&self, _id: &ids::node::Id) -> io::Result<()> {
        // no-op
        Ok(())
    }

    async fn disconnected(&self, _id: &ids::node::Id) -> io::Result<()> {
        // no-op
        Ok(())
    }
}

#[tonic::async_trait]
impl Checkable for Vm
{
    async fn health_check(&self) -> io::Result<Vec<u8>> {
        Ok("200".as_bytes().to_vec())
    }
}

#[tonic::async_trait]
impl Getter for Vm
{
    type Block = Block;

    async fn get_block(
        &self,
        blk_id: ids::Id,
    ) -> io::Result<<Self as Getter>::Block> {
        let vm_state = self.state.read().await;
        if let Some(state) = &vm_state.state {
            let mut block = state.get_block(&blk_id).await?;
            let mut new_state = state.clone();
            new_state.set_vm(self.clone());
            block.set_state(new_state);
            return Ok(block);
        }
        Err(Error::new(ErrorKind::NotFound, "state manager not found"))
    }
}

#[tonic::async_trait]
impl Parser for Vm
{
    type Block = Block;
    async fn parse_block(
        &self,
        bytes: &[u8],
    ) -> io::Result<<Self as Parser>::Block> {
        let vm_state = self.state.read().await;
        if let Some(state) = vm_state.state.as_ref() {
            let mut new_block = Block::from_slice(bytes)?;
            new_block.set_status(choices::status::Status::Processing);
            let mut new_state = state.clone();
            new_state.set_vm(self.clone());
            new_block.set_state(new_state);
            return match state.get_block(&new_block.id()).await {
                Ok(prev) => {
                    Ok(prev)
                }
                Err(_) => {
                    Ok(new_block)
                }
            };
        }
        Err(Error::new(ErrorKind::NotFound, "state manager not found"))
    }
}

#[tonic::async_trait]
impl CommonVm for Vm
{
    type DatabaseManager = DatabaseManager;
    type AppSender = AppSenderClient;
    type ChainHandler = ChainHandler<ChainService>;
    type StaticHandler = StaticHandler;
    type ValidatorState = ValidatorStateClient;

    async fn initialize(
        &mut self,
        ctx: Option<subnet::rpc::context::Context<Self::ValidatorState>>,
        db_manager: Self::DatabaseManager,
        genesis_bytes: &[u8],
        _upgrade_bytes: &[u8],
        _config_bytes: &[u8],
        to_engine: Sender<snow::engine::common::message::Message>,
        _fxs: &[snow::engine::common::vm::Fx],
        app_sender: Self::AppSender,
    ) -> io::Result<()> {
        let mut vm_state = self.state.write().await;
        vm_state.ctx = ctx.clone();
        let current = db_manager.current().await?;
        let state = state::State {
            db: Arc::new(RwLock::new(current.clone().db)),
            verified_blocks: Arc::new(RwLock::new(HashMap::new())),
            vm: None,
        };
        vm_state.state = Some(state.clone());
        self.to_engine = Some(Arc::new(RwLock::new(to_engine)));
        self.app_sender = Some(app_sender);
        drop(vm_state);

        self.init_aptos().await;
        let mut vm_state = self.state.write().await;
        let genesis = "hello world";
        let has_last_accepted = state.has_last_accepted_block().await?;
        if has_last_accepted {
            let last_accepted_blk_id = state.get_last_accepted_block_id().await?;
            vm_state.preferred = last_accepted_blk_id;
        } else {
            let genesis_bytes = genesis.as_bytes().to_vec();
            let data = AptosData(genesis_bytes.clone(),
                                 HashValue::zero(),
                                 HashValue::zero(),
                                 0,
                                 0);
            let mut genesis_block = Block::new(
                ids::Id::empty(),
                0,
                0,
                serde_json::to_vec(&data).unwrap(),
                choices::status::Status::default(),
            ).unwrap();
            genesis_block.set_state(state.clone());
            genesis_block.accept().await?;

            let genesis_blk_id = genesis_block.id();
            vm_state.preferred = genesis_blk_id;
        }
        log::info!("successfully initialized Vm");
        Ok(())
    }

    async fn set_state(&self, snow_state: snow::State) -> io::Result<()> {
        self.set_state(snow_state).await
    }

    /// Called when the node is shutting down.
    async fn shutdown(&self) -> io::Result<()> {
        Ok(())
    }

    async fn version(&self) -> io::Result<String> {
        Ok(String::from(VERSION))
    }

    async fn create_static_handlers(
        &mut self,
    ) -> io::Result<HashMap<String, HttpHandler<Self::StaticHandler>>> {
        let handler = StaticHandler::new(StaticService::new());
        let mut handlers = HashMap::new();
        handlers.insert(
            "/static".to_string(),
            HttpHandler {
                lock_option: LockOptions::WriteLock,
                handler,
                server_addr: None,
            },
        );

        Ok(handlers)
    }

    async fn create_handlers(
        &mut self,
    ) -> io::Result<HashMap<String, HttpHandler<Self::ChainHandler>>> {
        let handler = ChainHandler::new(ChainService::new(self.clone()));
        let mut handlers = HashMap::new();
        handlers.insert(
            "/rpc".to_string(),
            HttpHandler {
                lock_option: LockOptions::WriteLock,
                handler,
                server_addr: None,
            },
        );

        Ok(handlers)
    }
}

fn get_current_time_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Failed to get timestamp")
        .as_secs()
}