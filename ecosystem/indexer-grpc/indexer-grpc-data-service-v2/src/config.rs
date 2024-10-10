// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{
    connection_manager::ConnectionManager,
    historical_data_service::HistoricalDataService,
    live_data_service::LiveDataService,
    service::{DataServiceWrapper, DataServiceWrapperWrapper},
};
use anyhow::Result;
use aptos_indexer_grpc_server_framework::RunnableConfig;
use aptos_indexer_grpc_utils::{
    config::IndexerGrpcFileStoreConfig,
    status_page::{get_throughput_from_samples, render_status_page, Tab},
};
use aptos_protos::{
    indexer::v1::FILE_DESCRIPTOR_SET as INDEXER_V1_FILE_DESCRIPTOR_SET,
    transaction::v1::FILE_DESCRIPTOR_SET as TRANSACTION_V1_TESTING_FILE_DESCRIPTOR_SET,
    util::timestamp::FILE_DESCRIPTOR_SET as UTIL_TIMESTAMP_FILE_DESCRIPTOR_SET,
};
use build_html::{
    Container, ContainerType, HtmlContainer, HtmlElement, HtmlTag, Table, TableCell, TableCellType,
    TableRow,
};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::task::JoinHandle;
use tonic::{codec::CompressionEncoding, transport::Server};
use tracing::info;
use warp::{reply::Response, Rejection};

pub(crate) static LIVE_DATA_SERVICE: OnceCell<LiveDataService<'static>> = OnceCell::new();
pub(crate) static HISTORICAL_DATA_SERVICE: OnceCell<HistoricalDataService> = OnceCell::new();

pub(crate) const MAX_MESSAGE_SIZE: usize = 256 * (1 << 20);

// HTTP2 ping interval and timeout.
// This can help server to garbage collect dead connections.
// tonic server: https://docs.rs/tonic/latest/tonic/transport/server/struct.Server.html#method.http2_keepalive_interval
const HTTP2_PING_INTERVAL_DURATION: std::time::Duration = std::time::Duration::from_secs(60);
const HTTP2_PING_TIMEOUT_DURATION: std::time::Duration = std::time::Duration::from_secs(10);

const DEFAULT_MAX_RESPONSE_CHANNEL_SIZE: usize = 5;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert_path: String,
    pub key_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    /// The address to listen on.
    pub(crate) listen_address: SocketAddr,
    pub(crate) tls_config: Option<TlsConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LiveDataServiceConfig {
    pub enabled: bool,
    #[serde(default = "LiveDataServiceConfig::default_num_slots")]
    pub num_slots: usize,
    #[serde(default = "LiveDataServiceConfig::default_size_limit_bytes")]
    pub size_limit_bytes: usize,
}

impl LiveDataServiceConfig {
    fn default_num_slots() -> usize {
        5_000_000
    }

