use std::fmt::Display;
use std::string::FromUtf8Error;
use std::time::Duration;

use actix::{Addr, MailboxError};
use actix_cors::Cors;
use actix_web::{http, middleware, web, App, Error as HttpError, HttpResponse, HttpServer};
use borsh::BorshDeserialize;
use futures::Future;
use futures::{FutureExt, TryFutureExt};
use prometheus;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::{sleep, timeout};

use near_chain_configs::GenesisConfig;
use near_client::{
    ClientActor, GetBlock, GetBlockProof, GetChunk, GetExecutionOutcome, GetGasPrice,
    GetNetworkInfo, GetNextLightClientBlock, GetProtocolConfig, GetReceipt, GetStateChanges,
    GetStateChangesInBlock, GetValidatorInfo, GetValidatorOrdered, Query, Status, TxStatus,
    TxStatusError, ViewClientActor,
};
pub use near_jsonrpc_client as client;
use near_jsonrpc_primitives::errors::RpcError;
use near_jsonrpc_primitives::message::{Message, Request};
use near_jsonrpc_primitives::rpc::{
    RpcBroadcastTxSyncResponse, RpcLightClientExecutionProofRequest,
    RpcLightClientExecutionProofResponse, RpcStateChangesInBlockRequest,
    RpcStateChangesInBlockResponse, RpcStateChangesRequest, RpcStateChangesResponse,
    RpcValidatorsOrderedRequest, TransactionInfo,
};
use near_jsonrpc_primitives::types::config::RpcProtocolConfigResponse;
use near_metrics::{Encoder, TextEncoder};
#[cfg(feature = "adversarial")]
use near_network::types::{NetworkAdversarialMessage, NetworkViewClientMessages};
use near_network::{NetworkClientMessages, NetworkClientResponses};
use near_primitives::errors::{InvalidTxError, TxExecutionError};
use near_primitives::hash::CryptoHash;
use near_primitives::serialize::{from_base64, BaseEncode};
use near_primitives::transaction::SignedTransaction;
use near_primitives::types::{AccountId, MaybeBlockId};
use near_primitives::views::{FinalExecutionOutcomeView, FinalExecutionOutcomeViewEnum};
use near_runtime_utils::is_valid_account_id;

mod metrics;

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RpcLimitsConfig {
    /// Maximum byte size of the json payload.
    pub json_payload_max_size: usize,
}

impl Default for RpcLimitsConfig {
    fn default() -> Self {
        Self { json_payload_max_size: 10 * 1024 * 1024 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RpcConfig {
    pub addr: String,
    pub cors_allowed_origins: Vec<String>,
    pub polling_config: RpcPollingConfig,
    #[serde(default)]
    pub limits_config: RpcLimitsConfig,
}

impl Default for RpcConfig {
    fn default() -> Self {
        RpcConfig {
            addr: "0.0.0.0:3030".to_owned(),
            cors_allowed_origins: vec!["*".to_owned()],
            polling_config: Default::default(),
            limits_config: Default::default(),
        }
    }
}

impl RpcConfig {
    pub fn new(addr: &str) -> Self {
        RpcConfig { addr: addr.to_owned(), ..Default::default() }
    }
}

fn from_base64_or_parse_err(encoded: String) -> Result<Vec<u8>, RpcError> {
    from_base64(&encoded).map_err(|err| RpcError::parse_error(err.to_string()))
}

fn parse_params<T: DeserializeOwned>(value: Option<Value>) -> Result<T, RpcError> {
    if let Some(value) = value {
        serde_json::from_value(value)
            .map_err(|err| RpcError::invalid_params(format!("Failed parsing args: {}", err)))
    } else {
        Err(RpcError::invalid_params("Require at least one parameter".to_owned()))
    }
}

fn jsonify<T: serde::Serialize>(
    response: Result<Result<T, String>, MailboxError>,
) -> Result<Value, RpcError> {
    response
        .map_err(|err| err.to_string())
        .and_then(|value| {
            value.and_then(|value| serde_json::to_value(value).map_err(|err| err.to_string()))
        })
        .map_err(|err| RpcError::server_error(Some(err)))
}

fn parse_tx(params: Option<Value>) -> Result<SignedTransaction, RpcError> {
    let (encoded,) = parse_params::<(String,)>(params)?;
    let bytes = from_base64_or_parse_err(encoded)?;
    SignedTransaction::try_from_slice(&bytes)
        .map_err(|e| RpcError::invalid_params(format!("Failed to decode transaction: {}", e)))
}

/// A general Server Error
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, near_rpc_error_macro::RpcError)]
pub enum ServerError {
    TxExecutionError(TxExecutionError),
    Timeout,
    Closed,
    InternalError,
}

impl Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            ServerError::TxExecutionError(e) => write!(f, "ServerError: {}", e),
            ServerError::Timeout => write!(f, "ServerError: Timeout"),
            ServerError::Closed => write!(f, "ServerError: Closed"),
            ServerError::InternalError => write!(f, "ServerError: Internal Error"),
        }
    }
}

