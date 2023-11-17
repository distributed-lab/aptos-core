// Copyright © Aptos Foundation

use crate::{response_dispatcher::ResponseDispatcher, RequestMetadata, SERVICE_TYPE};
use aptos_indexer_grpc_data_access::{
    access_trait::{StorageReadError, StorageReadStatus, StorageTransactionRead},
    StorageClient,
};
use aptos_indexer_grpc_utils::{
    chunk_transactions,
    constants::MESSAGE_SIZE_LIMIT,
    counters::{
        IndexerGrpcStep, DURATION_IN_SECS, LATEST_PROCESSED_VERSION, NUM_TRANSACTIONS_COUNT,
        TRANSACTION_UNIX_TIMESTAMP,
    },
    time_diff_since_pb_timestamp_in_secs, timestamp_to_unixtime,
};
use aptos_protos::{indexer::v1::TransactionsResponse, util::timestamp::Timestamp};
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tonic::Status;

// The server will retry to send the response to the client and give up after RESPONSE_CHANNEL_SEND_TIMEOUT.
// This is to prevent the server from being occupied by a slow client.
const RESPONSE_CHANNEL_SEND_TIMEOUT: Duration = Duration::from_secs(120);
// Number of retries for fetching responses from upstream.
const FETCH_RETRY_COUNT: usize = 100;
const RETRY_BACKOFF_IN_MS: u64 = 500;
const NOT_AVAILABLE_RETRY_BACKOFF_IN_MS: u64 = 10;
const WAIT_TIME_BEFORE_CLOUSING_IN_MS: u64 = 60_000;
const RESPONSE_DISPATCH_NAME: &str = "GrpcResponseDispatcher";

pub struct GrpcResponseDispatcher {
    next_version_to_process: u64,
    transaction_count: Option<u64>,
    sender: Sender<Result<TransactionsResponse, Status>>,
    storages: Vec<StorageClient>,
    sender_capacity: usize,
    request_metadata: RequestMetadata,
}

impl GrpcResponseDispatcher {
    // Fetches the next batch of responses from storage.
    // This is a stateless function that only fetches from storage based on current state.
    async fn fetch_from_storages(
        &self,
    ) -> Result<(Vec<TransactionsResponse>, usize), StorageReadError> {
        if let Some(transaction_count) = self.transaction_count {
            if transaction_count == 0 {
                return Ok((vec![], 0));
            }
        }
        // Loop to wait for the next storage to be available.
        let mut previous_storage_not_found = false;
        loop {
            if self.sender.is_closed() {
                return Err(StorageReadError::PermenantError(
                    RESPONSE_DISPATCH_NAME,
                    anyhow::anyhow!("Sender is closed."),
                ));
            }
            for (index, storage) in self.storages.as_slice().iter().enumerate() {
                let metadata = storage.get_metadata().await?;
                match storage
                    .get_transactions(self.next_version_to_process, None)
                    .await
                {
                    Ok(StorageReadStatus::Ok(transactions)) => {
                        let responses = chunk_transactions(transactions, MESSAGE_SIZE_LIMIT);
                        return Ok((
                            responses
                                .into_iter()
                                .map(|transactions| TransactionsResponse {
                                    transactions,
                                    chain_id: Some(metadata.chain_id),
                                })
                                .collect(),
                            index,
                        ));
                    },
                    Ok(StorageReadStatus::NotAvailableYet) => {
                        // This is fatal; it means previous storage evicts the data before the current storage has it.
                        if previous_storage_not_found {
                            return Err(StorageReadError::PermenantError(
                                RESPONSE_DISPATCH_NAME,
                                anyhow::anyhow!("Gap detected between storages."),
                            ));
                        }
                        // If the storage is not available yet, retry the storages.
                        tokio::time::sleep(Duration::from_millis(
                            NOT_AVAILABLE_RETRY_BACKOFF_IN_MS,
                        ))
                        .await;
                        break;
                    },
                    Ok(StorageReadStatus::NotFound) => {
                        // Continue to the next storage.
                        previous_storage_not_found = true;
                        continue;
                    },
                    Err(e) => {
                        return Err(e);
                    },
                }
            }

            if previous_storage_not_found {
                return Err(StorageReadError::PermenantError(
                    RESPONSE_DISPATCH_NAME,
                    anyhow::anyhow!("Gap detected between storages."),
                ));
            }
        }
    }

