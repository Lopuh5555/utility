#![doc = include_str!("../README.md")]

use actix::{Addr, MailboxError};
use actix_cors::Cors;
use actix_web::http::header;
use actix_web::HttpRequest;
use actix_web::{get, http, middleware, web, App, Error as HttpError, HttpResponse, HttpServer};
use api::RpcRequest;
pub use api::{RpcFrom, RpcInto};
use futures::Future;
use futures::FutureExt;
use unc_chain_configs::GenesisConfig;
use unc_client::{
    ClientActor, DebugStatus, GetBlock, GetBlockProof, GetChunk, GetClientConfig,
    GetExecutionOutcome, GetGasPrice, GetMaintenanceWindows, GetNetworkInfo,
    GetNextLightClientBlock, GetProtocolConfig, GetReceipt, GetStateChanges,
    GetStateChangesInBlock, GetValidatorInfo, GetValidatorOrdered, ProcessTxRequest,
    ProcessTxResponse, Query, Status, TxStatus, ViewClientActor,
};
use unc_client_primitives::types::{GetProvider, GetSplitStorageInfo};
pub use unc_jsonrpc_client as client;
use unc_jsonrpc_primitives::errors::RpcError;
use unc_jsonrpc_primitives::message::{Message, Request};
use unc_jsonrpc_primitives::types::config::RpcProtocolConfigResponse;
use unc_jsonrpc_primitives::types::entity_debug::{EntityDebugHandler, EntityQuery};
use unc_jsonrpc_primitives::types::query::RpcQueryRequest;
use unc_jsonrpc_primitives::types::split_storage::{
    RpcSplitStorageInfoRequest, RpcSplitStorageInfoResponse,
};
use unc_jsonrpc_primitives::types::transactions::{
    RpcSendTransactionRequest, RpcTransactionResponse,
};
use unc_network::tcp;
use unc_network::PeerManagerActor;
use unc_o11y::metrics::{prometheus, Encoder, TextEncoder};
use unc_o11y::{WithSpanContext, WithSpanContextExt};
use unc_primitives::hash::CryptoHash;
use unc_primitives::transaction::SignedTransaction;
use unc_primitives::types::{AccountId, BlockHeight};
use unc_primitives::views::{QueryRequest, TxExecutionStatus};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::{sleep, timeout};
use tracing::{error, info};

mod api;
mod metrics;

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug)]
pub struct RpcPollingConfig {
    pub polling_interval: Duration,
    pub polling_timeout: Duration,
}