impl From<InvalidTxError> for ServerError {
    fn from(e: InvalidTxError) -> ServerError {
        ServerError::TxExecutionError(TxExecutionError::InvalidTxError(e))
    }
}

impl From<MailboxError> for ServerError {
    fn from(e: MailboxError) -> Self {
        match e {
            MailboxError::Closed => ServerError::Closed,
            MailboxError::Timeout => ServerError::Timeout,
        }
    }
}

impl From<ServerError> for RpcError {
    fn from(e: ServerError) -> RpcError {
        RpcError::server_error(Some(e))
    }
}

fn timeout_err() -> RpcError {
    RpcError::server_error(Some(ServerError::Timeout))
}

struct JsonRpcHandler {
    client_addr: Addr<ClientActor>,
    view_client_addr: Addr<ViewClientActor>,
    polling_config: RpcPollingConfig,
    genesis_config: GenesisConfig,
}

impl JsonRpcHandler {
    pub async fn process(&self, message: Message) -> Result<Message, HttpError> {
        let id = message.id();
        match message {
            Message::Request(request) => {
                Ok(Message::response(id, self.process_request(request).await))
            }
            _ => Ok(Message::error(RpcError::invalid_request())),
        }
    }

    async fn process_request(&self, request: Request) -> Result<Value, RpcError> {
        near_metrics::inc_counter_vec(&metrics::HTTP_RPC_REQUEST_COUNT, &[request.method.as_ref()]);
        let _rpc_processing_time = near_metrics::start_timer_vec(
            &metrics::RPC_PROCESSING_TIME,
            &[request.method.as_ref()],
        );

        #[cfg(feature = "adversarial")]
        {
            let params = request.params.clone();

            let res = match request.method.as_ref() {
                // Adversarial controls
                "adv_set_weight" => Some(self.adv_set_sync_info(params).await),
                "adv_disable_header_sync" => Some(self.adv_disable_header_sync(params).await),
                "adv_disable_doomslug" => Some(self.adv_disable_doomslug(params).await),
                "adv_produce_blocks" => Some(self.adv_produce_blocks(params).await),
                "adv_switch_to_height" => Some(self.adv_switch_to_height(params).await),
                "adv_get_saved_blocks" => Some(self.adv_get_saved_blocks(params).await),
                "adv_check_store" => Some(self.adv_check_store(params).await),
                _ => None,
            };

            if let Some(res) = res {
                return res;
            }
        }

        let response: Result<Value, RpcError> = match request.method.as_ref() {
            // Handlers ordered alphabetically
            "block" => {
                let rpc_block_request =
                    near_jsonrpc_primitives::types::blocks::RpcBlockRequest::parse(request.params)?;
                let block = self.block(rpc_block_request).await?;
                serde_json::to_value(block).map_err(|err| RpcError::parse_error(err.to_string()))
            }
            "broadcast_tx_async" => self.send_tx_async(request.params).await,
            "broadcast_tx_commit" => self.send_tx_commit(request.params).await,
            "chunk" => {
                let rpc_chunk_request =
                    near_jsonrpc_primitives::types::chunks::RpcChunkRequest::parse(request.params)?;
                let chunk = self.chunk(rpc_chunk_request).await?;
                serde_json::to_value(chunk).map_err(|err| RpcError::parse_error(err.to_string()))
            }
            "EXPERIMENTAL_broadcast_tx_sync" => self.send_tx_sync(request.params).await,
            "EXPERIMENTAL_changes" => self.changes_in_block_by_type(request.params).await,
            "EXPERIMENTAL_changes_in_block" => self.changes_in_block(request.params).await,
            "EXPERIMENTAL_check_tx" => self.check_tx(request.params).await,
            "EXPERIMENTAL_genesis_config" => self.genesis_config().await,
            "EXPERIMENTAL_light_client_proof" => {
                self.light_client_execution_outcome_proof(request.params).await
            }
            "EXPERIMENTAL_protocol_config" => {
                let rpc_protocol_config_request =
                    near_jsonrpc_primitives::types::config::RpcProtocolConfigRequest::parse(
                        request.params,
                    )?;
                let config = self.protocol_config(rpc_protocol_config_request).await?;
                serde_json::to_value(config).map_err(|err| RpcError::parse_error(err.to_string()))
            }
            "EXPERIMENTAL_receipt" => {
                let rpc_receipt_request =
                    near_jsonrpc_primitives::types::receipts::RpcReceiptRequest::parse(
                        request.params,
                    )?;
                let receipt = self.receipt(rpc_receipt_request).await?;
                serde_json::to_value(receipt).map_err(|err| RpcError::parse_error(err.to_string()))
            }
            "EXPERIMENTAL_tx_status" => self.tx_status_common(request.params, true).await,
            "EXPERIMENTAL_validators_ordered" => self.validators_ordered(request.params).await,
            "gas_price" => self.gas_price(request.params).await,
            "health" => self.health().await,
            "light_client_proof" => self.light_client_execution_outcome_proof(request.params).await,
            "next_light_client_block" => self.next_light_client_block(request.params).await,
            "network_info" => self.network_info().await,
            "query" => {
                let rpc_query_request =
                    near_jsonrpc_primitives::types::query::RpcQueryRequest::parse(request.params)?;
                let query_response = self.query(rpc_query_request).await?;
                serde_json::to_value(query_response)
                    .map_err(|err| RpcError::parse_error(err.to_string()))
            }
            "status" => self.status().await,
            "tx" => self.tx_status_common(request.params, false).await,
            "validators" => self.validators(request.params).await,
            _ => Err(RpcError::method_not_found(request.method.clone())),
        };

        if let Err(err) = &response {
            near_metrics::inc_counter_vec(
                &metrics::RPC_ERROR_COUNT,
                &[request.method.as_ref(), &err.code.to_string()],
            );
        }

        response
    }