    // Based on the response from fetch_from_storages, verify and dispatch the response, and update the state.
    async fn fetch_internal(&mut self) -> Result<Vec<TransactionsResponse>, StorageReadError> {
        // TODO: add retry to TransientError.
        let start_time = std::time::Instant::now();
        let (responses, index) = self.fetch_from_storages().await?;
        // Verify no empty response.
        if responses.iter().any(|v| v.transactions.is_empty()) {
            return Err(StorageReadError::TransientError(
                RESPONSE_DISPATCH_NAME,
                anyhow::anyhow!("Empty responses from storages."),
            ));
        }
        if responses.is_empty() {
            // End of finite stream.
            return Ok(responses);
        }

        let start_version_txn_latency = time_diff_since_pb_timestamp_in_secs(
            responses
                .first()
                .unwrap()
                .transactions
                .first()
                .unwrap()
                .timestamp
                .as_ref()
                .unwrap_or(&Timestamp::default()),
        );
        let end_version_txn_latency = time_diff_since_pb_timestamp_in_secs(
            responses
                .last()
                .unwrap()
                .transactions
                .last()
                .unwrap()
                .timestamp
                .as_ref()
                .unwrap_or(&Timestamp::default()),
        );
        let start_version_timestamp = responses
            .first()
            .unwrap()
            .transactions
            .first()
            .unwrap()
            .timestamp
            .clone();
        // Verify responses are consecutive and sequential.
        let mut version = self.next_version_to_process;
        let starting_version = version;
        for response in responses.iter() {
            for transaction in response.transactions.iter() {
                if transaction.version != version {
                    return Err(StorageReadError::TransientError(
                        RESPONSE_DISPATCH_NAME,
                        anyhow::anyhow!("Version mismatch in response."),
                    ));
                }
                // move to the next version.
                version += 1;
            }
        }
        let mut processed_responses = vec![];
        if let Some(transaction_count) = self.transaction_count {
            // If transactions_count is specified, truncate if necessary.
            let mut current_transaction_count = 0;
            for response in responses.into_iter() {
                if current_transaction_count == transaction_count {
                    break;
                }
                let current_response_size = response.transactions.len() as u64;
                if current_transaction_count + current_response_size > transaction_count {
                    let remaining_transaction_count = transaction_count - current_transaction_count;
                    let truncated_transactions = response
                        .transactions
                        .into_iter()
                        .take(remaining_transaction_count as usize)
                        .collect();
                    processed_responses.push(TransactionsResponse {
                        transactions: truncated_transactions,
                        chain_id: response.chain_id,
                    });
                    current_transaction_count += remaining_transaction_count;
                } else {
                    processed_responses.push(response);
                    current_transaction_count += current_response_size;
                }
            }
            self.transaction_count = Some(transaction_count - current_transaction_count);
        } else {
            // If not, continue to fetch.
            processed_responses = responses;
        }
        let processed_transactions_count = processed_responses
            .iter()
            .map(|v| v.transactions.len())
            .sum::<usize>() as u64;
        self.next_version_to_process += processed_transactions_count;
        // Hack: assume we don't directly fetch from redis.
        let (step, label) = match index {
            0 => (
                IndexerGrpcStep::DataServiceDataFetchedMemory.get_step(),
                IndexerGrpcStep::DataServiceDataFetchedMemory.get_label(),
            ),
            _ => (
                IndexerGrpcStep::DataServiceDataFetchedFilestore.get_step(),
                IndexerGrpcStep::DataServiceDataFetchedFilestore.get_label(),
            ),
        };
        tracing::info!(
            start_version = starting_version,
            end_version = starting_version + processed_transactions_count - 1,
            start_version_txn_latency,
            end_version_txn_latency,
            num_of_transactions = processed_transactions_count,
            duration_in_secs = start_time.elapsed().as_secs_f64(),
            connection_id = self.request_metadata.connection_id.as_str(),
            service_type = SERVICE_TYPE,
            step,
            "{}",
            label
        );

        LATEST_PROCESSED_VERSION
            .with_label_values(&[SERVICE_TYPE, step, label])
            .set(self.next_version_to_process as i64);
        NUM_TRANSACTIONS_COUNT
            .with_label_values(&[SERVICE_TYPE, step, label])
            .set(processed_transactions_count as i64);
        DURATION_IN_SECS
            .with_label_values(&[SERVICE_TYPE, step, label])
            .set(start_time.elapsed().as_secs_f64());
        TRANSACTION_UNIX_TIMESTAMP
            .with_label_values(&[SERVICE_TYPE, step, label])
            .set(
                start_version_timestamp
                    .map(|t| timestamp_to_unixtime(&t))
                    .unwrap_or_default(),
            );

        Ok(processed_responses)
    }
}

