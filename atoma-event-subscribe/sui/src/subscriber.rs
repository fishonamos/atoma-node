use std::{path::Path, time::Duration};

use futures::StreamExt;
use sui_sdk::rpc_types::EventFilter;
use sui_sdk::types::base_types::{ObjectID, ObjectIDParseError};
use sui_sdk::{SuiClient, SuiClientBuilder};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::config::SuiSubscriberConfig;
use atoma_types::Request;

pub struct SuiSubscriber {
    id: u64,
    sui_client: SuiClient,
    filter: EventFilter,
    event_sender: mpsc::Sender<Request>,
}

impl SuiSubscriber {
    pub async fn new(
        id: u64,
        http_url: &str,
        ws_url: Option<&str>,
        object_id: ObjectID,
        event_sender: mpsc::Sender<Request>,
        request_timeout: Option<Duration>,
    ) -> Result<Self, SuiSubscriberError> {
        let mut sui_client_builder = SuiClientBuilder::default();
        if let Some(duration) = request_timeout {
            sui_client_builder = sui_client_builder.request_timeout(duration);
        }
        if let Some(url) = ws_url {
            sui_client_builder = sui_client_builder.ws_url(url);
        }
        info!("Starting sui client..");
        let sui_client = sui_client_builder.build(http_url).await?;
        let filter = EventFilter::Package(object_id);
        Ok(Self {
            id,
            sui_client,
            filter,
            event_sender,
        })
    }

    pub async fn new_from_config<P: AsRef<Path>>(
        id: u64,
        config_path: P,
        event_sender: mpsc::Sender<Request>,
    ) -> Result<Self, SuiSubscriberError> {
        let config = SuiSubscriberConfig::from_file_path(config_path);
        let http_url = config.http_url();
        let ws_url = config.ws_url();
        let object_id = config.object_id();
        let request_timeout = config.request_timeout();
        Self::new(
            id,
            &http_url,
            Some(&ws_url),
            object_id,
            event_sender,
            Some(request_timeout),
        )
        .await
    }

    pub async fn subscribe(self) -> Result<(), SuiSubscriberError> {
        let event_api = self.sui_client.event_api();
        let mut subscribe_event = event_api.subscribe_event(self.filter).await?;
        info!("Starting event while loop");
        while let Some(event) = subscribe_event.next().await {
            match event {
                Ok(event) => {
                    let event_data = event.parsed_json;
                    let request = serde_json::from_value::<Request>(event_data)?;
                    info!("Received new request: {:?}", request);
                    let request_id = request.id();
                    let sampled_nodes = request.sampled_nodes();
                    if sampled_nodes.contains(&self.id) {
                        info!(
                            "Current node has been sampled for request with id: {}",
                            request_id
                        );
                        self.event_sender.send(request).await?;
                    } else {
                        info!("Current node has not been sampled for request with id: {}, ignoring it..", request_id);
                    }
                }
                Err(e) => {
                    error!("Failed to get event with error: {e}");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum SuiSubscriberError {
    #[error("Sui Builder error: `{0}`")]
    SuiBuilderError(#[from] sui_sdk::error::Error),
    #[error("Deserialize error: `{0}`")]
    DeserializeError(#[from] serde_json::Error),
    #[error("Object ID parse error: `{0}`")]
    ObjectIDParseError(#[from] ObjectIDParseError),
    #[error("Sender error: `{0}`")]
    SendError(#[from] mpsc::error::SendError<Request>),
}