    async fn send_tx_async(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let tx = parse_tx(params)?;
        let hash = (&tx.get_hash()).to_base();
        self.client_addr.do_send(NetworkClientMessages::Transaction {
            transaction: tx,
            is_forwarded: false,
            check_only: false,
        });
        Ok(Value::String(hash))
    }

    async fn tx_exists(
        &self,
        tx_hash: CryptoHash,
        signer_account_id: &AccountId,
    ) -> Result<bool, ServerError> {
        timeout(self.polling_config.polling_timeout, async {
            loop {
                // TODO(optimization): Introduce a view_client method to only get transaction
                // status without the information about execution outcomes.
                match self
                    .view_client_addr
                    .send(TxStatus {
                        tx_hash,
                        signer_account_id: signer_account_id.clone(),
                        fetch_receipt: false,
                    })
                    .await
                {
                    Ok(Ok(Some(_))) => {
                        return Ok(true);
                    }
                    Ok(Err(TxStatusError::MissingTransaction(_))) => {
                        return Ok(false);
                    }
                    Err(_) => return Err(ServerError::InternalError),
                    _ => {}
                }
                sleep(self.polling_config.polling_interval).await;
            }
        })
        .await
        .map_err(|_| {
            near_metrics::inc_counter(&metrics::RPC_TIMEOUT_TOTAL);
            ServerError::Timeout
        })?
    }