    fn default_size_limit_bytes() -> usize {
        10_000_000_000
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalDataServiceConfig {
    pub enabled: bool,
    pub file_store_config: IndexerGrpcFileStoreConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexerGrpcDataServiceConfig {
    pub(crate) chain_id: u64,
    pub(crate) service_config: ServiceConfig,
    pub(crate) live_data_service_config: LiveDataServiceConfig,
    pub(crate) historical_data_service_config: HistoricalDataServiceConfig,
    pub(crate) grpc_manager_addresses: Vec<String>,
    pub(crate) self_advertised_address: String,
    #[serde(default = "IndexerGrpcDataServiceConfig::default_data_service_response_channel_size")]
    pub data_service_response_channel_size: usize,
}

impl IndexerGrpcDataServiceConfig {
    const fn default_data_service_response_channel_size() -> usize {
        DEFAULT_MAX_RESPONSE_CHANNEL_SIZE
    }

    async fn create_live_data_service(
        &self,
        tasks: &mut Vec<JoinHandle<Result<()>>>,
    ) -> Option<DataServiceWrapper> {
        if !self.live_data_service_config.enabled {
            return None;
        }
        let connection_manager = Arc::new(
            ConnectionManager::new(
                self.chain_id,
                self.grpc_manager_addresses.clone(),
                self.self_advertised_address.clone(),
                /*is_live_data_service=*/ true,
            )
            .await,
        );
        let (handler_tx, handler_rx) = tokio::sync::mpsc::channel(10);
        let service = DataServiceWrapper::new(
            connection_manager.clone(),
            handler_tx,
            self.data_service_response_channel_size,
            /*is_live_data_service=*/ true,
        );

        let connection_manager_clone = connection_manager.clone();
        tasks.push(tokio::task::spawn(async move {
            connection_manager_clone.start().await;
            Ok(())
        }));

        let chain_id = self.chain_id;
        let config = self.live_data_service_config.clone();
        tasks.push(tokio::task::spawn_blocking(move || {
            LIVE_DATA_SERVICE
                .get_or_init(|| LiveDataService::new(chain_id, config, connection_manager))
                .run(handler_rx);
            Ok(())
        }));

        Some(service)
    }

    async fn create_historical_data_service(
        &self,
        tasks: &mut Vec<JoinHandle<Result<()>>>,
    ) -> Option<DataServiceWrapper> {
        if !self.historical_data_service_config.enabled {
            return None;
        }
        let connection_manager = Arc::new(
            ConnectionManager::new(
                self.chain_id,
                self.grpc_manager_addresses.clone(),
                self.self_advertised_address.clone(),
                /*is_live_data_service=*/ false,
            )
            .await,
        );
        let (handler_tx, handler_rx) = tokio::sync::mpsc::channel(10);
        let service = DataServiceWrapper::new(
            connection_manager.clone(),
            handler_tx,
            self.data_service_response_channel_size,
            /*is_live_data_service=*/ false,
        );

        let connection_manager_clone = connection_manager.clone();
        tasks.push(tokio::task::spawn(async move {
            connection_manager_clone.start().await;
            Ok(())
        }));

        let chain_id = self.chain_id;
        let config = self.historical_data_service_config.clone();
        tasks.push(tokio::task::spawn_blocking(move || {
            HISTORICAL_DATA_SERVICE
                .get_or_init(|| HistoricalDataService::new(chain_id, config, connection_manager))
                .run(handler_rx);
            Ok(())
        }));

        Some(service)
    }
}

#[async_trait::async_trait]
impl RunnableConfig for IndexerGrpcDataServiceConfig {
    async fn run(&self) -> Result<()> {
        let reflection_service = tonic_reflection::server::Builder::configure()
            // Note: It is critical that the file descriptor set is registered for every
            // file that the top level API proto depends on recursively. If you don't,
            // compilation will still succeed but reflection will fail at runtime.
            //
            // TODO: Add a test for this / something in build.rs, this is a big footgun.
            .register_encoded_file_descriptor_set(INDEXER_V1_FILE_DESCRIPTOR_SET)
            .register_encoded_file_descriptor_set(TRANSACTION_V1_TESTING_FILE_DESCRIPTOR_SET)
            .register_encoded_file_descriptor_set(UTIL_TIMESTAMP_FILE_DESCRIPTOR_SET)
            .build_v1alpha()
            .map_err(|e| anyhow::anyhow!("Failed to build reflection service: {}", e))?
            .send_compressed(CompressionEncoding::Zstd)
            .accept_compressed(CompressionEncoding::Zstd)
            .accept_compressed(CompressionEncoding::Gzip);

        let mut tasks = vec![];

        let live_data_service = self.create_live_data_service(&mut tasks).await;
        let historical_data_service = self.create_historical_data_service(&mut tasks).await;

        let wrapper = Arc::new(DataServiceWrapperWrapper::new(
            live_data_service,
            historical_data_service,
        ));
        let wrapper_service_raw =
            aptos_protos::indexer::v1::raw_data_server::RawDataServer::from_arc(wrapper.clone())
                .send_compressed(CompressionEncoding::Zstd)
                .accept_compressed(CompressionEncoding::Zstd)
                .accept_compressed(CompressionEncoding::Gzip)
                .max_decoding_message_size(MAX_MESSAGE_SIZE)
                .max_encoding_message_size(MAX_MESSAGE_SIZE);
        let wrapper_service =
            aptos_protos::indexer::v1::data_service_server::DataServiceServer::from_arc(wrapper)
                .send_compressed(CompressionEncoding::Zstd)
                .accept_compressed(CompressionEncoding::Zstd)
                .accept_compressed(CompressionEncoding::Gzip)
                .max_decoding_message_size(MAX_MESSAGE_SIZE)
                .max_encoding_message_size(MAX_MESSAGE_SIZE);

        let listen_address = self.service_config.listen_address;
        let mut server_builder = Server::builder()
            .http2_keepalive_interval(Some(HTTP2_PING_INTERVAL_DURATION))
            .http2_keepalive_timeout(Some(HTTP2_PING_TIMEOUT_DURATION));
        if let Some(config) = &self.service_config.tls_config {
            let cert = tokio::fs::read(config.cert_path.clone()).await?;
            let key = tokio::fs::read(config.key_path.clone()).await?;
            let identity = tonic::transport::Identity::from_pem(cert, key);
            server_builder = server_builder
                .tls_config(tonic::transport::ServerTlsConfig::new().identity(identity))?;
            info!(
                grpc_address = listen_address.to_string().as_str(),
                "[Data Service] Starting gRPC server with TLS."
            );
        } else {
            info!(
                grpc_address = listen_address.to_string().as_str(),
                "[data service] starting gRPC server with non-TLS."
            );
        }

        tasks.push(tokio::spawn(async move {
            server_builder
                .add_service(wrapper_service)
                .add_service(wrapper_service_raw)
                .add_service(reflection_service)
                .serve(listen_address)
                .await
                .map_err(|e| anyhow::anyhow!(e))
        }));

        futures::future::try_join_all(tasks).await?;
        Ok(())
    }

    fn get_server_name(&self) -> String {
        "indexer_grpc_data_service_v2".to_string()
    }

    async fn status_page(&self) -> Result<Response, Rejection> {
        let mut tabs = vec![];
        // TODO(grao): Add something real.
        let overview_tab_content = HtmlElement::new(HtmlTag::Div).with_raw("Welcome!").into();
        tabs.push(Tab::new("Overview", overview_tab_content));
        if let Some(live_data_service) = LIVE_DATA_SERVICE.get() {
            let connection_manager_info =
                render_connection_manager_info(live_data_service.get_connection_manager());
            let cache_info = render_cache_info();
            let content = HtmlElement::new(HtmlTag::Div)
                .with_container(connection_manager_info)
                .with_container(cache_info)
                .into();
            tabs.push(Tab::new("LiveDataService", content));
        }

        if let Some(historical_data_service) = HISTORICAL_DATA_SERVICE.get() {
            let connection_manager_info =
                render_connection_manager_info(historical_data_service.get_connection_manager());
            let file_store_info = render_file_store_info();
            let content = HtmlElement::new(HtmlTag::Div)
                .with_container(connection_manager_info)
                .with_container(file_store_info)
                .into();
            tabs.push(Tab::new("HistoricalDataService", content));
        }

        render_status_page(tabs)
    }
}

fn render_connection_manager_info(connection_manager: &ConnectionManager) -> Container {
    let known_latest_version = connection_manager.known_latest_version();
    let active_streams = connection_manager.get_active_streams();
    let active_streams_table = active_streams.into_iter().fold(
        Table::new()
            .with_attributes([("style", "width: 100%; border: 5px solid black;")])
            .with_thead_attributes([("style", "background-color: lightcoral; color: white;")])
            .with_custom_header_row(
                TableRow::new()
                    .with_cell(TableCell::new(TableCellType::Header).with_raw("Id"))
                    .with_cell(TableCell::new(TableCellType::Header).with_raw("Current Version"))
                    .with_cell(TableCell::new(TableCellType::Header).with_raw("End Version"))
                    .with_cell(
                        TableCell::new(TableCellType::Header).with_raw("Past 10s throughput"),
                    )
                    .with_cell(
                        TableCell::new(TableCellType::Header).with_raw("Past 60s throughput"),
                    )
                    .with_cell(
                        TableCell::new(TableCellType::Header).with_raw("Past 10min throughput"),
                    ),
            ),
        |table, active_stream| {
            table.with_custom_body_row(
                TableRow::new()
                    .with_cell(TableCell::new(TableCellType::Data).with_raw(&active_stream.id))
                    .with_cell(TableCell::new(TableCellType::Data).with_raw(format!(
                            "{:?}",
                            active_stream
                                .progress.as_ref()
                                .map(|progress| {
                                    progress.samples.last().map(|sample| sample.version)
                                })
                                .flatten()
                        )))
                    .with_cell(
                        TableCell::new(TableCellType::Data).with_raw(active_stream.end_version()),
                    )
                    .with_cell(TableCell::new(TableCellType::Data).with_raw(
                        get_throughput_from_samples(
                            active_stream.progress.as_ref(),
                            Duration::from_secs(10),
                        ),
                    ))
                    .with_cell(TableCell::new(TableCellType::Data).with_raw(
                        get_throughput_from_samples(
                            active_stream.progress.as_ref(),
                            Duration::from_secs(60),
                        ),
                    ))
                    .with_cell(TableCell::new(TableCellType::Data).with_raw(
                        get_throughput_from_samples(
                            active_stream.progress.as_ref(),
                            Duration::from_secs(600),
                        ),
                    )),
            )
        },
    );

    Container::new(ContainerType::Section)
        .with_paragraph_attr(
            "Connection Manager",
            [("style", "font-size: 24px; font-weight: bold;")],
        )
        .with_paragraph(format!("Known latest version: {known_latest_version}."))
        .with_paragraph_attr(
            "Active Streams",
            [("style", "font-size: 16px; font-weight: bold;")],
        )
        .with_table(active_streams_table)
}

fn render_cache_info() -> Container {
    Container::new(ContainerType::Section).with_paragraph_attr(
        "In Memory Cache",
        [("style", "font-size: 24px; font-weight: bold;")],
    )
}

fn render_file_store_info() -> Container {
    Container::new(ContainerType::Section).with_paragraph_attr(
        "File Store",
        [("style", "font-size: 24px; font-weight: bold;")],
    )
}