#[async_trait::async_trait]
impl ResponseDispatcher for GrpcResponseDispatcher {
    fn new(
        starting_version: u64,
        transaction_count: Option<u64>,
        sender: Sender<Result<TransactionsResponse, Status>>,
        storages: &[StorageClient],
        request_metadata: RequestMetadata,
    ) -> Self {
        let sender_capacity = sender.capacity();
        Self {
            next_version_to_process: starting_version,
            transaction_count,
            sender,
            sender_capacity,
            storages: storages.to_vec(),
            request_metadata,
        }
    }

    async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            match self.fetch_with_retries().await {
                Ok(responses) => {
                    if responses.is_empty() {
                        break;
                    }
                    for response in responses {
                        self.dispatch(Ok(response)).await?;
                    }
                },
                Err(status) => {
                    self.dispatch(Err(status)).await?;
                    anyhow::bail!("Failed to fetch transactions from storages.");
                },
            }
        }
        // We don't want to close the channel immediately before all the finite items are sent.s
        if self.transaction_count.is_some() {
            let start_time = std::time::Instant::now();
            loop {
                if start_time.elapsed().as_millis() > WAIT_TIME_BEFORE_CLOUSING_IN_MS as u128 {
                    break;
                }
                if self.sender.capacity() == self.sender_capacity {
                    // Sender is empty now; no need to wait.
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        }
        Ok(())
    }

    async fn fetch_with_retries(&mut self) -> anyhow::Result<Vec<TransactionsResponse>, Status> {
        for _ in 0..FETCH_RETRY_COUNT {
            match self.fetch_internal().await {
                Ok(responses) => {
                    return Ok(responses);
                },
                Err(StorageReadError::TransientError(s, _e)) => {
                    tracing::warn!("Failed to fetch transactions from storage: {:#}", s);
                    tokio::time::sleep(Duration::from_millis(RETRY_BACKOFF_IN_MS)).await;
                    continue;
                },
                Err(StorageReadError::PermenantError(s, _e)) => Err(Status::internal(format!(
                    "Failed to fetch transactions from storages, {:}",
                    s
                )))?,
            }
        }
        Err(Status::internal(
            "Failed to fetch transactions from storages.",
        ))
    }

    async fn dispatch(
        &mut self,
        response: Result<TransactionsResponse, Status>,
    ) -> anyhow::Result<()> {
        let start_time = std::time::Instant::now();
        let first_version_opt = response
            .as_ref()
            .ok()
            .map(|v| v.transactions.first().unwrap().version);
        let end_version_opt = response
            .as_ref()
            .ok()
            .map(|v| v.transactions.last().unwrap().version);
        let start_version_txn_latency = response.as_ref().ok().map(|v| {
            time_diff_since_pb_timestamp_in_secs(
                v.transactions
                    .first()
                    .unwrap()
                    .timestamp
                    .as_ref()
                    .unwrap_or(&Timestamp::default()),
            )
        });
        let end_version_txn_latency = response.as_ref().ok().map(|v| {
            time_diff_since_pb_timestamp_in_secs(
                v.transactions
                    .last()
                    .unwrap()
                    .timestamp
                    .as_ref()
                    .unwrap_or(&Timestamp::default()),
            )
        });
        let first_version_timestamp_opt = response
            .as_ref()
            .ok()
            .map(|v| v.transactions.first().unwrap().timestamp.clone().unwrap());
        let num_of_transactions_opt = response.as_ref().ok().map(|v| v.transactions.len());
        let step = IndexerGrpcStep::DataServiceChunkSent.get_step();
        let label = IndexerGrpcStep::DataServiceChunkSent.get_label();
        match self
            .sender
            .send_timeout(response, RESPONSE_CHANNEL_SEND_TIMEOUT)
            .await
        {
            Ok(_) => {
                if let Some(first_version) = first_version_opt {
                    tracing::info!(
                        start_version = first_version,
                        end_version = end_version_opt.unwrap(),
                        start_version_txn_latency = start_version_txn_latency.unwrap(),
                        end_version_txn_latency = end_version_txn_latency.unwrap(),
                        num_of_transactions = num_of_transactions_opt.unwrap(),
                        duration_in_secs = start_time.elapsed().as_secs_f64(),
                        connection_id = self.request_metadata.connection_id.as_str(),
                        service_type = SERVICE_TYPE,
                        step = step,
                        "{}",
                        label
                    );

                    DURATION_IN_SECS
                        .with_label_values(&[SERVICE_TYPE, step, label])
                        .set(start_time.elapsed().as_secs_f64());
                    TRANSACTION_UNIX_TIMESTAMP
                        .with_label_values(&[SERVICE_TYPE, step, label])
                        .set(timestamp_to_unixtime(
                            first_version_timestamp_opt.as_ref().unwrap(),
                        ));
                    NUM_TRANSACTIONS_COUNT
                        .with_label_values(&[SERVICE_TYPE, step, label])
                        .set(num_of_transactions_opt.unwrap() as i64);
                    LATEST_PROCESSED_VERSION
                        .with_label_values(&[SERVICE_TYPE, step, label])
                        .set(end_version_opt.unwrap() as i64);
                }
            },
            Err(e) => {
                tracing::warn!("Failed to send response to downstream: {:#}", e);
                return Err(anyhow::anyhow!("Failed to send response to downstream."));
            },
        };
        Ok(())
    }
}