    async fn tx_status_fetch(
        &self,
        tx_info: TransactionInfo,
        fetch_receipt: bool,
    ) -> Result<FinalExecutionOutcomeViewEnum, TxStatusError> {
        let (tx_hash, account_id) = match &tx_info {
            TransactionInfo::Transaction(tx) => (tx.get_hash(), tx.transaction.signer_id.clone()),
            TransactionInfo::TransactionId { hash, account_id } => (*hash, account_id.clone()),
        };
        timeout(self.polling_config.polling_timeout, async {
            loop {
                let tx_status_result = self
                    .view_client_addr
                    .send(TxStatus {
                        tx_hash,
                        signer_account_id: account_id.clone(),
                        fetch_receipt,
                    })
                    .await;
                match tx_status_result {
                    Ok(Ok(Some(outcome))) => break Ok(outcome),
                    Ok(Ok(None)) => {}
                    Ok(Err(err @ TxStatusError::MissingTransaction(_))) => {
                        if let TransactionInfo::Transaction(tx) = &tx_info {
                            if let Ok(NetworkClientResponses::InvalidTx(e)) =
                                self.send_tx(tx.clone(), true).await
                            {
                                break Err(TxStatusError::InvalidTx(e));
                            }
                        }
                        break Err(err);
                    }
                    Ok(Err(err)) => break Err(err),
                    Err(_) => break Err(TxStatusError::InternalError),
                }
                let _ = sleep(self.polling_config.polling_interval).await;
            }
        })
        .await
        .map_err(|_| {
            near_metrics::inc_counter(&metrics::RPC_TIMEOUT_TOTAL);
            TxStatusError::TimeoutError
        })?
    }

    async fn tx_polling(&self, tx_info: TransactionInfo) -> Result<Value, RpcError> {
        timeout(self.polling_config.polling_timeout, async {
            loop {
                match self.tx_status_fetch(tx_info.clone(), false).await {
                    Ok(tx_status) => break jsonify(Ok(Ok(tx_status))),
                    // If transaction is missing, keep polling.
                    Err(TxStatusError::MissingTransaction(_)) => {}
                    // If we hit any other error, we return to the user.
                    Err(err) => {
                        break jsonify::<FinalExecutionOutcomeView>(Ok(Err(err.into())));
                    }
                }
                let _ = sleep(self.polling_config.polling_interval).await;
            }
        })
        .await
        .map_err(|_| {
            near_metrics::inc_counter(&metrics::RPC_TIMEOUT_TOTAL);
            timeout_err()
        })?
    }

    /// Send a transaction idempotently (subsequent send of the same transaction will not cause
    /// any new side-effects and the result will be the same unless we garbage collected it
    /// already).
    async fn send_tx(
        &self,
        tx: SignedTransaction,
        check_only: bool,
    ) -> Result<NetworkClientResponses, RpcError> {
        let tx_hash = tx.get_hash();
        let signer_account_id = tx.transaction.signer_id.clone();
        let response = self
            .client_addr
            .send(NetworkClientMessages::Transaction {
                transaction: tx,
                is_forwarded: false,
                check_only,
            })
            .map_err(|err| RpcError::server_error(Some(ServerError::from(err))))
            .await?;

        // If we receive InvalidNonce error, it might be the case that the transaction was
        // resubmitted, and we should check if that is the case and return ValidTx response to
        // maintain idempotence of the send_tx method.
        if let NetworkClientResponses::InvalidTx(
            near_primitives::errors::InvalidTxError::InvalidNonce { .. },
        ) = response
        {
            if self.tx_exists(tx_hash, &signer_account_id).await? {
                return Ok(NetworkClientResponses::ValidTx);
            }
        }

        Ok(response)
    }

    async fn send_tx_sync(&self, params: Option<Value>) -> Result<Value, RpcError> {
        self.send_or_check_tx(params, false).await
    }

    async fn check_tx(&self, params: Option<Value>) -> Result<Value, RpcError> {
        self.send_or_check_tx(params, true).await
    }

    async fn send_or_check_tx(
        &self,
        params: Option<Value>,
        check_only: bool,
    ) -> Result<Value, RpcError> {
        let tx = parse_tx(params)?;
        let tx_hash = (&tx.get_hash()).to_base();
        let does_not_track_shard_err =
            "Node doesn't track this shard. Cannot determine whether the transaction is valid";
        match self.send_tx(tx, check_only).await? {
            NetworkClientResponses::ValidTx => {
                if check_only {
                    Ok(Value::Null)
                } else {
                    jsonify(Ok(Ok(RpcBroadcastTxSyncResponse {
                        transaction_hash: tx_hash,
                        is_routed: false,
                    })))
                }
            }
            NetworkClientResponses::RequestRouted => {
                if check_only {
                    Err(RpcError::server_error(Some(does_not_track_shard_err.to_string())))
                } else {
                    jsonify(Ok(Ok(RpcBroadcastTxSyncResponse {
                        transaction_hash: tx_hash,
                        is_routed: true,
                    })))
                }
            }
            NetworkClientResponses::InvalidTx(err) => {
                Err(RpcError::server_error(Some(ServerError::TxExecutionError(err.into()))))
            }
            NetworkClientResponses::DoesNotTrackShard => {
                Err(RpcError::server_error(Some(does_not_track_shard_err.to_string())))
            }
            _ => {
                // this is only possible if something went wrong with the node internally.
                Err(RpcError::server_error(Some(ServerError::InternalError)))
            }
        }
    }

