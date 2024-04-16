use candle::Error as CandleError;
use ed25519_consensus::{SigningKey as PrivateKey, VerificationKey as PublicKey};
use futures::StreamExt;
use serde_json::Value;
use std::fmt::Debug;
use std::{io, path::PathBuf, time::Instant};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;
use tracing::{error, info};

use thiserror::Error;

use crate::{
    apis::ApiError,
    model_thread::{ModelThreadDispatcher, ModelThreadError, ModelThreadHandle},
    models::config::ModelsConfig,
};

pub struct ModelService {
    model_thread_handle: Vec<ModelThreadHandle>,
    dispatcher: ModelThreadDispatcher,
    start_time: Instant,
    flush_storage: bool,
    public_key: PublicKey,
    cache_dir: PathBuf,
    json_server_req_rx: Receiver<(Value, oneshot::Sender<Value>)>,
    subscriber_req_rx: Receiver<Value>,
    atoma_node_resp_tx: Sender<Value>,
}

impl ModelService {
    pub fn start(
        model_config: ModelsConfig,
        private_key: PrivateKey,
        json_server_req_rx: Receiver<(Value, oneshot::Sender<Value>)>,
        subscriber_req_rx: Receiver<Value>,
        atoma_node_resp_tx: Sender<Value>,
    ) -> Result<Self, ModelServiceError> {
        let public_key = private_key.verification_key();

        let flush_storage = model_config.flush_storage();
        let cache_dir = model_config.cache_dir();

        let (dispatcher, model_thread_handle) =
            ModelThreadDispatcher::start(model_config, public_key)
                .map_err(ModelServiceError::ModelThreadError)?;
        let start_time = Instant::now();

        Ok(Self {
            dispatcher,
            model_thread_handle,
            start_time,
            flush_storage,
            cache_dir,
            public_key,
            json_server_req_rx,
            subscriber_req_rx,
            atoma_node_resp_tx,
        })
    }

    pub async fn run(&mut self) -> Result<(), ModelServiceError> {
        loop {
            tokio::select! {
                message = self.json_server_req_rx.recv() => {
                    if let Some(request) = message {
                        self.dispatcher.run_json_inference(request);
                    }
                },
                message = self.subscriber_req_rx.recv() => {
                    if let Some(request) = message {
                        self.dispatcher.run_subscriber_inference(request);
                    }
                }
                Some(resp) = self.dispatcher.responses.next() => {
                        match resp {
                            Ok(response) => {
                                info!("Received a new inference response: {:?}", response);
                                self.atoma_node_resp_tx.send(response).await.map_err(|e| ModelServiceError::SendError(e.to_string()))?;
                            }
                            Err(e) => {
                                error!("Found error in generating inference response: {e}");
                            }
                        }
                }
            }
        }
    }

    pub fn public_key(&self) -> PublicKey {
        self.public_key
    }
}

impl ModelService {
    pub async fn stop(mut self) {
        info!(
            "Stopping Inference Service, running time: {:?}",
            self.start_time.elapsed()
        );

        if self.flush_storage {
            match std::fs::remove_dir(self.cache_dir) {
                Ok(()) => {}
                Err(e) => error!("Failed to remove storage folder, on shutdown: {e}"),
            };
        }

        let _ = self
            .model_thread_handle
            .drain(..)
            .map(|h| h.stop())
            .collect::<Vec<_>>();
    }
}

#[derive(Debug, Error)]
pub enum ModelServiceError {
    #[error("Failed to run inference: `{0}`")]
    FailedInference(Box<dyn std::error::Error + Send + Sync>),
    #[error("Failed to fecth model: `{0}`")]
    FailedModelFetch(String),
    #[error("Failed to generate private key: `{0}`")]
    PrivateKeyError(io::Error),
    #[error("Core error: `{0}`")]
    ModelThreadError(ModelThreadError),
    #[error("Api error: `{0}`")]
    ApiError(ApiError),
    #[error("Candle error: `{0}`")]
    CandleError(CandleError),
    #[error("Sender error: `{0}`")]
    SendError(String),
}

impl From<ApiError> for ModelServiceError {
    fn from(error: ApiError) -> Self {
        Self::ApiError(error)
    }
}

impl From<CandleError> for ModelServiceError {
    fn from(error: CandleError) -> Self {
        Self::CandleError(error)
    }
}

#[cfg(test)]
mod tests {
    use ed25519_consensus::VerificationKey as PublicKey;
    use rand::rngs::OsRng;
    use std::io::Write;
    use toml::{toml, Value};

    use crate::models::{config::ModelConfig, ModelTrait, Request, Response};

    use super::*;

    impl Request for () {
        type ModelInput = ();

        fn into_model_input(self) -> Self::ModelInput {}

        fn is_node_authorized(&self, _: &PublicKey) -> bool {
            true
        }

        fn request_id(&self) -> usize {
            0
        }

        fn requested_model(&self) -> crate::models::ModelId {
            String::from("")
        }
    }

    impl Response for () {
        type ModelOutput = ();

        fn from_model_output(_: Self::ModelOutput) -> Self {}
    }

    #[derive(Clone)]
    struct TestModelInstance {}

    impl ModelTrait for TestModelInstance {
        type Input = ();
        type Output = ();
        type LoadData = ();

        fn fetch(_: String, _: PathBuf, _: ModelConfig) -> Result<(), crate::models::ModelError> {
            Ok(())
        }

        fn load(_: Self::LoadData) -> Result<Self, crate::models::ModelError> {
            Ok(Self {})
        }

        fn model_type(&self) -> crate::models::types::ModelType {
            crate::models::types::ModelType::LlamaV1
        }

        fn run(&mut self, _: Self::Input) -> Result<Self::Output, crate::models::ModelError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_inference_service_initialization() {
        const CONFIG_FILE_PATH: &str = "./inference.toml";

        let private_key = PrivateKey::new(OsRng);

        let config_data = Value::Table(toml! {
            api_key = "your_api_key"
            cache_dir = "./cache_dir/"
            flush_storage = true
            models = [
            [
                0,
                "bf16",
                "mamba_370m",
                "",
                false
            ]]
            tracing = true
            jrpc_port = 3000
        });
        let toml_string =
            toml::to_string_pretty(&config_data).expect("Failed to serialize to TOML");

        let mut file = std::fs::File::create(CONFIG_FILE_PATH).expect("Failed to create file");
        file.write_all(toml_string.as_bytes())
            .expect("Failed to write to file");

        let (_, json_server_req_rx) = tokio::sync::mpsc::channel(1);
        let (_, subscriber_req_rx) = tokio::sync::mpsc::channel(1);
        let (atoma_node_resp_tx, _) = tokio::sync::mpsc::channel(1);

        let config = ModelsConfig::from_file_path(CONFIG_FILE_PATH);

        let _ = ModelService::start(
            config,
            private_key,
            json_server_req_rx,
            subscriber_req_rx,
            atoma_node_resp_tx,
        )
        .unwrap();

        std::fs::remove_file(CONFIG_FILE_PATH).unwrap();
    }
}