impl Drop for GrpcResponseDispatcher {
    fn drop(&mut self) {
        tracing::info!(
            request_email = self.request_metadata.request_email.as_str(),
            request_api_key_name = self.request_metadata.request_api_key_name.as_str(),
            processor_name = self.request_metadata.processor_name.as_str(),
            connection_id = self.request_metadata.connection_id.as_str(),
            request_user_classification = self.request_metadata.user_classification.as_str(),
            service_type = SERVICE_TYPE,
            "[Data Service] Client disconnected."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aptos_indexer_grpc_data_access::MockStorageClient;
    use aptos_protos::transaction::v1::Transaction;
    fn create_transactions(starting_version: u64, size: usize) -> Vec<Transaction> {
        let mut transactions = vec![];
        for i in 0..size {
            transactions.push(Transaction {
                version: starting_version + i as u64,
                ..Default::default()
            });
        }
        transactions
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_finite_stream() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(100);
        let res = tokio::spawn(async move {
            let first_storage_transactions = create_transactions(20, 100);
            let second_storage_transactions = create_transactions(10, 20);
            let third_storage_transactions = create_transactions(0, 15);
            let storages = vec![
                StorageClient::MockClient(MockStorageClient::new(1, first_storage_transactions)),
                StorageClient::MockClient(MockStorageClient::new(2, second_storage_transactions)),
                StorageClient::MockClient(MockStorageClient::new(3, third_storage_transactions)),
            ];
            let mut dispatcher = GrpcResponseDispatcher::new(
                0,
                Some(40),
                sender,
                storages.as_slice(),
                RequestMetadata::default(),
            );
            let run_result = dispatcher.run().await;
            assert!(run_result.is_ok());
        });

        let mut transactions = vec![];
        while let Some(response) = receiver.recv().await {
            for transaction in response.unwrap().transactions {
                transactions.push(transaction);
            }
        }
        assert_eq!(transactions.len(), 40);
        for (current_version, t) in transactions.into_iter().enumerate() {
            assert_eq!(t.version, current_version as u64);
        }
        res.await.expect("Dispatch thread should exit.");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_storages_gap() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(100);
        let res = tokio::spawn(async move {
            let first_storage_transactions = create_transactions(30, 100);
            let second_storage_transactions = create_transactions(10, 10);
            let storages = vec![
                StorageClient::MockClient(MockStorageClient::new(1, first_storage_transactions)),
                StorageClient::MockClient(MockStorageClient::new(2, second_storage_transactions)),
            ];
            let mut dispatcher = GrpcResponseDispatcher::new(
                15,
                Some(30),
                sender,
                storages.as_slice(),
                RequestMetadata::default(),
            );
            let run_result = dispatcher.run().await;
            assert!(run_result.is_err());
        });

        let first_response = receiver.recv().await.unwrap();
        assert!(first_response.is_ok());
        let transactions_response = first_response.unwrap();
        assert!(transactions_response.transactions.len() == 5);
        let second_response = receiver.recv().await.unwrap();
        // Gap is detected.
        assert!(second_response.is_err());
        res.await.expect("Dispatch thread should exit.");
    }

    // This test is to make sure dispatch doesn't leak memory.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_infinite_stream_with_client_closure() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(100);
        let task_result = tokio::spawn(async move {
            let first_storage_transactions = create_transactions(20, 20);
            let second_storage_transactions = create_transactions(10, 30);
            let third_storage_transactions = create_transactions(0, 15);
            let storages = vec![
                StorageClient::MockClient(MockStorageClient::new(1, first_storage_transactions)),
                StorageClient::MockClient(MockStorageClient::new(2, second_storage_transactions)),
                StorageClient::MockClient(MockStorageClient::new(3, third_storage_transactions)),
            ];
            let mut dispatcher = GrpcResponseDispatcher::new(
                0,
                None,
                sender,
                storages.as_slice(),
                RequestMetadata::default(),
            );
            dispatcher.run().await
        });
        // Let the dispatcher run for 1 second.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let first_peek = receiver.try_recv();
        // transactions 0 - 15
        assert!(first_peek.is_ok());
        let first_response = first_peek.unwrap();
        assert!(first_response.is_ok());
        let transactions_response = first_response.unwrap();
        assert!(transactions_response.transactions.len() == 15);
        let second_peek = receiver.try_recv();
        // transactions 15 - 40
        assert!(second_peek.is_ok());
        let second_response = second_peek.unwrap();
        assert!(second_response.is_ok());
        let transactions_response = second_response.unwrap();
        assert!(transactions_response.transactions.len() == 25);
        let third_peek = receiver.try_recv();
        match third_peek {
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {},
            _ => unreachable!("This is not possible."),
        }
        // Drop the receiver to close the channel.
        drop(receiver);
        let task_result = task_result.await;

        // The task should finish successfully.
        assert!(task_result.is_ok());
        let task_result = task_result.unwrap();
        // The dispatcher thread should exit with error.
        assert!(task_result.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_not_found_in_all_storages() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(100);
        let res = tokio::spawn(async move {
            let first_storage_transactions = create_transactions(20, 100);
            let storages = vec![StorageClient::MockClient(MockStorageClient::new(
                1,
                first_storage_transactions,
            ))];
            let mut dispatcher = GrpcResponseDispatcher::new(
                0,
                Some(40),
                sender,
                storages.as_slice(),
                RequestMetadata::default(),
            );
            let run_result = dispatcher.run().await;
            assert!(run_result.is_err());
        });

        let first_response = receiver.recv().await.unwrap();
        assert!(first_response.is_err());
        res.await.expect("Dispatch thread should exit.");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_back_pressure_from_client_should_error() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let res = tokio::spawn(async move {
            let first_storage_transactions = create_transactions(200, 200);
            let second_storage_transactions = create_transactions(0, 200);
            let storages = vec![
                StorageClient::MockClient(MockStorageClient::new(1, first_storage_transactions)),
                StorageClient::MockClient(MockStorageClient::new(1, second_storage_transactions)),
            ];
            let mut dispatcher = GrpcResponseDispatcher::new(
                0,
                None,
                sender,
                storages.as_slice(),
                RequestMetadata::default(),
            );
            let run_result = dispatcher.run().await;
            assert!(run_result.is_err());
        });
        // Let the dispatcher run for 1 second.
        tokio::time::sleep(std::time::Duration::from_secs(130)).await;
        // First the channel is full, then the sender is closed.
        let first_response = receiver.recv().await.unwrap();
        assert!(first_response.is_ok());
        res.await.expect("Dispatch thread should exit.");
    }
}