    async fn send_tx_commit(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let tx = parse_tx(params)?;
        match self.tx_status_fetch(TransactionInfo::Transaction(tx.clone()), false).await {
            Ok(outcome) => {
                return jsonify(Ok(Ok(outcome)));
            }
            Err(TxStatusError::InvalidTx(e)) => {
                return Err(RpcError::server_error(Some(ServerError::TxExecutionError(e.into()))));
            }
            _ => {}
        }
        match self.send_tx(tx.clone(), false).await? {
            NetworkClientResponses::ValidTx | NetworkClientResponses::RequestRouted => {
                self.tx_polling(TransactionInfo::Transaction(tx)).await
            }
            NetworkClientResponses::InvalidTx(err) => {
                Err(RpcError::server_error(Some(ServerError::TxExecutionError(err.into()))))
            }
            NetworkClientResponses::NoResponse => {
                Err(RpcError::server_error(Some(ServerError::Timeout)))
            }
            _ => Err(RpcError::server_error(Some(ServerError::InternalError))),
        }
    }

    async fn health(&self) -> Result<Value, RpcError> {
        match self.client_addr.send(Status { is_health_check: true }).await {
            Ok(Ok(_)) => Ok(Value::Null),
            Ok(Err(err)) => Err(RpcError::new(-32_001, err, None)),
            Err(_) => Err(RpcError::server_error::<()>(None)),
        }
    }

    pub async fn status(&self) -> Result<Value, RpcError> {
        match self.client_addr.send(Status { is_health_check: false }).await {
            Ok(Ok(result)) => jsonify(Ok(Ok(result))),
            Ok(Err(err)) => Err(RpcError::new(-32_001, err, None)),
            Err(_) => Err(RpcError::server_error::<()>(None)),
        }
    }

    /// Expose Genesis Config (with internal Runtime Config) without state records to keep the
    /// output at a reasonable size.
    ///
    /// See also `genesis_records` API.
    pub async fn genesis_config(&self) -> Result<Value, RpcError> {
        jsonify(Ok(Ok(&self.genesis_config)))
    }

    pub async fn protocol_config(
        &self,
        request_data: near_jsonrpc_primitives::types::config::RpcProtocolConfigRequest,
    ) -> Result<
        near_jsonrpc_primitives::types::config::RpcProtocolConfigResponse,
        near_jsonrpc_primitives::types::config::RpcProtocolConfigError,
    > {
        let config_view = self
            .view_client_addr
            .send(GetProtocolConfig(request_data.block_reference.into()))
            .await??;
        Ok(RpcProtocolConfigResponse { config_view })
    }

    async fn query(
        &self,
        request_data: near_jsonrpc_primitives::types::query::RpcQueryRequest,
    ) -> Result<
        near_jsonrpc_primitives::types::query::RpcQueryResponse,
        near_jsonrpc_primitives::types::query::RpcQueryError,
    > {
        let query = Query::new(request_data.block_reference, request_data.request);
        let query_response = self.view_client_addr.send(query).await??;
        Ok(near_jsonrpc_primitives::types::query::RpcQueryResponse { query_response })
    }

    async fn tx_status_common(
        &self,
        params: Option<Value>,
        fetch_receipt: bool,
    ) -> Result<Value, RpcError> {
        let tx_status_request =
            if let Ok((hash, account_id)) = parse_params::<(CryptoHash, String)>(params.clone()) {
                if !is_valid_account_id(&account_id) {
                    return Err(RpcError::invalid_params(format!(
                        "Invalid account id: {}",
                        account_id
                    )));
                }
                TransactionInfo::TransactionId { hash, account_id }
            } else {
                let tx = parse_tx(params)?;
                TransactionInfo::Transaction(tx)
            };

        jsonify(Ok(self
            .tx_status_fetch(tx_status_request, fetch_receipt)
            .await
            .map_err(|err| err.into())))
    }