impl Default for RpcPollingConfig {
    fn default() -> Self {
        Self {
            polling_interval: Duration::from_millis(500),
            polling_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct RpcLimitsConfig {
    /// Maximum byte size of the json payload.
    pub json_payload_max_size: usize,
}

impl Default for RpcLimitsConfig {
    fn default() -> Self {
        Self { json_payload_max_size: 10 * 1024 * 1024 }
    }
}

fn default_enable_debug_rpc() -> bool {
    false
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct RpcConfig {
    pub addr: tcp::ListenerAddr,
    // If provided, will start an http server exporting only Prometheus metrics on that address.
    pub prometheus_addr: Option<String>,
    pub cors_allowed_origins: Vec<String>,
    pub polling_config: RpcPollingConfig,
    #[serde(default)]
    pub limits_config: RpcLimitsConfig,
    // If true, enable some debug RPC endpoints (like one to get the latest block).
    // We disable it by default, as some of those endpoints might be quite CPU heavy.
    #[serde(default = "default_enable_debug_rpc")]
    pub enable_debug_rpc: bool,
    // For node developers only: if specified, the HTML files used to serve the debug pages will
    // be read from this directory, instead of the contents compiled into the binary. This allows
    // for quick iterative development.
    pub experimental_debug_pages_src_path: Option<String>,
}

impl Default for RpcConfig {
    fn default() -> Self {
        RpcConfig {
            addr: tcp::ListenerAddr::new("0.0.0.0:3030".parse().unwrap()),
            prometheus_addr: None,
            cors_allowed_origins: vec!["*".to_owned()],
            polling_config: Default::default(),
            limits_config: Default::default(),
            enable_debug_rpc: false,
            experimental_debug_pages_src_path: None,
        }
    }
}

impl RpcConfig {
    pub fn new(addr: tcp::ListenerAddr) -> Self {
        RpcConfig { addr, ..Default::default() }
    }
}

/// Serialises response of a query into JSON to be sent to the client.
///
/// Returns an internal server error if the value fails to serialise.
fn serialize_response(value: impl serde::ser::Serialize) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|err| RpcError::serialization_error(err.to_string()))
}

/// Processes a specific method call.
///
/// The arguments for the method (which is implemented by the `callback`) will
/// be parsed (using [`RpcRequest::parse`]) from the `request.params`.  Ok
/// results of the `callback` will be converted into a [`Value`] via serde
/// serialisation.
async fn process_method_call<R, V, E, F>(
    request: Request,
    callback: impl FnOnce(R) -> F,
) -> Result<Value, RpcError>
where
    R: RpcRequest,
    V: serde::ser::Serialize,
    RpcError: std::convert::From<E>,
    F: std::future::Future<Output = Result<V, E>>,
{
    serialize_response(callback(R::parse(request.params)?).await?)
}

#[easy_ext::ext(FromNetworkClientResponses)]
impl unc_jsonrpc_primitives::types::transactions::RpcTransactionError {
    pub fn from_network_client_responses(resp: ProcessTxResponse) -> Self {
        match resp {
            ProcessTxResponse::InvalidTx(context) => Self::InvalidTransaction { context },
            ProcessTxResponse::NoResponse => Self::TimeoutError,
            ProcessTxResponse::DoesNotTrackShard | ProcessTxResponse::RequestRouted => {
                Self::DoesNotTrackShard
            }
            internal_error => Self::InternalError { debug_info: format!("{:?}", internal_error) },
        }
    }
}

/// This function processes response from query method to introduce
/// backward compatible response in case of specific errors
fn process_query_response(
    query_response: Result<
        unc_jsonrpc_primitives::types::query::RpcQueryResponse,
        unc_jsonrpc_primitives::types::query::RpcQueryError,
    >,
) -> Result<Value, RpcError> {
    // This match is used here to give backward compatible error message for specific
    // error variants. Should be refactored once structured errors fully shipped
    match query_response {
        Ok(rpc_query_response) => serialize_response(rpc_query_response),
        Err(err) => match err {
            unc_jsonrpc_primitives::types::query::RpcQueryError::ContractExecutionError {
                vm_error,
                block_height,
                block_hash,
            } => Ok(json!({
                "error": vm_error,
                "logs": json!([]),
                "block_height": block_height,
                "block_hash": block_hash,
            })),
            unc_jsonrpc_primitives::types::query::RpcQueryError::UnknownAccessKey {
                public_key,
                block_height,
                block_hash,
            } => Ok(json!({
                "error": format!("access key {} does not exist while viewing", public_key),
                "logs": json!([]),
                "block_height": block_height,
                "block_hash": block_hash,
            })),
            unc_jsonrpc_primitives::types::query::RpcQueryError::UnknownBlock {
                block_reference: unc_primitives::types::BlockReference::BlockId(ref block_id),
            } => {
                let error_data = Some(match block_id {
                    unc_primitives::types::BlockId::Height(height) => json!(format!(
                        "DB Not Found Error: BLOCK HEIGHT: {} \n Cause: Unknown",
                        height
                    )),
                    unc_primitives::types::BlockId::Hash(block_hash) => {
                        json!(format!("DB Not Found Error: BLOCK HEADER: {}", block_hash))
                    }
                });
                let error_data_value = match serde_json::to_value(err) {
                    Ok(value) => value,
                    Err(err) => {
                        return Err(RpcError::new_internal_error(
                            None,
                            format!("Failed to serialize RpcQueryError: {:?}", err),
                        ))
                    }
                };
                Err(RpcError::new_internal_or_handler_error(error_data, error_data_value))
            }
            _ => Err(err.into()),
        },
    }
}

struct JsonRpcHandler {
    client_addr: Addr<ClientActor>,
    view_client_addr: Addr<ViewClientActor>,
    peer_manager_addr: Option<Addr<PeerManagerActor>>,
    polling_config: RpcPollingConfig,
    genesis_config: GenesisConfig,
    enable_debug_rpc: bool,
    debug_pages_src_path: Option<PathBuf>,
    entity_debug_handler: Arc<dyn EntityDebugHandler>,
}

impl JsonRpcHandler {
    pub async fn process(&self, message: Message) -> Result<Message, HttpError> {
        let id = message.id();
        match message {
            Message::Request(request) => {
                Ok(Message::response(id, self.process_request(request).await))
            }
            _ => Ok(Message::error(RpcError::parse_error(
                "JSON RPC Request format was expected".to_owned(),
            ))),
        }
    }

    // `process_request` increments affected metrics but the request processing is done by
    // `process_request_internal`.
    async fn process_request(&self, request: Request) -> Result<Value, RpcError> {
        let timer = Instant::now();
        let (metrics_name, response) = self.process_request_internal(request).await;

        metrics::HTTP_RPC_REQUEST_COUNT.with_label_values(&[&metrics_name]).inc();
        metrics::RPC_PROCESSING_TIME
            .with_label_values(&[&metrics_name])
            .observe(timer.elapsed().as_secs_f64());

        if let Err(err) = &response {
            metrics::RPC_ERROR_COUNT
                .with_label_values(&[&metrics_name, &err.code.to_string()])
                .inc();
        }

        response
    }

    /// Processes the request without updating any metrics.
    /// Returns metrics name (method name with optional details as a suffix)
    /// and the result of the execution.
    async fn process_request_internal(
        &self,
        request: Request,
    ) -> (String, Result<Value, RpcError>) {
        let method_name = request.method.to_string();
        let request = match self.process_adversarial_request_internal(request).await {
            Ok(response) => return (method_name, response),
            Err(request) => request,
        };

        let request = match self.process_basic_requests_internal(request).await {
            Ok(response) => return (method_name, response),
            Err(request) => request,
        };

        match request.method.as_ref() {
            "query" => {
                let params: RpcQueryRequest = match RpcRequest::parse(request.params) {
                    Ok(params) => params,
                    Err(err) => return (method_name, Err(RpcError::from(err))),
                };
                let metrics_name = match params.request {
                    QueryRequest::ViewAccount { .. } => "query_view_account",
                    QueryRequest::ViewCode { .. } => "query_view_code",
                    QueryRequest::ViewState { include_proof, .. } => {
                        if include_proof {
                            "query_view_state_with_proof"
                        } else {
                            "query_view_state"
                        }
                    }
                    QueryRequest::ViewAccessKey { .. } => "query_view_access_key",
                    QueryRequest::ViewAccessKeyList { .. } => "query_view_access_key_list",
                    QueryRequest::CallFunction { .. } => "query_call_function",
                };
                (metrics_name.to_string(), process_query_response(self.query(params).await))
            }
            _ => {
                ("UNSUPPORTED_METHOD".to_string(), Err(RpcError::method_not_found(request.method)))
            }
        }
    }

    async fn process_basic_requests_internal(
        &self,
        request: Request,
    ) -> Result<Result<Value, RpcError>, Request> {
        Ok(match request.method.as_ref() {
            // Handlers ordered alphabetically
            "block" => process_method_call(request, |params| self.block(params)).await,
            "broadcast_tx_async" => {
                process_method_call(request, |params| async {
                    let tx = self.send_tx_async(params).await.to_string();
                    Result::<_, std::convert::Infallible>::Ok(tx)
                })
                .await
            }
            "broadcast_tx_commit" => {
                process_method_call(request, |params| self.send_tx_commit(params)).await
            }
            "chunk" => process_method_call(request, |params| self.chunk(params)).await,
            "gas_price" => process_method_call(request, |params| self.gas_price(params)).await,
            "health" => process_method_call(request, |_params: ()| self.health()).await,
            "light_client_proof" => {
                process_method_call(request, |params| {
                    self.light_client_execution_outcome_proof(params)
                })
                .await
            }
            "next_light_client_block" => {
                process_method_call(request, |params| self.next_light_client_block(params)).await
            }
            "network_info" => process_method_call(request, |_params: ()| self.network_info()).await,
            "send_tx" => process_method_call(request, |params| self.send_tx(params)).await,
            "status" => process_method_call(request, |_params: ()| self.status()).await,
            "tx" => {
                process_method_call(request, |params| self.tx_status_common(params, false)).await
            }
            "validators" => process_method_call(request, |params| self.validators(params)).await,
            "client_config" => {
                process_method_call(request, |_params: ()| self.client_config()).await
            }
            "EXPERIMENTAL_changes" => {
                process_method_call(request, |params| self.changes_in_block_by_type(params)).await
            }
            "EXPERIMENTAL_changes_in_block" => {
                process_method_call(request, |params| self.changes_in_block(params)).await
            }
            "EXPERIMENTAL_genesis_config" => {
                process_method_call(request, |_params: ()| async {
                    Result::<_, std::convert::Infallible>::Ok(&self.genesis_config)
                })
                .await
            }
            "EXPERIMENTAL_light_client_proof" => {
                process_method_call(request, |params| {
                    self.light_client_execution_outcome_proof(params)
                })
                .await
            }
            "EXPERIMENTAL_protocol_config" => {
                process_method_call(request, |params| self.protocol_config(params)).await
            }
            "EXPERIMENTAL_receipt" => {
                process_method_call(request, |params| self.receipt(params)).await
            }
            "EXPERIMENTAL_tx_status" => {
                process_method_call(request, |params| self.tx_status_common(params, true)).await
            }
            "EXPERIMENTAL_validators_ordered" => {
                process_method_call(request, |params| self.validators_ordered(params)).await
            }
            "EXPERIMENTAL_maintenance_windows" => {
                process_method_call(request, |params| self.maintenance_windows(params)).await
            }
            "EXPERIMENTAL_split_storage_info" => {
                process_method_call(request, |params| self.split_storage_info(params)).await
            }
            #[cfg(feature = "sandbox")]
            "sandbox_patch_state" => {
                process_method_call(request, |params| self.sandbox_patch_state(params)).await
            }
            #[cfg(feature = "sandbox")]
            "sandbox_fast_forward" => {
                process_method_call(request, |params| self.sandbox_fast_forward(params)).await
            }
            "provider" => {
                process_method_call(request, |params | self.get_provider(params)).await
            }
            _ => return Err(request),
        })
    }

    /// Handles adversarial requests if they are enabled.
    ///
    /// Adversarial requests are only enabled when `test_features` Cargo feature
    /// is turned on.  If the request has not been recognised as an adversarial
    /// request, returns `Err(request)` so that caller can continue handling the
    /// request.  Otherwise returns `Ok(response)` where `response` is the
    /// result of handling the request.
    #[cfg(not(feature = "test_features"))]
    async fn process_adversarial_request_internal(
        &self,
        request: Request,
    ) -> Result<Result<Value, RpcError>, Request> {
        Err(request)
    }

    #[cfg(feature = "test_features")]
    async fn process_adversarial_request_internal(
        &self,
        request: Request,
    ) -> Result<Result<Value, RpcError>, Request> {
        Ok(match request.method.as_ref() {
            "adv_disable_header_sync" => self.adv_disable_header_sync(request.params).await,
            "adv_disable_doomslug" => self.adv_disable_doomslug(request.params).await,
            "adv_produce_blocks" => self.adv_produce_blocks(request.params).await,
            "adv_switch_to_height" => self.adv_switch_to_height(request.params).await,
            "adv_get_saved_blocks" => self.adv_get_saved_blocks(request.params).await,
            "adv_check_store" => self.adv_check_store(request.params).await,
            _ => return Err(request),
        })
    }

    async fn client_send<M, T, E, F>(&self, msg: M) -> Result<T, E>
    where
        ClientActor: actix::Handler<WithSpanContext<M>>,
        M: actix::Message<Result = Result<T, F>> + Send + 'static,
        M::Result: Send,
        E: RpcFrom<F>,
        E: RpcFrom<actix::MailboxError>,
    {
        self.client_addr
            .send(msg.with_span_context())
            .await
            .map_err(RpcFrom::rpc_from)?
            .map_err(RpcFrom::rpc_from)
    }

    async fn view_client_send<M, T, E, F>(&self, msg: M) -> Result<T, E>
    where
        ViewClientActor: actix::Handler<WithSpanContext<M>>,
        M: actix::Message<Result = Result<T, F>> + Send + 'static,
        M::Result: Send,
        E: RpcFrom<F>,
        E: RpcFrom<actix::MailboxError>,
    {
        self.view_client_addr
            .send(msg.with_span_context())
            .await
            .map_err(RpcFrom::rpc_from)?
            .map_err(RpcFrom::rpc_from)
    }

    async fn peer_manager_send<M, T, E>(&self, msg: M) -> Result<T, E>
    where
        PeerManagerActor: actix::Handler<M>,
        M: actix::Message<Result = T> + Send + 'static,
        M::Result: Send,
        E: RpcFrom<actix::MailboxError>,
    {
        match &self.peer_manager_addr {
            Some(peer_manager_addr) => peer_manager_addr.send(msg).await.map_err(RpcFrom::rpc_from),
            None => Err(RpcFrom::rpc_from(MailboxError::Closed)),
        }
    }

    async fn send_tx_async(
        &self,
        request_data: unc_jsonrpc_primitives::types::transactions::RpcSendTransactionRequest,
    ) -> CryptoHash {
        let tx = request_data.signed_transaction;
        let hash = tx.get_hash();
        self.client_addr.do_send(
            ProcessTxRequest {
                transaction: tx,
                is_forwarded: false,
                check_only: false, // if we set true here it will not actually send the transaction
            }
            .with_span_context(),
        );
        hash
    }

    async fn tx_exists(
        &self,
        tx_hash: CryptoHash,
        signer_account_id: &AccountId,
    ) -> Result<bool, unc_jsonrpc_primitives::types::transactions::RpcTransactionError> {
        timeout(self.polling_config.polling_timeout, async {
            loop {
                // TODO(optimization): Introduce a view_client method to only get transaction
                // status without the information about execution outcomes.
                match self.view_client_send(
                    TxStatus {
                        tx_hash,
                        signer_account_id: signer_account_id.clone(),
                        fetch_receipt: false,
                    })
                    .await
                {
                    Ok(status) => {
                        if let Some(_) = status.execution_outcome {
                            return Ok(true);
                        }
                    }
                    Err(unc_jsonrpc_primitives::types::transactions::RpcTransactionError::UnknownTransaction {
                        ..
                    }) => {
                        return Ok(false);
                    }
                    _ => {}
                }
                sleep(self.polling_config.polling_interval).await;
            }
        })
        .await
        .map_err(|_| {
            metrics::RPC_TIMEOUT_TOTAL.inc();
            tracing::warn!(
                target: "jsonrpc", "Timeout: tx_exists method. tx_hash {:?} signer_account_id {:?}",
                tx_hash,
                signer_account_id
            );
            unc_jsonrpc_primitives::types::transactions::RpcTransactionError::TimeoutError
        })?
    }

    /// Return status of the given transaction
    ///
    /// `finality` forces the execution to wait until the desired finality level is reached
    async fn tx_status_fetch(
        &self,
        tx_info: unc_jsonrpc_primitives::types::transactions::TransactionInfo,
        finality: unc_primitives::views::TxExecutionStatus,
        fetch_receipt: bool,
    ) -> Result<
        unc_jsonrpc_primitives::types::transactions::RpcTransactionResponse,
        unc_jsonrpc_primitives::types::transactions::RpcTransactionError,
    > {
        let (tx_hash, account_id) = tx_info.to_tx_hash_and_account();
        let mut tx_status_result =
            Err(unc_jsonrpc_primitives::types::transactions::RpcTransactionError::TimeoutError);
        timeout(self.polling_config.polling_timeout, async {
            loop {
                tx_status_result = self.view_client_send( TxStatus {
                    tx_hash,
                    signer_account_id: account_id.clone(),
                    fetch_receipt,
                })
                .await;
                match tx_status_result.clone() {
                    Ok(result) => {
                        if result.status >= finality {
                            break Ok(result.into())
                        }
                        // else: No such transaction recorded on chain yet
                    },
                    Err(err @ unc_jsonrpc_primitives::types::transactions::RpcTransactionError::UnknownTransaction {
                        ..
                    }) => {
                        if let Some(tx) = tx_info.to_signed_tx() {
                            if let Ok(ProcessTxResponse::InvalidTx(context)) =
                                self.send_tx_internal(tx.clone(), true).await
                            {
                                break Err(
                                    unc_jsonrpc_primitives::types::transactions::RpcTransactionError::InvalidTransaction {
                                        context
                                    }
                                );
                            }
                        }
                        if finality == TxExecutionStatus::None {
                            break Err(err);
                        }
                    }
                    Err(err) => break Err(err),
                }
                sleep(self.polling_config.polling_interval).await;
            }
        })
        .await
        .map_err(|_| {
            metrics::RPC_TIMEOUT_TOTAL.inc();
            tracing::warn!(
                target: "jsonrpc", "Timeout: tx_status_fetch method. tx_info {:?} fetch_receipt {:?}",
                tx_info,
                fetch_receipt,
            );
            if let Err(error) = tx_status_result {
                error
            } else {
                unc_jsonrpc_primitives::types::transactions::RpcTransactionError::TimeoutError
            }
        })?
    }

    /// Send a transaction idempotently (subsequent send of the same transaction will not cause
    /// any new side-effects and the result will be the same unless we garbage collected it
    /// already).
    async fn send_tx_internal(
        &self,
        tx: SignedTransaction,
        check_only: bool,
    ) -> Result<ProcessTxResponse, unc_jsonrpc_primitives::types::transactions::RpcTransactionError>
    {
        let tx_hash = tx.get_hash();
        let signer_account_id = tx.transaction.signer_id.clone();
        let response = self
            .client_addr
            .send(
                ProcessTxRequest { transaction: tx, is_forwarded: false, check_only }
                    .with_span_context(),
            )
            .await
            .map_err(RpcFrom::rpc_from)?;

        // If we receive InvalidNonce error, it might be the case that the transaction was
        // resubmitted, and we should check if that is the case and return ValidTx response to
        // maintain idempotence of the send_tx method.
        if let ProcessTxResponse::InvalidTx(
            unc_primitives::errors::InvalidTxError::InvalidNonce { .. },
        ) = response
        {
            if self.tx_exists(tx_hash, &signer_account_id).await? {
                return Ok(ProcessTxResponse::ValidTx);
            }
        }

        Ok(response)
    }

    async fn send_tx(
        &self,
        request_data: unc_jsonrpc_primitives::types::transactions::RpcSendTransactionRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::transactions::RpcTransactionResponse,
        unc_jsonrpc_primitives::types::transactions::RpcTransactionError,
    > {
        if request_data.wait_until == TxExecutionStatus::None {
            self.send_tx_async(request_data).await;
            return Ok(RpcTransactionResponse {
                final_execution_outcome: None,
                final_execution_status: TxExecutionStatus::None,
            });
        }
        let tx = request_data.signed_transaction;
        match self.send_tx_internal(tx.clone(), false).await? {
            ProcessTxResponse::ValidTx | ProcessTxResponse::RequestRouted => {
                self.tx_status_fetch(
                    unc_jsonrpc_primitives::types::transactions::TransactionInfo::from_signed_tx(tx.clone()),
                    request_data.wait_until,
                    false,
                ).await
            }
            network_client_response=> {
                Err(
                    unc_jsonrpc_primitives::types::transactions::RpcTransactionError::from_network_client_responses(
                        network_client_response
                    )
                )
            }
        }
    }

    async fn send_tx_commit(
        &self,
        request_data: unc_jsonrpc_primitives::types::transactions::RpcSendTransactionRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::transactions::RpcTransactionResponse,
        unc_jsonrpc_primitives::types::transactions::RpcTransactionError,
    > {
        self.send_tx(RpcSendTransactionRequest {
            signed_transaction: request_data.signed_transaction,
            wait_until: TxExecutionStatus::Final,
        })
        .await
    }

    async fn health(
        &self,
    ) -> Result<
        unc_jsonrpc_primitives::types::status::RpcHealthResponse,
        unc_jsonrpc_primitives::types::status::RpcStatusError,
    > {
        let status = self.client_send(Status { is_health_check: true, detailed: false }).await?;
        Ok(status.rpc_into())
    }

    pub async fn status(
        &self,
    ) -> Result<
        unc_jsonrpc_primitives::types::status::RpcStatusResponse,
        unc_jsonrpc_primitives::types::status::RpcStatusError,
    > {
        let status = self.client_send(Status { is_health_check: false, detailed: false }).await?;
        Ok(status.rpc_into())
    }

    pub async fn old_debug(
        &self,
    ) -> Result<
        Option<unc_jsonrpc_primitives::types::status::RpcStatusResponse>,
        unc_jsonrpc_primitives::types::status::RpcStatusError,
    > {
        if self.enable_debug_rpc {
            let status =
                self.client_send(Status { is_health_check: false, detailed: true }).await?;
            Ok(Some(status.rpc_into()))
        } else {
            Ok(None)
        }
    }

    pub async fn debug(
        &self,
        path: &str,
    ) -> Result<
        Option<unc_jsonrpc_primitives::types::status::RpcDebugStatusResponse>,
        unc_jsonrpc_primitives::types::status::RpcStatusError,
    > {
        if self.enable_debug_rpc {
            let debug_status: unc_jsonrpc_primitives::types::status::DebugStatusResponse =
                match path {
                    "/debug/api/tracked_shards" => {
                        self.client_send(DebugStatus::TrackedShards).await?.rpc_into()
                    }
                    "/debug/api/sync_status" => {
                        self.client_send(DebugStatus::SyncStatus).await?.rpc_into()
                    }
                    "/debug/api/catchup_status" => {
                        self.client_send(DebugStatus::CatchupStatus).await?.rpc_into()
                    }
                    "/debug/api/epoch_info" => {
                        self.client_send(DebugStatus::EpochInfo).await?.rpc_into()
                    }
                    "/debug/api/block_status" => {
                        self.client_send(DebugStatus::BlockStatus(None)).await?.rpc_into()
                    }
                    "/debug/api/validator_status" => {
                        self.client_send(DebugStatus::ValidatorStatus).await?.rpc_into()
                    }
                    "/debug/api/chain_processing_status" => {
                        self.client_send(DebugStatus::ChainProcessingStatus).await?.rpc_into()
                    }
                    "/debug/api/requested_state_parts" => {
                        self.client_send(DebugStatus::RequestedStateParts).await?.rpc_into()
                    }
                    "/debug/api/peer_store" => self
                        .peer_manager_send(unc_network::debug::GetDebugStatus::PeerStore)
                        .await?
                        .rpc_into(),
                    "/debug/api/network_graph" => self
                        .peer_manager_send(unc_network::debug::GetDebugStatus::Graph)
                        .await?
                        .rpc_into(),
                    "/debug/api/recent_outbound_connections" => self
                        .peer_manager_send(
                            unc_network::debug::GetDebugStatus::RecentOutboundConnections,
                        )
                        .await?
                        .rpc_into(),
                    "/debug/api/network_routes" => self
                        .peer_manager_send(unc_network::debug::GetDebugStatus::Routes)
                        .await?
                        .rpc_into(),
                    "/debug/api/snapshot_hosts" => self
                        .peer_manager_send(unc_network::debug::GetDebugStatus::SnapshotHosts)
                        .await?
                        .rpc_into(),
                    "/debug/api/split_store_info" => {
                        let split_storage_info: RpcSplitStorageInfoResponse = self
                            .split_storage_info(RpcSplitStorageInfoRequest {})
                            .await
                            .map_err(|e| e.into_rpc_status_error())?;
                        unc_jsonrpc_primitives::types::status::DebugStatusResponse::SplitStoreStatus(split_storage_info.result)
                    }
                    _ => return Ok(None),
                };
            Ok(Some(unc_jsonrpc_primitives::types::status::RpcDebugStatusResponse {
                status_response: debug_status,
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn debug_block_status(
        &self,
        starting_height: Option<BlockHeight>,
    ) -> Result<
        Option<unc_jsonrpc_primitives::types::status::RpcDebugStatusResponse>,
        unc_jsonrpc_primitives::types::status::RpcStatusError,
    > {
        if self.enable_debug_rpc {
            let debug_status =
                self.client_send(DebugStatus::BlockStatus(starting_height)).await?.rpc_into();
            Ok(Some(unc_jsonrpc_primitives::types::status::RpcDebugStatusResponse {
                status_response: debug_status,
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn protocol_config(
        &self,
        request_data: unc_jsonrpc_primitives::types::config::RpcProtocolConfigRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::config::RpcProtocolConfigResponse,
        unc_jsonrpc_primitives::types::config::RpcProtocolConfigError,
    > {
        let config_view =
            self.view_client_send(GetProtocolConfig(request_data.block_reference)).await?;
        Ok(RpcProtocolConfigResponse { config_view })
    }

    async fn query(
        &self,
        request_data: unc_jsonrpc_primitives::types::query::RpcQueryRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::query::RpcQueryResponse,
        unc_jsonrpc_primitives::types::query::RpcQueryError,
    > {
        let query_response = self
            .view_client_send(Query::new(request_data.block_reference, request_data.request))
            .await?;
        Ok(query_response.rpc_into())
    }

    async fn tx_status_common(
        &self,
        request_data: unc_jsonrpc_primitives::types::transactions::RpcTransactionStatusRequest,
        fetch_receipt: bool,
    ) -> Result<
        unc_jsonrpc_primitives::types::transactions::RpcTransactionResponse,
        unc_jsonrpc_primitives::types::transactions::RpcTransactionError,
    > {
        let tx_status = self
            .tx_status_fetch(request_data.transaction_info, request_data.wait_until, fetch_receipt)
            .await?;
        Ok(tx_status.rpc_into())
    }

    async fn get_provider(
        &self,
        request_data: unc_jsonrpc_primitives::types::provider::RpcProviderRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::provider::RpcProviderResponse,
        unc_jsonrpc_primitives::types::provider::RpcProviderError,
    > {
        let provider_account = self.view_client_send(GetProvider(request_data.epoch_id, request_data.block_height)).await?;
        Ok(unc_jsonrpc_primitives::types::provider::RpcProviderResponse{ provider_account })
    }

    async fn block(
        &self,
        request_data: unc_jsonrpc_primitives::types::blocks::RpcBlockRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::blocks::RpcBlockResponse,
        unc_jsonrpc_primitives::types::blocks::RpcBlockError,
    > {
        let block_view = self.view_client_send(GetBlock(request_data.block_reference)).await?;
        Ok(unc_jsonrpc_primitives::types::blocks::RpcBlockResponse { block_view })
    }

    async fn chunk(
        &self,
        request_data: unc_jsonrpc_primitives::types::chunks::RpcChunkRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::chunks::RpcChunkResponse,
        unc_jsonrpc_primitives::types::chunks::RpcChunkError,
    > {
        let chunk_view =
            self.view_client_send(GetChunk::rpc_from(request_data.chunk_reference)).await?;
        Ok(unc_jsonrpc_primitives::types::chunks::RpcChunkResponse { chunk_view })
    }

    async fn receipt(
        &self,
        request_data: unc_jsonrpc_primitives::types::receipts::RpcReceiptRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::receipts::RpcReceiptResponse,
        unc_jsonrpc_primitives::types::receipts::RpcReceiptError,
    > {
        match self
            .view_client_send(GetReceipt { receipt_id: request_data.receipt_reference.receipt_id })
            .await?
        {
            Some(receipt_view) => {
                Ok(unc_jsonrpc_primitives::types::receipts::RpcReceiptResponse { receipt_view })
            }
            None => {
                Err(unc_jsonrpc_primitives::types::receipts::RpcReceiptError::UnknownReceipt {
                    receipt_id: request_data.receipt_reference.receipt_id,
                })
            }
        }
    }

    async fn changes_in_block(
        &self,
        request: unc_jsonrpc_primitives::types::changes::RpcStateChangesInBlockRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::changes::RpcStateChangesInBlockByTypeResponse,
        unc_jsonrpc_primitives::types::changes::RpcStateChangesError,
    > {
        let block: unc_primitives::views::BlockView =
            self.view_client_send(GetBlock(request.block_reference)).await?;

        let block_hash = block.header.hash;
        let changes = self.view_client_send(GetStateChangesInBlock { block_hash }).await?;

        Ok(unc_jsonrpc_primitives::types::changes::RpcStateChangesInBlockByTypeResponse {
            block_hash: block.header.hash,
            changes,
        })
    }

    async fn changes_in_block_by_type(
        &self,
        request: unc_jsonrpc_primitives::types::changes::RpcStateChangesInBlockByTypeRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::changes::RpcStateChangesInBlockResponse,
        unc_jsonrpc_primitives::types::changes::RpcStateChangesError,
    > {
        let block: unc_primitives::views::BlockView =
            self.view_client_send(GetBlock(request.block_reference)).await?;

        let block_hash = block.header.hash;
        let changes = self
            .view_client_send(GetStateChanges {
                block_hash,
                state_changes_request: request.state_changes_request,
            })
            .await?;

        Ok(unc_jsonrpc_primitives::types::changes::RpcStateChangesInBlockResponse {
            block_hash: block.header.hash,
            changes,
        })
    }

    async fn next_light_client_block(
        &self,
        request: unc_jsonrpc_primitives::types::light_client::RpcLightClientNextBlockRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::light_client::RpcLightClientNextBlockResponse,
        unc_jsonrpc_primitives::types::light_client::RpcLightClientNextBlockError,
    > {
        let response = self
            .view_client_send(GetNextLightClientBlock { last_block_hash: request.last_block_hash })
            .await?;
        Ok(response.rpc_into())
    }

    async fn light_client_execution_outcome_proof(
        &self,
        request: unc_jsonrpc_primitives::types::light_client::RpcLightClientExecutionProofRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::light_client::RpcLightClientExecutionProofResponse,
        unc_jsonrpc_primitives::types::light_client::RpcLightClientProofError,
    > {
        let unc_jsonrpc_primitives::types::light_client::RpcLightClientExecutionProofRequest {
            id,
            light_client_head,
        } = request;

        let execution_outcome_proof: unc_client_primitives::types::GetExecutionOutcomeResponse =
            self.view_client_send(GetExecutionOutcome { id }).await?;

        let block_proof: unc_client_primitives::types::GetBlockProofResponse = self
            .view_client_send(GetBlockProof {
                block_hash: execution_outcome_proof.outcome_proof.block_hash,
                head_block_hash: light_client_head,
            })
            .await?;

        Ok(unc_jsonrpc_primitives::types::light_client::RpcLightClientExecutionProofResponse {
            outcome_proof: execution_outcome_proof.outcome_proof,
            outcome_root_proof: execution_outcome_proof.outcome_root_proof,
            block_header_lite: block_proof.block_header_lite,
            block_proof: block_proof.proof,
        })
    }

    async fn network_info(
        &self,
    ) -> Result<
        unc_jsonrpc_primitives::types::network_info::RpcNetworkInfoResponse,
        unc_jsonrpc_primitives::types::network_info::RpcNetworkInfoError,
    > {
        let network_info = self.client_send(GetNetworkInfo {}).await?;
        Ok(network_info.rpc_into())
    }

    async fn gas_price(
        &self,
        request_data: unc_jsonrpc_primitives::types::gas_price::RpcGasPriceRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::gas_price::RpcGasPriceResponse,
        unc_jsonrpc_primitives::types::gas_price::RpcGasPriceError,
    > {
        let gas_price_view =
            self.view_client_send(GetGasPrice { block_id: request_data.block_id }).await?;
        Ok(unc_jsonrpc_primitives::types::gas_price::RpcGasPriceResponse { gas_price_view })
    }

    async fn validators(
        &self,
        request_data: unc_jsonrpc_primitives::types::validator::RpcValidatorRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::validator::RpcValidatorResponse,
        unc_jsonrpc_primitives::types::validator::RpcValidatorError,
    > {
        let validator_info = self
            .view_client_send(GetValidatorInfo { epoch_reference: request_data.epoch_reference })
            .await?;
        Ok(unc_jsonrpc_primitives::types::validator::RpcValidatorResponse { validator_info })
    }

    /// Returns the current epoch validators ordered in the block producer order with repetition.
    /// This endpoint is solely used for bridge currently and is not intended for other external use
    /// cases.
    async fn validators_ordered(
        &self,
        request: unc_jsonrpc_primitives::types::validator::RpcValidatorsOrderedRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::validator::RpcValidatorsOrderedResponse,
        unc_jsonrpc_primitives::types::validator::RpcValidatorError,
    > {
        let unc_jsonrpc_primitives::types::validator::RpcValidatorsOrderedRequest { block_id } =
            request;
        let validators = self.view_client_send(GetValidatorOrdered { block_id }).await?;
        Ok(validators)
    }

    /// If experimental_debug_pages_src_path config is set, reads the html file from that
    /// directory. Otherwise, returns None.
    fn read_html_file_override(&self, html_file: &'static str) -> Option<String> {
        if let Some(directory) = &self.debug_pages_src_path {
            let path = directory.join(html_file);
            return Some(std::fs::read_to_string(path.clone()).unwrap_or_else(|err| {
                format!("Could not load path {}: {:?}", path.display(), err)
            }));
        }
        None
    }

    /// Returns the future windows for maintenance in current epoch for the specified account
    /// In the maintenance windows, the node will not be block producer or chunk producer
    async fn maintenance_windows(
        &self,
        request: unc_jsonrpc_primitives::types::maintenance::RpcMaintenanceWindowsRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::maintenance::RpcMaintenanceWindowsResponse,
        unc_jsonrpc_primitives::types::maintenance::RpcMaintenanceWindowsError,
    > {
        let unc_jsonrpc_primitives::types::maintenance::RpcMaintenanceWindowsRequest {
            account_id,
        } = request;
        let windows = self.view_client_send(GetMaintenanceWindows { account_id }).await?;
        Ok(windows.iter().map(|r| (r.start, r.end)).collect())
    }

    async fn client_config(
        &self,
    ) -> Result<
        unc_jsonrpc_primitives::types::client_config::RpcClientConfigResponse,
        unc_jsonrpc_primitives::types::client_config::RpcClientConfigError,
    > {
        let client_config = self.client_send(GetClientConfig {}).await?;
        Ok(unc_jsonrpc_primitives::types::client_config::RpcClientConfigResponse { client_config })
    }

    pub async fn split_storage_info(
        &self,
        _request_data: unc_jsonrpc_primitives::types::split_storage::RpcSplitStorageInfoRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::split_storage::RpcSplitStorageInfoResponse,
        unc_jsonrpc_primitives::types::split_storage::RpcSplitStorageInfoError,
    > {
        let split_storage = self.view_client_send(GetSplitStorageInfo {}).await?;
        Ok(RpcSplitStorageInfoResponse { result: split_storage })
    }
}

#[cfg(feature = "sandbox")]
impl JsonRpcHandler {
    async fn sandbox_patch_state(
        &self,
        patch_state_request: unc_jsonrpc_primitives::types::sandbox::RpcSandboxPatchStateRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::sandbox::RpcSandboxPatchStateResponse,
        unc_jsonrpc_primitives::types::sandbox::RpcSandboxPatchStateError,
    > {
        self.client_addr
            .send(
                unc_client_primitives::types::SandboxMessage::SandboxPatchState(
                    patch_state_request.records,
                )
                .with_span_context(),
            )
            .await
            .map_err(RpcFrom::rpc_from)?;

        timeout(self.polling_config.polling_timeout, async {
            loop {
                let patch_state_finished = self
                    .client_addr
                    .send(
                        unc_client_primitives::types::SandboxMessage::SandboxPatchStateStatus {}
                            .with_span_context(),
                    )
                    .await;
                if let Ok(
                    unc_client_primitives::types::SandboxResponse::SandboxPatchStateFinished(true),
                ) = patch_state_finished
                {
                    break;
                }
                let _ = sleep(self.polling_config.polling_interval).await;
            }
        })
        .await
        .expect("patch state should happen at next block, never timeout");

        Ok(unc_jsonrpc_primitives::types::sandbox::RpcSandboxPatchStateResponse {})
    }

    async fn sandbox_fast_forward(
        &self,
        fast_forward_request: unc_jsonrpc_primitives::types::sandbox::RpcSandboxFastForwardRequest,
    ) -> Result<
        unc_jsonrpc_primitives::types::sandbox::RpcSandboxFastForwardResponse,
        unc_jsonrpc_primitives::types::sandbox::RpcSandboxFastForwardError,
    > {
        use unc_client_primitives::types::SandboxResponse;

        self.client_addr
            .send(
                unc_client_primitives::types::SandboxMessage::SandboxFastForward(
                    fast_forward_request.delta_height,
                )
                .with_span_context(),
            )
            .await
            .map_err(RpcFrom::rpc_from)?;

        // Hard limit the request to timeout at an hour, since fast forwarding can take a while,
        // where we can leave it to the rpc clients to set their own timeouts if necessary.
        timeout(Duration::from_secs(60 * 60), async {
            loop {
                let fast_forward_finished = self
                    .client_addr
                    .send(
                        unc_client_primitives::types::SandboxMessage::SandboxFastForwardStatus {}
                            .with_span_context(),
                    )
                    .await;

                match fast_forward_finished {
                    Ok(SandboxResponse::SandboxFastForwardFinished(true)) => break,
                    Ok(SandboxResponse::SandboxFastForwardFailed(err)) => return Err(err),
                    _ => (),
                }

                let _ = sleep(self.polling_config.polling_interval).await;
            }
            Ok(())
        })
        .await
        .map_err(|_| {
            unc_jsonrpc_primitives::types::sandbox::RpcSandboxFastForwardError::InternalError {
                error_message: "sandbox failed to fast forward within reasonable time of an hour"
                    .to_string(),
            }
        })?
        .map_err(|err| {
            unc_jsonrpc_primitives::types::sandbox::RpcSandboxFastForwardError::InternalError {
                error_message: format!("sandbox failed to fast forward due to: {:?}", err),
            }
        })?;

        Ok(unc_jsonrpc_primitives::types::sandbox::RpcSandboxFastForwardResponse {})
    }
}

#[cfg(feature = "test_features")]
impl JsonRpcHandler {
    async fn adv_disable_header_sync(&self, _params: Value) -> Result<Value, RpcError> {
        actix::spawn(
            self.client_addr
                .send(
                    unc_client::NetworkAdversarialMessage::AdvDisableHeaderSync
                        .with_span_context(),
                )
                .map(|_| ()),
        );
        actix::spawn(
            self.view_client_addr
                .send(
                    unc_client::NetworkAdversarialMessage::AdvDisableHeaderSync
                        .with_span_context(),
                )
                .map(|_| ()),
        );
        Ok(Value::String(String::new()))
    }

    async fn adv_disable_doomslug(&self, _params: Value) -> Result<Value, RpcError> {
        actix::spawn(
            self.client_addr
                .send(
                    unc_client::NetworkAdversarialMessage::AdvDisableDoomslug.with_span_context(),
                )
                .map(|_| ()),
        );
        actix::spawn(
            self.view_client_addr
                .send(
                    unc_client::NetworkAdversarialMessage::AdvDisableDoomslug.with_span_context(),
                )
                .map(|_| ()),
        );
        Ok(Value::String(String::new()))
    }

    async fn adv_produce_blocks(&self, params: Value) -> Result<Value, RpcError> {
        let (num_blocks, only_valid) = crate::api::Params::parse(params)?;
        actix::spawn(
            self.client_addr
                .send(
                    unc_client::NetworkAdversarialMessage::AdvProduceBlocks(
                        num_blocks, only_valid,
                    )
                    .with_span_context(),
                )
                .map(|_| ()),
        );
        Ok(Value::String(String::new()))
    }

    async fn adv_switch_to_height(&self, params: Value) -> Result<Value, RpcError> {
        let (height,) = crate::api::Params::parse(params)?;
        actix::spawn(
            self.client_addr
                .send(
                    unc_client::NetworkAdversarialMessage::AdvSwitchToHeight(height)
                        .with_span_context(),
                )
                .map(|_| ()),
        );
        actix::spawn(
            self.view_client_addr
                .send(
                    unc_client::NetworkAdversarialMessage::AdvSwitchToHeight(height)
                        .with_span_context(),
                )
                .map(|_| ()),
        );
        Ok(Value::String(String::new()))
    }

    async fn adv_get_saved_blocks(&self, _params: Value) -> Result<Value, RpcError> {
        match self
            .client_addr
            .send(unc_client::NetworkAdversarialMessage::AdvGetSavedBlocks.with_span_context())
            .await
        {
            Ok(result) => match result {
                Some(value) => serialize_response(value),
                None => Err(RpcError::server_error::<String>(None)),
            },
            _ => Err(RpcError::server_error::<String>(None)),
        }
    }

    async fn adv_check_store(&self, _params: Value) -> Result<Value, RpcError> {
        match self
            .client_addr
            .send(
                unc_client::NetworkAdversarialMessage::AdvCheckStorageConsistency
                    .with_span_context(),
            )
            .await
        {
            Ok(result) => match result {
                Some(value) => serialize_response(value),
                None => Err(RpcError::server_error::<String>(None)),
            },
            _ => Err(RpcError::server_error::<String>(None)),
        }
    }
}

fn rpc_handler(
    message: web::Json<Message>,
    handler: web::Data<JsonRpcHandler>,
) -> impl Future<Output = Result<HttpResponse, HttpError>> {
    let response = async move {
        let message = handler.process(message.0).await?;
        Ok(HttpResponse::Ok().json(&message))
    };
    response.boxed()
}

fn status_handler(
    handler: web::Data<JsonRpcHandler>,
) -> impl Future<Output = Result<HttpResponse, HttpError>> {
    metrics::HTTP_STATUS_REQUEST_COUNT.inc();

    let response = async move {
        match handler.status().await {
            Ok(value) => Ok(HttpResponse::Ok().json(&value)),
            Err(_) => Ok(HttpResponse::ServiceUnavailable().finish()),
        }
    };
    response.boxed()
}

async fn debug_handler(
    req: HttpRequest,
    handler: web::Data<JsonRpcHandler>,
) -> Result<HttpResponse, HttpError> {
    if req.path() == "/debug/api/status" {
        // This is a temporary workaround - as we migrate the debug information to the separate class below.
        return match handler.old_debug().await {
            Ok(Some(value)) => Ok(HttpResponse::Ok().json(&value)),
            Ok(None) => Ok(HttpResponse::MethodNotAllowed().finish()),
            Err(_) => Ok(HttpResponse::ServiceUnavailable().finish()),
        };
    }
    match handler.debug(req.path()).await {
        Ok(Some(value)) => Ok(HttpResponse::Ok().json(&value)),
        Ok(None) => Ok(HttpResponse::MethodNotAllowed().finish()),
        Err(_) => Ok(HttpResponse::ServiceUnavailable().finish()),
    }
}

async fn handle_entity_debug(
    req: web::Json<EntityQuery>,
    handler: web::Data<JsonRpcHandler>,
) -> Result<HttpResponse, HttpError> {
    match handler.entity_debug_handler.query(req.0) {
        Ok(value) => Ok(HttpResponse::Ok().json(&value)),
        Err(err) => Ok(HttpResponse::ServiceUnavailable().body(format!("{:?}", err))),
    }
}

async fn debug_block_status_handler(
    path: web::Path<u64>,
    handler: web::Data<JsonRpcHandler>,
) -> Result<HttpResponse, HttpError> {
    match handler.debug_block_status(Some(*path)).await {
        Ok(Some(value)) => Ok(HttpResponse::Ok().json(&value)),
        Ok(None) => Ok(HttpResponse::MethodNotAllowed().finish()),
        Err(_) => Ok(HttpResponse::ServiceUnavailable().finish()),
    }
}

fn health_handler(
    handler: web::Data<JsonRpcHandler>,
) -> impl Future<Output = Result<HttpResponse, HttpError>> {
    let response = async move {
        match handler.health().await {
            Ok(value) => Ok(HttpResponse::Ok().json(&value)),
            Err(_) => Ok(HttpResponse::ServiceUnavailable().finish()),
        }
    };
    response.boxed()
}

fn network_info_handler(
    handler: web::Data<JsonRpcHandler>,
) -> impl Future<Output = Result<HttpResponse, HttpError>> {
    let response = async move {
        match handler.network_info().await {
            Ok(value) => Ok(HttpResponse::Ok().json(&value)),
            Err(_) => Ok(HttpResponse::ServiceUnavailable().finish()),
        }
    };
    response.boxed()
}

pub async fn prometheus_handler() -> Result<HttpResponse, HttpError> {
    metrics::PROMETHEUS_REQUEST_COUNT.inc();

    let mut buffer = vec![];
    let encoder = TextEncoder::new();
    encoder.encode(&prometheus::gather(), &mut buffer).unwrap();

    match String::from_utf8(buffer) {
        Ok(text) => Ok(HttpResponse::Ok().body(text)),
        Err(_) => Ok(HttpResponse::ServiceUnavailable().finish()),
    }
}

fn client_config_handler(
    handler: web::Data<JsonRpcHandler>,
) -> impl Future<Output = Result<HttpResponse, HttpError>> {
    let response = async move {
        match handler.client_config().await {
            Ok(value) => Ok(HttpResponse::Ok().json(&value)),
            Err(_) => Ok(HttpResponse::ServiceUnavailable().finish()),
        }
    };
    response.boxed()
}

fn get_cors(cors_allowed_origins: &[String]) -> Cors {
    let mut cors = Cors::permissive();
    if cors_allowed_origins != ["*".to_string()] {
        for origin in cors_allowed_origins {
            cors = cors.allowed_origin(origin);
        }
    }
    cors.allowed_methods(vec!["GET", "POST"])
        .allowed_headers(vec![http::header::AUTHORIZATION, http::header::ACCEPT])
        .allowed_header(http::header::CONTENT_TYPE)
        .max_age(3600)
}

macro_rules! debug_page_string {
    ($html_file: literal, $handler: expr) => {
        $handler
            .read_html_file_override($html_file)
            .unwrap_or_else(|| include_str!(concat!("../res/", $html_file)).to_string())
    };
}

#[get("/debug")]
async fn debug_html(
    handler: web::Data<JsonRpcHandler>,
) -> actix_web::Result<impl actix_web::Responder> {
    Ok(HttpResponse::Ok().body(debug_page_string!("debug.html", handler)))
}

#[get("/debug/pages/{page}")]
async fn display_debug_html(
    path: web::Path<(String,)>,
    handler: web::Data<JsonRpcHandler>,
) -> actix_web::Result<impl actix_web::Responder> {
    let page_name = path.into_inner().0;

    let content = match page_name.as_str() {
        "last_blocks" => Some(debug_page_string!("last_blocks.html", handler)),
        "last_blocks.css" => Some(debug_page_string!("last_blocks.css", handler)),
        "last_blocks.js" => Some(debug_page_string!("last_blocks.js", handler)),
        "network_info" => Some(debug_page_string!("network_info.html", handler)),
        "network_info.css" => Some(debug_page_string!("network_info.css", handler)),
        "network_info.js" => Some(debug_page_string!("network_info.js", handler)),
        "tier1_network_info" => Some(debug_page_string!("tier1_network_info.html", handler)),
        "epoch_info" => Some(debug_page_string!("epoch_info.html", handler)),
        "epoch_info.css" => Some(debug_page_string!("epoch_info.css", handler)),
        "chain_n_chunk_info" => Some(debug_page_string!("chain_n_chunk_info.html", handler)),
        "chain_n_chunk_info.css" => Some(debug_page_string!("chain_n_chunk_info.css", handler)),
        "sync" => Some(debug_page_string!("sync.html", handler)),
        "sync.css" => Some(debug_page_string!("sync.css", handler)),
        "validator" => Some(debug_page_string!("validator.html", handler)),
        "validator.css" => Some(debug_page_string!("validator.css", handler)),
        "split_store" => Some(debug_page_string!("split_store.html", handler)),
        _ => None,
    };

    match content {
        Some(content) => {
            Ok(HttpResponse::Ok().insert_header(header::ContentType::html()).body(content))
        }
        None => Ok(HttpResponse::NotFound().finish()),
    }
}

/// Starts HTTP server(s) listening for RPC requests.
///
/// Starts an HTTP server which handles JSON RPC calls as well as states
/// endpoints such as `/status`, `/health`, `/metrics` etc.  Depending on
/// configuration may also start another HTTP server just for providing
/// Prometheus metrics (i.e. covering the `/metrics` path).
///
/// Returns a vector of servers that have been started.  Each server is returned
/// as a tuple containing a name of the server (e.g. `"JSON RPC"`) which can be
/// used in diagnostic messages and a [`actix_web::dev::Server`] object which
/// can be used to control the server (most notably stop it).
pub fn start_http(
    config: RpcConfig,
    genesis_config: GenesisConfig,
    client_addr: Addr<ClientActor>,
    view_client_addr: Addr<ViewClientActor>,
    peer_manager_addr: Option<Addr<PeerManagerActor>>,
    entity_debug_handler: Arc<dyn EntityDebugHandler>,
) -> Vec<(&'static str, actix_web::dev::ServerHandle)> {
    let RpcConfig {
        addr,
        prometheus_addr,
        cors_allowed_origins,
        polling_config,
        limits_config,
        enable_debug_rpc,
        experimental_debug_pages_src_path: debug_pages_src_path,
    } = config;
    let prometheus_addr = prometheus_addr.filter(|it| it != &addr.to_string());
    let cors_allowed_origins_clone = cors_allowed_origins.clone();
    info!(target:"network", "Starting http server at {}", addr);
    let mut servers = Vec::new();
    let listener = HttpServer::new(move || {
        App::new()
            .wrap(get_cors(&cors_allowed_origins))
            .app_data(web::Data::new(JsonRpcHandler {
                client_addr: client_addr.clone(),
                view_client_addr: view_client_addr.clone(),
                peer_manager_addr: peer_manager_addr.clone(),
                polling_config,
                genesis_config: genesis_config.clone(),
                enable_debug_rpc,
                debug_pages_src_path: debug_pages_src_path.clone().map(Into::into),
                entity_debug_handler: entity_debug_handler.clone(),
            }))
            .app_data(web::JsonConfig::default().limit(limits_config.json_payload_max_size))
            .wrap(middleware::Logger::default())
            .service(web::resource("/").route(web::post().to(rpc_handler)))
            .service(
                web::resource("/status")
                    .route(web::get().to(status_handler))
                    .route(web::head().to(status_handler)),
            )
            .service(
                web::resource("/health")
                    .route(web::get().to(health_handler))
                    .route(web::head().to(health_handler)),
            )
            .service(web::resource("/network_info").route(web::get().to(network_info_handler)))
            .service(web::resource("/metrics").route(web::get().to(prometheus_handler)))
            .service(web::resource("/debug/api/entity").route(web::post().to(handle_entity_debug)))
            .service(web::resource("/debug/api/{api}").route(web::get().to(debug_handler)))
            .service(
                web::resource("/debug/api/block_status/{starting_height}")
                    .route(web::get().to(debug_block_status_handler)),
            )
            .service(
                web::resource("/debug/client_config").route(web::get().to(client_config_handler)),
            )
            .service(debug_html)
            .service(display_debug_html)
    });

    match listener.listen(addr.std_listener().unwrap()) {
        std::result::Result::Ok(s) => {
            let server = s.workers(4).shutdown_timeout(5).disable_signals().run();
            servers.push(("JSON RPC", server.handle()));
            tokio::spawn(server);
        }
        std::result::Result::Err(e) => {
            error!(
                target:"network",
                "Could not start http server at {} due to {:?}", &addr, e,
            )
        }
    };

    if let Some(prometheus_addr) = prometheus_addr {
        info!(target:"network", "Starting http monitoring server at {}", prometheus_addr);
        // Export only the /metrics service. It's a read-only service and can have very relaxed
        // access restrictions.
        let listener = HttpServer::new(move || {
            App::new()
                .wrap(get_cors(&cors_allowed_origins_clone))
                .wrap(middleware::Logger::default())
                .service(web::resource("/metrics").route(web::get().to(prometheus_handler)))
        });

        match listener.bind(&prometheus_addr) {
            std::result::Result::Ok(s) => {
                let server = s.workers(2).shutdown_timeout(5).disable_signals().run();
                servers.push(("Prometheus Metrics", server.handle()));
                tokio::spawn(server);
            }
            std::result::Result::Err(e) => {
                error!(
                    target:"network",
                    "Can't export Prometheus metrics at {} due to {:?}", &prometheus_addr, e,
                )
            }
        };
    }

    servers
}