    async fn block(
        &self,
        request_data: near_jsonrpc_primitives::types::blocks::RpcBlockRequest,
    ) -> Result<
        near_jsonrpc_primitives::types::blocks::RpcBlockResponse,
        near_jsonrpc_primitives::types::blocks::RpcBlockError,
    > {
        let block_view =
            self.view_client_addr.send(GetBlock(request_data.block_reference.into())).await??;
        Ok(near_jsonrpc_primitives::types::blocks::RpcBlockResponse { block_view })
    }

    async fn chunk(
        &self,
        request_data: near_jsonrpc_primitives::types::chunks::RpcChunkRequest,
    ) -> Result<
        near_jsonrpc_primitives::types::chunks::RpcChunkResponse,
        near_jsonrpc_primitives::types::chunks::RpcChunkError,
    > {
        let chunk_view =
            self.view_client_addr.send(GetChunk::from(request_data.chunk_reference)).await??;
        Ok(near_jsonrpc_primitives::types::chunks::RpcChunkResponse { chunk_view })
    }

    async fn receipt(
        &self,
        request_data: near_jsonrpc_primitives::types::receipts::RpcReceiptRequest,
    ) -> Result<
        near_jsonrpc_primitives::types::receipts::RpcReceiptResponse,
        near_jsonrpc_primitives::types::receipts::RpcReceiptError,
    > {
        match self
            .view_client_addr
            .send(GetReceipt { receipt_id: request_data.receipt_reference.receipt_id })
            .await??
        {
            Some(receipt_view) => {
                Ok(near_jsonrpc_primitives::types::receipts::RpcReceiptResponse { receipt_view })
            }
            None => Err(near_jsonrpc_primitives::types::receipts::RpcReceiptError::UnknownReceipt(
                request_data.receipt_reference.receipt_id,
            )),
        }
    }

    async fn changes_in_block(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let RpcStateChangesInBlockRequest { block_reference } = parse_params(params)?;
        // TODO refactor it. Changed to keep it working before refactoring
        let result = self.view_client_addr.send(GetBlock(block_reference)).await?;
        let block = match result {
            Ok(block) => block,
            Err(err) => {
                return Err(RpcError::from(
                    near_jsonrpc_primitives::types::blocks::RpcBlockError::from(err),
                ))
            }
        };

        let block_hash = block.header.hash.clone();
        jsonify(self.view_client_addr.send(GetStateChangesInBlock { block_hash }).await.map(|v| {
            v.map(|changes| RpcStateChangesInBlockResponse {
                block_hash: block.header.hash,
                changes,
            })
        }))
    }

    async fn changes_in_block_by_type(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let RpcStateChangesRequest { block_reference, state_changes_request } =
            parse_params(params)?;
        // TODO refactor it. Changed to keep it working before refactoring
        let result = self.view_client_addr.send(GetBlock(block_reference)).await?;
        let block = match result {
            Ok(block) => block,
            Err(err) => {
                return Err(RpcError::from(
                    near_jsonrpc_primitives::types::blocks::RpcBlockError::from(err),
                ))
            }
        };

        let block_hash = block.header.hash.clone();
        jsonify(
            self.view_client_addr
                .send(GetStateChanges { block_hash, state_changes_request })
                .await
                .map(|v| {
                    v.map(|changes| RpcStateChangesResponse {
                        block_hash: block.header.hash,
                        changes,
                    })
                }),
        )
    }

    async fn next_light_client_block(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let (last_block_hash,) = parse_params::<(CryptoHash,)>(params)?;
        jsonify(self.view_client_addr.send(GetNextLightClientBlock { last_block_hash }).await)
    }

    async fn light_client_execution_outcome_proof(
        &self,
        params: Option<Value>,
    ) -> Result<Value, RpcError> {
        let RpcLightClientExecutionProofRequest { id, light_client_head } = parse_params(params)?;
        let execution_outcome_proof = self
            .view_client_addr
            .send(GetExecutionOutcome { id })
            .await
            .map_err(|e| RpcError::from(ServerError::from(e)))?
            .map_err(|e| RpcError::server_error(Some(e)))?;
        let block_proof = self
            .view_client_addr
            .send(GetBlockProof {
                block_hash: execution_outcome_proof.outcome_proof.block_hash,
                head_block_hash: light_client_head,
            })
            .await
            .map_err(|e| RpcError::from(ServerError::from(e)))?;
        let res = block_proof.map(|block_proof| RpcLightClientExecutionProofResponse {
            outcome_proof: execution_outcome_proof.outcome_proof,
            outcome_root_proof: execution_outcome_proof.outcome_root_proof,
            block_header_lite: block_proof.block_header_lite,
            block_proof: block_proof.proof,
        });
        jsonify(Ok(res))
    }

    async fn network_info(&self) -> Result<Value, RpcError> {
        jsonify(self.client_addr.send(GetNetworkInfo {}).await)
    }

    async fn gas_price(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let (block_id,) = parse_params::<(MaybeBlockId,)>(params)?;
        jsonify(self.view_client_addr.send(GetGasPrice { block_id }).await)
    }

    pub async fn metrics(&self) -> Result<String, FromUtf8Error> {
        // Gather metrics and return them as a String
        let mut buffer = vec![];
        let encoder = TextEncoder::new();
        encoder.encode(&prometheus::gather(), &mut buffer).unwrap();

        String::from_utf8(buffer)
    }

    async fn validators(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let (block_id,) = parse_params::<(MaybeBlockId,)>(params)?;
        jsonify(self.view_client_addr.send(GetValidatorInfo { block_id }).await)
    }

    /// Returns the current epoch validators ordered in the block producer order with repetition.
    /// This endpoint is solely used for bridge currently and is not intended for other external use
    /// cases.
    async fn validators_ordered(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let RpcValidatorsOrderedRequest { block_id } =
            parse_params::<RpcValidatorsOrderedRequest>(params)?;
        jsonify(self.view_client_addr.send(GetValidatorOrdered { block_id }).await)
    }
}

#[cfg(feature = "adversarial")]
impl JsonRpcHandler {
    async fn adv_set_sync_info(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let height = parse_params::<u64>(params)?;
        actix::spawn(
            self.view_client_addr
                .send(NetworkViewClientMessages::Adversarial(
                    NetworkAdversarialMessage::AdvSetSyncInfo(height),
                ))
                .map(|_| ()),
        );
        Ok(Value::String("".to_string()))
    }

    async fn adv_disable_header_sync(&self, _params: Option<Value>) -> Result<Value, RpcError> {
        actix::spawn(
            self.client_addr
                .send(NetworkClientMessages::Adversarial(
                    NetworkAdversarialMessage::AdvDisableHeaderSync,
                ))
                .map(|_| ()),
        );
        actix::spawn(
            self.view_client_addr
                .send(NetworkViewClientMessages::Adversarial(
                    NetworkAdversarialMessage::AdvDisableHeaderSync,
                ))
                .map(|_| ()),
        );
        Ok(Value::String("".to_string()))
    }

    async fn adv_disable_doomslug(&self, _params: Option<Value>) -> Result<Value, RpcError> {
        actix::spawn(
            self.client_addr
                .send(NetworkClientMessages::Adversarial(
                    NetworkAdversarialMessage::AdvDisableDoomslug,
                ))
                .map(|_| ()),
        );
        actix::spawn(
            self.view_client_addr
                .send(NetworkViewClientMessages::Adversarial(
                    NetworkAdversarialMessage::AdvDisableDoomslug,
                ))
                .map(|_| ()),
        );
        Ok(Value::String("".to_string()))
    }

    async fn adv_produce_blocks(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let (num_blocks, only_valid) = parse_params::<(u64, bool)>(params)?;
        actix::spawn(
            self.client_addr
                .send(NetworkClientMessages::Adversarial(
                    NetworkAdversarialMessage::AdvProduceBlocks(num_blocks, only_valid),
                ))
                .map(|_| ()),
        );
        Ok(Value::String("".to_string()))
    }

    async fn adv_switch_to_height(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let (height,) = parse_params::<(u64,)>(params)?;
        actix::spawn(
            self.client_addr
                .send(NetworkClientMessages::Adversarial(
                    NetworkAdversarialMessage::AdvSwitchToHeight(height),
                ))
                .map(|_| ()),
        );
        actix::spawn(
            self.view_client_addr
                .send(NetworkViewClientMessages::Adversarial(
                    NetworkAdversarialMessage::AdvSwitchToHeight(height),
                ))
                .map(|_| ()),
        );
        Ok(Value::String("".to_string()))
    }

    async fn adv_get_saved_blocks(&self, _params: Option<Value>) -> Result<Value, RpcError> {
        match self
            .client_addr
            .send(NetworkClientMessages::Adversarial(NetworkAdversarialMessage::AdvGetSavedBlocks))
            .await
        {
            Ok(result) => match result {
                NetworkClientResponses::AdvResult(value) => jsonify(Ok(Ok(value))),
                _ => Err(RpcError::server_error::<String>(None)),
            },
            _ => Err(RpcError::server_error::<String>(None)),
        }
    }

    async fn adv_check_store(&self, _params: Option<Value>) -> Result<Value, RpcError> {
        match self
            .client_addr
            .send(NetworkClientMessages::Adversarial(
                NetworkAdversarialMessage::AdvCheckStorageConsistency,
            ))
            .await
        {
            Ok(result) => match result {
                NetworkClientResponses::AdvResult(value) => jsonify(Ok(Ok(value))),
                _ => Err(RpcError::server_error::<String>(None)),
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
        Ok(HttpResponse::Ok().json(message))
    };
    response.boxed()
}

fn status_handler(
    handler: web::Data<JsonRpcHandler>,
) -> impl Future<Output = Result<HttpResponse, HttpError>> {
    near_metrics::inc_counter(&metrics::HTTP_STATUS_REQUEST_COUNT);

    let response = async move {
        match handler.status().await {
            Ok(value) => Ok(HttpResponse::Ok().json(value)),
            Err(_) => Ok(HttpResponse::ServiceUnavailable().finish()),
        }
    };
    response.boxed()
}

fn health_handler(
    handler: web::Data<JsonRpcHandler>,
) -> impl Future<Output = Result<HttpResponse, HttpError>> {
    let response = async move {
        match handler.health().await {
            Ok(value) => Ok(HttpResponse::Ok().json(value)),
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
            Ok(value) => Ok(HttpResponse::Ok().json(value)),
            Err(_) => Ok(HttpResponse::ServiceUnavailable().finish()),
        }
    };
    response.boxed()
}

fn prometheus_handler(
    handler: web::Data<JsonRpcHandler>,
) -> impl Future<Output = Result<HttpResponse, HttpError>> {
    near_metrics::inc_counter(&metrics::PROMETHEUS_REQUEST_COUNT);

    let response = async move {
        match handler.metrics().await {
            Ok(value) => Ok(HttpResponse::Ok().body(value)),
            Err(_) => Ok(HttpResponse::ServiceUnavailable().finish()),
        }
    };
    response.boxed()
}

fn get_cors(cors_allowed_origins: &[String]) -> Cors {
    let mut cors = Cors::permissive();
    if cors_allowed_origins != ["*".to_string()] {
        for origin in cors_allowed_origins {
            cors = cors.allowed_origin(&origin);
        }
    }
    cors.allowed_methods(vec!["GET", "POST"])
        .allowed_headers(vec![http::header::AUTHORIZATION, http::header::ACCEPT])
        .allowed_header(http::header::CONTENT_TYPE)
        .max_age(3600)
}

pub fn start_http(
    config: RpcConfig,
    genesis_config: GenesisConfig,
    client_addr: Addr<ClientActor>,
    view_client_addr: Addr<ViewClientActor>,
) {
    let RpcConfig { addr, cors_allowed_origins, polling_config, limits_config } = config;
    HttpServer::new(move || {
        App::new()
            .wrap(get_cors(&cors_allowed_origins))
            .data(JsonRpcHandler {
                client_addr: client_addr.clone(),
                view_client_addr: view_client_addr.clone(),
                polling_config,
                genesis_config: genesis_config.clone(),
            })
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
    })
    .bind(addr)
    .unwrap()
    .workers(4)
    .shutdown_timeout(5)
    .run();
}
