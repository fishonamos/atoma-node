use std::str::FromStr;

use crate::{
    error::AtomaServiceError,
    handlers::{
        chat_completions::CHAT_COMPLETIONS_PATH, embeddings::EMBEDDINGS_PATH,
        image_generations::IMAGE_GENERATIONS_PATH,
    },
    server::AppState,
};
use atoma_confidential::types::{
    ConfidentialComputeDecryptionRequest, ConfidentialComputeDecryptionResponse, DH_PUBLIC_KEY_SIZE,
};
use atoma_state::types::AtomaAtomaStateManagerEvent;
use atoma_utils::{
    constants::{NONCE_SIZE, PAYLOAD_HASH_SIZE, SALT_SIZE},
    hashing::blake2b_hash,
    parse_json_byte_array, verify_signature,
};
use axum::{body::Body, extract::State, http::Request, middleware::Next, response::Response};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use sui_sdk::types::{
    base_types::SuiAddress,
    crypto::{PublicKey, Signature, SuiSignature},
    digests::TransactionDigest,
};
use tokio::sync::oneshot;
use tracing::instrument;

/// Body size limit for signature verification (contains the body size of the request)
const MAX_BODY_SIZE: usize = 1024 * 1024; // 1MB

/// The key for the model in the request body
const MODEL: &str = "model";

/// The key for the max tokens in the request body
const MAX_TOKENS: &str = "max_tokens";

/// The key for the messages in the request body
const MESSAGES: &str = "messages";

/// The key for the input tokens in the request body
const INPUT: &str = "input";

/// The key for the image size in the request body
const IMAGE_SIZE: &str = "size";

/// The key for the number of images in the request body
const IMAGE_N: &str = "n";

/// Metadata for confidential compute encryption requests
#[derive(Clone, Debug)]
pub struct EncryptionMetadata {
    /// The client's proxy X25519 public key
    pub proxy_x25519_public_key: [u8; DH_PUBLIC_KEY_SIZE],
    /// The salt
    pub salt: [u8; SALT_SIZE],
}

/// Metadata extracted from the request
#[derive(Clone, Debug, Default)]
pub struct RequestMetadata {
    /// The stack small ID
    pub stack_small_id: i64,
    /// The estimated total number of compute units
    pub estimated_total_compute_units: i64,
    /// The payload hash
    pub payload_hash: [u8; 32],
    /// The type of request
    pub request_type: RequestType,
    /// The client's Diffie-Hellman public key and salt
    pub client_encryption_metadata: Option<EncryptionMetadata>,
    /// endpoint path
    pub endpoint_path: String,
}

/// The type of request
///
/// This enum is used to determine the type of request based on the path of the request.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum RequestType {
    #[default]
    ChatCompletions,
    Embeddings,
    ImageGenerations,
    NonInference,
}

impl RequestMetadata {
    /// Create a new `RequestMetadata` with the given stack info
    pub fn with_stack_info(
        mut self,
        stack_small_id: i64,
        estimated_total_compute_units: i64,
    ) -> Self {
        self.stack_small_id = stack_small_id;
        self.estimated_total_compute_units = estimated_total_compute_units;
        self
    }

    /// Create a new `RequestMetadata` with the given payload hash
    pub fn with_payload_hash(mut self, payload_hash: [u8; PAYLOAD_HASH_SIZE]) -> Self {
        self.payload_hash = payload_hash;
        self
    }

    /// Sets the request type for this metadata instance
    ///
    /// # Arguments
    /// * `request_type` - The type of request (ChatCompletions, Embeddings, ImageGenerations, or NonInference)
    ///
    /// # Returns
    /// Returns self with the updated request type for method chaining
    ///
    /// # Example
    /// ```
    /// use atoma_service::middleware::{RequestMetadata, RequestType};
    ///
    /// let metadata = RequestMetadata::default()
    ///     .with_request_type(RequestType::ChatCompletions);
    /// ```
    pub fn with_request_type(mut self, request_type: RequestType) -> Self {
        self.request_type = request_type;
        self
    }

    /// Sets the client's encryption metadata for this metadata instance
    ///
    /// * `client_dh_public_key` - The client's Diffie-Hellman public key
    /// * `salt` - The salt
    ///
    /// # Returns
    /// Returns self with the updated client's encryption metadata for method chaining
    ///
    /// # Example
    /// ```rust,ignore
    /// use atoma_service::middleware::RequestMetadata;
    ///
    /// let metadata = RequestMetadata::default()
    ///     .with_client_encryption_metadata(client_dh_public_key, salt);
    /// ```
    pub fn with_client_encryption_metadata(
        mut self,
        proxy_x25519_public_key: [u8; DH_PUBLIC_KEY_SIZE],
        salt: [u8; SALT_SIZE],
    ) -> Self {
        self.client_encryption_metadata = Some(EncryptionMetadata {
            proxy_x25519_public_key,
            salt,
        });
        self
    }

    /// Sets the endpoint path for this metadata instance
    ///
    /// * `endpoint_path` - The path of the endpoint
    ///
    /// # Returns
    /// Returns self with the updated endpoint path for method chaining
    ///
    /// # Example
    /// ```rust,ignore
    /// use atoma_service::middleware::RequestMetadata;
    ///
    /// let metadata = RequestMetadata::default().with_endpoint_path(CHAT_COMPLETIONS_PATH.to_string());
    /// ```
    pub fn with_endpoint_path(mut self, endpoint_path: String) -> Self {
        self.endpoint_path = endpoint_path;
        self
    }
}

/// Middleware for verifying the signature of incoming requests.
///
/// This middleware is designed to authenticate and verify the integrity of incoming requests
/// for an OpenAI API-compatible chat completions endpoint. It performs the following steps:
///
/// 1. Extracts the signature, public key, and signature scheme from the request headers.
/// 2. Decodes the signature and public key from base64.
/// 3. Reads and hashes the request body, which contains data such as the prompt, parameters,
///    and other information necessary for chat completions.
/// 4. Verifies the signature using the specified signature scheme, ensuring the request
///    has not been tampered with and comes from a trusted source.
///
/// # Request Body
/// The body of the request is expected to contain JSON data compatible with the OpenAI API
/// chat completions endpoint, including:
/// - `prompt`: The input text or conversation history.
/// - `parameters`: Various parameters for controlling the chat completion (e.g., temperature,
///   max_tokens, stop sequences).
/// - Other fields as required by the OpenAI API specification.
///
/// # Headers
/// The middleware expects the following custom headers:
/// - `X-Signature`: The signature of the request body, base64 encoded.
///
/// # Extensions
/// This middleware adds or updates a `RequestMetadata` extension to the request containing:
/// - `payload_hash`: The 32-byte Blake2b hash of the request body
///
/// # Errors
/// Returns a `BAD_REQUEST` status code if:
/// - Required headers are missing or cannot be parsed.
/// - The signature or public key cannot be decoded.
/// - The request body exceeds the maximum size limit.
///
/// Returns an `UNAUTHORIZED` status code if:
/// - The signature verification fails.
///
/// # Security Note
/// This middleware is crucial for ensuring that only authorized clients can access the
/// chat completions endpoint, protecting against unauthorized use and potential abuse.
#[instrument(
    level = "info",
    skip_all,
    fields(
        endpoint = %req.uri().path(),
    )
)]
pub async fn signature_verification_middleware(
    req: Request<Body>,
    next: Next,
) -> Result<Response, AtomaServiceError> {
    let (mut req_parts, req_body) = req.into_parts();
    let endpoint = req_parts.uri.path().to_string();

    let base64_signature = req_parts
        .headers
        .get(atoma_utils::constants::SIGNATURE)
        .ok_or_else(|| AtomaServiceError::MissingHeader {
            header: atoma_utils::constants::SIGNATURE.to_string(),
            endpoint: endpoint.clone(),
        })?
        .to_str()
        .map_err(|e| AtomaServiceError::InvalidHeader {
            message: format!("Failed to convert signature to string, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let body_bytes = axum::body::to_bytes(req_body, MAX_BODY_SIZE)
        .await
        .map_err(|e| AtomaServiceError::InvalidBody {
            message: format!("Failed to convert body to bytes, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let body_json: Value =
        serde_json::from_slice(&body_bytes).map_err(|e| AtomaServiceError::InvalidBody {
            message: format!("Failed to parse body as JSON, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let body_blake2b_hash = blake2b_hash(body_json.to_string().as_bytes());
    let body_blake2b_hash_bytes: [u8; 32] = body_blake2b_hash
        .as_slice()
        .try_into()
        .expect("Invalid Blake2b hash length");
    verify_signature(base64_signature, &body_blake2b_hash_bytes).map_err(|e| {
        AtomaServiceError::AuthError {
            auth_error: format!("Failed to verify signature, with error: {e}"),
            endpoint,
        }
    })?;
    let request_metadata = req_parts
        .extensions
        .get::<RequestMetadata>()
        .cloned()
        .unwrap_or_default()
        .with_payload_hash(body_blake2b_hash_bytes);
    req_parts.extensions.insert(request_metadata);
    let req = Request::from_parts(req_parts, Body::from(body_bytes));

    Ok(next.run(req).await)
}

/// Middleware for verifying stack permissions and compute units usage.
///
/// This middleware performs several checks to ensure that the incoming request
/// is authorized to use the specified model and has sufficient compute units available.
///
/// # Steps:
/// 1. Extracts and validates the public key and stack ID from request headers.
/// 2. Parses the request body to extract the model and messages.
/// 3. Verifies that the requested model is supported.
/// 4. Calculates the total number of compute units required for the request.
/// 5. Checks if the user has an available stack with sufficient compute units.
///
/// # Headers
/// The middleware expects the following custom headers:
/// - `X-Stack-Small-Id`: The ID of the stack being used for this request.
///
/// # Request Body
/// The body should be a JSON object containing:
/// - `model`: The name of the AI model to be used.
/// - `messages`: An array of message objects, each containing a "content" field.
/// - `max_tokens`: The maximum number of tokens for the AI's response.
///
/// # Extensions
/// This middleware adds a `RequestMetadata` extension to the request containing:
/// - `stack_small_id`: The ID of the stack being used
/// - `estimated_total_compute_units`: The total number of compute units calculated for the request
///
/// This metadata can be accessed by downstream handlers using `req.extensions()`.
///
/// # Errors
/// Returns a `BAD_REQUEST` status code if:
/// - Required headers are missing or invalid.
/// - The request body is invalid or missing required fields.
/// - The requested model is not supported.
///
/// Returns an `UNAUTHORIZED` status code if:
/// - There's no available stack with sufficient compute units.
/// - Fetching available stacks fails.
///
/// # Security Note
/// This middleware is crucial for ensuring that users only consume resources they're
/// authorized to use and have sufficient compute units for their requests.
#[instrument(
    level = "info",
    skip_all,
    fields(
        endpoint = %req.uri().path(),
    )
)]
pub async fn verify_stack_permissions(
    state: State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, AtomaServiceError> {
    let (mut req_parts, req_body) = req.into_parts();
    let endpoint = req_parts.uri.path().to_string();

    // Get request path to determine type
    let request_type = match req_parts.uri.path() {
        CHAT_COMPLETIONS_PATH => RequestType::ChatCompletions,
        EMBEDDINGS_PATH => RequestType::Embeddings,
        IMAGE_GENERATIONS_PATH => RequestType::ImageGenerations,
        _ => RequestType::NonInference,
    };

    let base64_signature = req_parts
        .headers
        .get(atoma_utils::constants::SIGNATURE)
        .ok_or_else(|| AtomaServiceError::MissingHeader {
            header: atoma_utils::constants::SIGNATURE.to_string(),
            endpoint: endpoint.clone(),
        })?
        .to_str()
        .map_err(|e| AtomaServiceError::InvalidHeader {
            message: format!("Failed to convert signature to string, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let signature =
        Signature::from_str(base64_signature).map_err(|e| AtomaServiceError::InvalidHeader {
            message: format!("Failed to parse signature, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let public_key_bytes = signature.public_key_bytes();
    let public_key =
        PublicKey::try_from_bytes(signature.scheme(), public_key_bytes).map_err(|e| {
            AtomaServiceError::InvalidHeader {
                message: format!("Failed to extract public key from bytes, with error: {e}"),
                endpoint: endpoint.clone(),
            }
        })?;
    let sui_address = SuiAddress::from(&public_key);
    let stack_small_id = req_parts
        .headers
        .get(atoma_utils::constants::STACK_SMALL_ID)
        .ok_or_else(|| AtomaServiceError::MissingHeader {
            header: atoma_utils::constants::STACK_SMALL_ID.to_string(),
            endpoint: endpoint.clone(),
        })?;
    let stack_small_id = stack_small_id
        .to_str()
        .map_err(|e| AtomaServiceError::InvalidHeader {
            message: format!("Stack small ID cannot be converted to a string, with error: {e}"),
            endpoint: endpoint.clone(),
        })?
        .parse::<i64>()
        .map_err(|e| AtomaServiceError::InvalidHeader {
            message: format!("Stack small ID is not a valid integer, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let body_bytes = axum::body::to_bytes(req_body, MAX_BODY_SIZE)
        .await
        .map_err(|e| AtomaServiceError::InvalidBody {
            message: format!("Failed to convert body to bytes, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let body_json: Value =
        serde_json::from_slice(&body_bytes).map_err(|e| AtomaServiceError::InvalidBody {
            message: format!("Failed to parse body as JSON, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let model = body_json
        .get(MODEL)
        .ok_or_else(|| AtomaServiceError::InvalidBody {
            message: "Model not found in body".to_string(),
            endpoint: endpoint.clone(),
        })?
        .as_str()
        .ok_or_else(|| AtomaServiceError::InvalidBody {
            message: "Model is not a string".to_string(),
            endpoint: endpoint.clone(),
        })?;
    if !state.models.contains(&model.to_string()) {
        return Err(AtomaServiceError::InvalidBody {
            message: format!("Model not supported, supported models: {:?}", state.models),
            endpoint: endpoint.clone(),
        });
    }

    let total_num_compute_units = utils::calculate_compute_units(
        &body_json,
        request_type.clone(),
        &state,
        model,
        endpoint.clone(),
    )?;

    let (result_sender, result_receiver) = oneshot::channel();
    state
        .state_manager_sender
        .send(
            AtomaAtomaStateManagerEvent::GetAvailableStackWithComputeUnits {
                stack_small_id,
                sui_address: sui_address.to_string(),
                total_num_compute_units,
                result_sender,
            },
        )
        .map_err(|err| AtomaServiceError::InternalError {
            message: format!("Failed to get available stacks: {}", err),
            endpoint: endpoint.clone(),
        })?;
    let available_stack = result_receiver
        .await
        .map_err(|e| AtomaServiceError::AuthError {
            auth_error: format!(
                "Failed to get available stack with enough compute units, with error: {e}"
            ),
            endpoint: endpoint.clone(),
        })?
        .map_err(|err| AtomaServiceError::AuthError {
            auth_error: format!(
                "Failed to get available stack with enough compute units, with error: {err}"
            ),
            endpoint: endpoint.clone(),
        })?;
    if available_stack.is_none() {
        let tx_digest_str = req_parts
            .headers
            .get(atoma_utils::constants::TX_DIGEST)
            .ok_or_else(|| AtomaServiceError::InvalidHeader {
                message: "Stack not found, tx digest header expected but not found".to_string(),
                endpoint: endpoint.clone(),
            })?
            .to_str()
            .map_err(|e| AtomaServiceError::InvalidHeader {
                message: format!("Tx digest cannot be converted to a string, with error: {e}"),
                endpoint: endpoint.clone(),
            })?;
        let tx_digest = TransactionDigest::from_str(tx_digest_str).unwrap();
        let (tx_stack_small_id, compute_units) = utils::request_blockchain_for_stack(
            &state,
            tx_digest,
            total_num_compute_units,
            endpoint.clone(),
        )
        .await?;

        // NOTE: We need to check that the stack small id matches the one in the request
        // otherwise, the user is requesting for a different stack, which is invalid. We
        // must also check that the compute units are enough for processing the request.
        // We do not update the [`AtomaStateManager`] with the new stack small id and compute units
        // as the Sui subscriber service should catch the event for `Stack` creation.
        if stack_small_id != tx_stack_small_id as i64
            || compute_units > total_num_compute_units as u64
        {
            return Err(AtomaServiceError::AuthError {
                auth_error: "No available stack with enough compute units".to_string(),
                endpoint,
            });
        }
    }
    let request_metadata = req_parts
        .extensions
        .get::<RequestMetadata>()
        .cloned()
        .unwrap_or_default()
        .with_stack_info(stack_small_id, total_num_compute_units)
        .with_request_type(request_type)
        .with_endpoint_path(req_parts.uri.path().to_string());
    req_parts.extensions.insert(request_metadata);
    let req = Request::from_parts(req_parts, Body::from(body_bytes));
    Ok(next.run(req).await)
}

/// Middleware for handling confidential compute requests by decrypting encrypted payloads.
///
/// This middleware intercepts requests containing encrypted data and processes them through
/// a confidential compute pipeline before passing them to the next handler. It performs
/// decryption using a combination of:
/// - A salt for key derivation
/// - A nonce for encryption uniqueness
/// - A Diffie-Hellman public key for secure key exchange
///
/// # Headers Required
/// The middleware expects the following headers:
/// - `X-Salt`: Base string containing the salt used in key derivation
/// - `X-Nonce`: Base string containing the nonce used in encryption
/// - `X-Node-X25519-PublicKey`: Base64-encoded public key (32 bytes) for key exchange
///
/// # Request Flow
/// 1. Extracts and validates required headers
/// 2. Decodes the Diffie-Hellman public key from base64
/// 3. Reads the encrypted request body
/// 4. Sends decryption request to confidential compute service
/// 5. Receives decrypted plaintext
/// 6. Reconstructs request with decrypted body
///
/// # Returns
/// * `Ok(Response)` - The response from the next handler after successful decryption
/// * `Err(AtomaServiceError)` - One of:
///   - `AtomaServiceError::InvalidHeader` if headers are missing or invalid
///   - `AtomaServiceError::InternalError` if decryption service communication fails
///
/// # Example Headers
/// ```text
/// X-Salt: randomsaltvalue
/// X-Nonce: uniquenoncevalue
/// X-Node-X25519-PublicKey: base64encodedkey...
/// ```
///
/// # Security Notes
/// - The salt and nonce should be unique for each request
/// - The Diffie-Hellman public key must be exactly 32 bytes when decoded
/// - All cryptographic operations are performed in a separate confidential compute service
#[instrument(
    level = "info", skip_all,
    fields(
        endpoint = %req.uri().path(),
    )
)]
pub async fn confidential_compute_middleware(
    state: State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, AtomaServiceError> {
    let (mut req_parts, req_body) = req.into_parts();

    let endpoint = req_parts.uri.path().to_string();
    let salt = req_parts
        .headers
        .get(atoma_utils::constants::SALT)
        .ok_or_else(|| AtomaServiceError::InvalidHeader {
            message: "Salt header not found".to_string(),
            endpoint: endpoint.clone(),
        })?;
    let salt_str = salt
        .to_str()
        .map_err(|e| AtomaServiceError::InvalidHeader {
            message: format!("Salt cannot be converted to a string, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let salt_bytes = STANDARD
        .decode(salt_str)
        .map_err(|e| AtomaServiceError::InvalidHeader {
            message: format!("Failed to decode salt from base64 encoding, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let salt_bytes: [u8; SALT_SIZE] = salt_bytes.try_into().map_err(|e| {
        AtomaServiceError::InvalidHeader {
            message: format!(
                "Failed to convert salt bytes to {SALT_SIZE}-byte array, incorrect length, with error: {:?}",
                e
            ),
            endpoint: endpoint.clone(),
        }
    })?;
    let nonce = req_parts
        .headers
        .get(atoma_utils::constants::NONCE)
        .ok_or_else(|| AtomaServiceError::InvalidHeader {
            message: "Nonce header not found".to_string(),
            endpoint: endpoint.clone(),
        })?;
    let nonce_str = nonce
        .to_str()
        .map_err(|e| AtomaServiceError::InvalidHeader {
            message: format!("Nonce cannot be converted to a string, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let nonce_bytes = STANDARD
        .decode(nonce_str)
        .map_err(|e| AtomaServiceError::InvalidHeader {
            message: format!("Failed to decode nonce from base64 encoding, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let nonce_bytes: [u8; NONCE_SIZE] = nonce_bytes.try_into().map_err(|e| {
        AtomaServiceError::InvalidHeader {
            message: format!(
                "Failed to convert nonce bytes to {NONCE_SIZE}-byte array, incorrect length, with error: {:?}",
                e
            ),
            endpoint: endpoint.clone(),
        }
    })?;
    let proxy_x25519_public_key = req_parts
        .headers
        .get(atoma_utils::constants::PROXY_X25519_PUBLIC_KEY)
        .ok_or_else(|| AtomaServiceError::InvalidHeader {
            message: "Diffie-Hellman public key header not found".to_string(),
            endpoint: endpoint.clone(),
        })?;
    let proxy_x25519_public_key_bytes: [u8; DH_PUBLIC_KEY_SIZE] = STANDARD
        .decode(proxy_x25519_public_key)
        .map_err(|e| {
            AtomaServiceError::InvalidHeader {
                message: format!("Failed to decode Proxy X25519 public key from base64 encoding, with error: {e}"),
                endpoint: endpoint.clone(),
            }
        })?
        .try_into()
        .map_err(|e| {
            AtomaServiceError::InvalidHeader {
                message: format!(
                    "Failed to convert Proxy X25519 public key bytes to 32-byte array, incorrect length, with error: {:?}",
                    e
                ),
                endpoint: endpoint.clone(),
            }
        })?;
    let node_x25519_public_key = req_parts
        .headers
        .get(atoma_utils::constants::NODE_X25519_PUBLIC_KEY)
        .ok_or_else(|| AtomaServiceError::InvalidHeader {
            message: "Node X25519 public key header not found".to_string(),
            endpoint: endpoint.clone(),
        })?;
    let node_x25519_public_key_bytes: [u8; DH_PUBLIC_KEY_SIZE] = STANDARD
        .decode(node_x25519_public_key)
        .map_err(|e| {
            AtomaServiceError::InvalidHeader {
                message: format!("Failed to decode Node X25519 public key from base64 encoding, with error: {e}"),
                endpoint: endpoint.clone(),
            }
        })?
        .try_into()
        .map_err(|e| {
            AtomaServiceError::InvalidHeader {
                message: format!(
                    "Failed to convert Node X25519 public key bytes to 32-byte array, incorrect length, with error: {:?}",
                    e
                ),
                endpoint: endpoint.clone(),
            }
        })?;
    let body_bytes = axum::body::to_bytes(req_body, 1024 * MAX_BODY_SIZE)
        .await
        .map_err(|e| AtomaServiceError::InvalidBody {
            message: format!("Failed to convert body to bytes, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    // Convert body bytes to string since it was created with .to_string() on the client
    let body_str =
        String::from_utf8(body_bytes.to_vec()).map_err(|e| AtomaServiceError::InvalidBody {
            message: format!("Failed to convert body bytes to string: {}", e),
            endpoint: endpoint.clone(),
        })?;

    let body_json: Value =
        serde_json::from_str(&body_str).map_err(|e| AtomaServiceError::InvalidBody {
            message: format!("Failed to parse body as JSON, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let ciphertext_bytes = parse_json_byte_array(&body_json, atoma_utils::constants::CIPHERTEXT)
        .map_err(|e| AtomaServiceError::InvalidBody {
            message: format!("Failed to parse ciphertext from body, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let confidential_compute_decryption_request = ConfidentialComputeDecryptionRequest {
        ciphertext: ciphertext_bytes,
        nonce: nonce_bytes,
        salt: salt_bytes,
        proxy_x25519_public_key: proxy_x25519_public_key_bytes,
        node_x25519_public_key: node_x25519_public_key_bytes,
    };
    let (result_sender, result_receiver) = oneshot::channel();
    state
        .decryption_sender
        .send((confidential_compute_decryption_request, result_sender))
        .map_err(|e| AtomaServiceError::InternalError {
            message: format!("Failed to send confidential compute request, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    let result = result_receiver
        .await
        .map_err(|e| AtomaServiceError::InternalError {
            message: format!("Failed to receive confidential compute response, with error: {e}"),
            endpoint: endpoint.clone(),
        })?;
    match result {
        Ok(ConfidentialComputeDecryptionResponse { plaintext }) => {
            let body = Body::from(plaintext);
            req_parts.extensions.insert(
                RequestMetadata::default()
                    .with_client_encryption_metadata(proxy_x25519_public_key_bytes, salt_bytes),
            );
            let req = Request::from_parts(req_parts, body);
            Ok(next.run(req).await)
        }
        Err(e) => Err(AtomaServiceError::InternalError {
            message: format!("Failed to decrypt confidential compute response, with error: {e}"),
            endpoint: endpoint.clone(),
        }),
    }
}

pub(crate) mod utils {
    use super::*;

    /// Queries the blockchain to retrieve compute units associated with a specific transaction.
    ///
    /// This asynchronous function uses a combination of channels to communicate with the blockchain:
    /// - An mpsc channel to send the query request to the compute units handler
    /// - A oneshot channel to receive the response back in the current scope
    ///
    /// # Arguments
    /// * `state` - Reference to the application state containing the compute units mpsc sender
    /// * `tx_digest` - The transaction digest to query compute units for
    /// * `endpoint` - The endpoint that the request was made to
    ///
    /// # Returns
    /// * `Ok(u64)` - The number of compute units if found
    /// * `Err(AtomaServiceError)` - If the request fails, returns one of:
    ///   - `AtomaServiceError::InternalError` if channel communication fails
    ///   - `AtomaServiceError::AuthError` if no compute units are found for the transaction
    ///
    /// # Channel Communication Flow
    /// 1. Creates a oneshot channel for receiving the response
    /// 2. Sends the transaction digest and oneshot sender through the mpsc channel
    /// 3. Awaits the response on the oneshot receiver
    ///
    /// # Example
    /// ```rust,ignore
    /// let compute_units = request_blockchain_for_stack(&app_state, transaction_digest).await?;
    /// ```
    #[instrument(level = "trace", skip_all)]
    pub(crate) async fn request_blockchain_for_stack(
        state: &AppState,
        tx_digest: TransactionDigest,
        estimated_compute_units: i64,
        endpoint: String,
    ) -> Result<(u64, u64), AtomaServiceError> {
        let (result_sender, result_receiver) = oneshot::channel();
        state
            .stack_retrieve_sender
            .send((tx_digest, estimated_compute_units, result_sender))
            .map_err(|_| AtomaServiceError::InternalError {
                message: "Failed to send compute units request".to_string(),
                endpoint: endpoint.clone(),
            })?;
        let result = result_receiver
            .await
            .map_err(|_| AtomaServiceError::InternalError {
                message: "Failed to receive compute units".to_string(),
                endpoint: endpoint.clone(),
            })?;
        if let (Some(stack_small_id), Some(compute_units)) = result {
            Ok((stack_small_id, compute_units))
        } else {
            Err(AtomaServiceError::AuthError {
                auth_error: "No compute units found for transaction".to_string(),
                endpoint,
            })
        }
    }

    /// Calculates the total number of compute units required for a request based on its type and content.
    ///
    /// # Arguments
    /// * `body_json` - The parsed JSON body of the request containing model-specific parameters
    /// * `request_type` - The type of request (ChatCompletions, Embeddings, ImageGenerations, or NonInference)
    /// * `state` - Application state containing model configurations and tokenizers
    /// * `model` - The name of the AI model being used
    /// * `endpoint` - The endpoint that the request was made to
    ///
    /// # Returns
    /// * `Ok(i64)` - The total number of compute units required
    /// * `Err(AtomaServiceError)` - If there's an error calculating the units, returns an appropriate HTTP status code
    ///
    /// # Compute Unit Calculation
    /// The calculation varies by request type:
    /// - ChatCompletions: Based on input tokens + max output tokens
    /// - Embeddings: Based on input text length
    /// - ImageGenerations: Based on image dimensions and quantity
    /// - NonInference: Returns 0 (no compute units required)
    ///
    /// This function delegates to specific calculators based on the request type:
    /// - `calculate_chat_completion_compute_units`
    /// - `calculate_embedding_compute_units`
    /// - `calculate_image_generation_compute_units`
    pub(crate) fn calculate_compute_units(
        body_json: &Value,
        request_type: RequestType,
        state: &AppState,
        model: &str,
        endpoint: String,
    ) -> Result<i64, AtomaServiceError> {
        match request_type {
            RequestType::ChatCompletions => {
                calculate_chat_completion_compute_units(body_json, state, model, endpoint)
            }
            RequestType::Embeddings => {
                calculate_embedding_compute_units(body_json, state, model, endpoint)
            }
            RequestType::ImageGenerations => {
                calculate_image_generation_compute_units(body_json, endpoint)
            }
            RequestType::NonInference => Ok(0),
        }
    }

    /// Calculates the total number of compute units required for a chat completion request.
    ///
    /// This function analyzes the request body to determine the total computational cost by:
    /// 1. Counting tokens in all input messages
    /// 2. Adding safety margins for message formatting
    /// 3. Including the requested maximum output tokens
    ///
    /// # Arguments
    /// * `body_json` - The parsed JSON body of the request containing:
    ///   - `messages`: Array of message objects with "content" fields
    ///   - `max_tokens`: Maximum number of tokens for the model's response
    /// * `state` - Application state containing model configurations and tokenizers
    /// * `model` - The name of the AI model being used
    /// * `endpoint` - The endpoint that the request was made to
    /// # Returns
    /// * `Ok(i64)` - The total number of compute units required
    /// * `Err(AtomaServiceError)` - AtomaServiceError::InvalidBody if:
    ///   - The model is not supported
    ///   - Required fields are missing
    ///   - Message content is invalid
    ///   - Tokenization fails
    ///
    /// # Token Calculation
    /// For each message:
    /// - Base tokens: Number of tokens in the message content
    /// - +2 tokens: Safety margin for message delimiters
    /// - +1 token: Safety margin for role name
    ///     Plus the requested max_tokens for the response
    ///
    /// # Example JSON Structure
    /// ```json
    /// {
    ///     "messages": [
    ///         {"role": "user", "content": "Hello, how are you?"},
    ///         {"role": "assistant", "content": "I'm doing well, thank you!"}
    ///     ],
    ///     "max_tokens": 100
    /// }
    /// ```
    #[instrument(level = "trace", skip_all)]
    pub(crate) fn calculate_chat_completion_compute_units(
        body_json: &Value,
        state: &AppState,
        model: &str,
        endpoint: String,
    ) -> Result<i64, AtomaServiceError> {
        let tokenizer_index = state
            .models
            .iter()
            .position(|m| m == model)
            .ok_or_else(|| AtomaServiceError::InvalidBody {
                message: "Model not supported".to_string(),
                endpoint: endpoint.clone(),
            })?;

        let messages = body_json
            .get(MESSAGES)
            .ok_or_else(|| AtomaServiceError::InvalidBody {
                message: "Messages not found in body".to_string(),
                endpoint: endpoint.clone(),
            })?
            .as_array()
            .ok_or_else(|| AtomaServiceError::InvalidBody {
                message: "Messages is not an array".to_string(),
                endpoint: endpoint.clone(),
            })?;

        let mut total_num_compute_units = 0;
        for message in messages {
            let content = message
                .get("content")
                .ok_or_else(|| AtomaServiceError::InvalidBody {
                    message: "Message content not found".to_string(),
                    endpoint: endpoint.clone(),
                })?;
            let content_str = content
                .as_str()
                .ok_or_else(|| AtomaServiceError::InvalidBody {
                    message: "Message content is not a string".to_string(),
                    endpoint: endpoint.clone(),
                })?;
            let num_tokens = state.tokenizers[tokenizer_index]
                .encode(content_str, true)
                .map_err(|_| AtomaServiceError::InvalidBody {
                    message: "Failed to encode message content".to_string(),
                    endpoint: endpoint.clone(),
                })?
                .get_ids()
                .len() as i64;
            total_num_compute_units += num_tokens;
            // add 2 tokens as a safety margin, for start and end message delimiters
            total_num_compute_units += 2;
            // add 1 token as a safety margin, for the role name of the message
            total_num_compute_units += 1;
        }

        total_num_compute_units += body_json
            .get(MAX_TOKENS)
            .and_then(|value| value.as_i64())
            .ok_or_else(|| AtomaServiceError::InvalidBody {
                message: "Max tokens not found in body".to_string(),
                endpoint: endpoint.clone(),
            })?;

        Ok(total_num_compute_units)
    }

    /// Calculates the total number of compute units required for an embedding request.
    ///
    /// This function analyzes the request body to determine the computational cost by counting
    /// the number of tokens in the input text(s) that will be embedded.
    ///
    /// # Arguments
    /// * `body_json` - The parsed JSON body of the request containing:
    ///   - `input`: Either a single string or an array of strings to be embedded
    /// * `state` - Application state containing model configurations and tokenizers
    /// * `model` - The name of the AI model being used
    ///
    /// # Returns
    /// * `Ok(i64)` - The total number of compute units required
    /// * `Err(AtomaServiceError)` - AtomaServiceError::InvalidBody if:
    ///   - The model is not supported
    ///   - The input field is missing
    ///   - The input format is invalid
    ///   - Tokenization fails
    ///
    /// # Input Formats
    /// The function supports two input formats:
    /// 1. Single string:
    /// ```json
    /// {
    ///     "input": "text to embed"
    /// }
    /// ```
    ///
    /// 2. Array of strings:
    /// ```json
    /// {
    ///     "input": ["text one", "text two"]
    /// }
    /// ```
    ///
    /// # Computation
    /// The total compute units is calculated as the sum of tokens across all input texts.
    /// For array inputs, each string is tokenized separately and the results are summed.
    #[instrument(level = "trace", skip_all)]
    fn calculate_embedding_compute_units(
        body_json: &Value,
        state: &AppState,
        model: &str,
        endpoint: String,
    ) -> Result<i64, AtomaServiceError> {
        let tokenizer_index = state
            .models
            .iter()
            .position(|m| m == model)
            .ok_or_else(|| AtomaServiceError::InvalidBody {
                message: "Model not supported".to_string(),
                endpoint: endpoint.clone(),
            })?;

        let input = body_json
            .get(INPUT)
            .ok_or_else(|| AtomaServiceError::InvalidBody {
                message: "Input not found in body".to_string(),
                endpoint: endpoint.clone(),
            })?;

        // input can be a string or an array of strings
        let total_units = match input {
            Value::String(text) => state.tokenizers[tokenizer_index]
                .encode(text.as_str(), true)
                .map_err(|_| AtomaServiceError::InvalidBody {
                    message: "Failed to encode input text".to_string(),
                    endpoint: endpoint.clone(),
                })?
                .get_ids()
                .len() as i64,
            Value::Array(texts) => texts
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(|s| {
                            state.tokenizers[tokenizer_index]
                                .encode(s, true)
                                .map(|tokens| tokens.get_ids().len() as i64)
                                .unwrap_or(0)
                        })
                        .unwrap_or(0)
                })
                .sum(),
            _ => {
                return Err(AtomaServiceError::InvalidBody {
                    message: "Invalid input format".to_string(),
                    endpoint: endpoint.clone(),
                });
            }
        };

        Ok(total_units)
    }

    /// Calculates the total number of compute units required for an image generation request.
    ///
    /// This function analyzes the request body to determine the computational cost based on:
    /// - The dimensions of the requested image(s)
    /// - The number of images to generate
    ///
    /// # Arguments
    /// * `body_json` - The parsed JSON body of the request containing:
    ///   - `size`: String in format "WxH" (e.g., "1024x1024")
    ///   - `n`: Number of images to generate
    ///
    /// # Returns
    /// * `Ok(i64)` - The total number of compute units required (width * height * n)
    /// * `Err(AtomaServiceError)` - AtomaServiceError::InvalidBody if:
    ///   - The size field is missing or invalid
    ///   - The dimensions cannot be parsed
    ///   - The number of images is missing or invalid
    ///
    /// # Example JSON Structure
    /// ```json
    /// {
    ///     "size": "1024x1024",
    ///     "n": 1
    /// }
    /// ```
    #[instrument(level = "trace", skip_all)]
    fn calculate_image_generation_compute_units(
        body_json: &Value,
        endpoint: String,
    ) -> Result<i64, AtomaServiceError> {
        let size = body_json
            .get(IMAGE_SIZE)
            .ok_or_else(|| AtomaServiceError::InvalidBody {
                message: "Image size not found in body".to_string(),
                endpoint: endpoint.clone(),
            })?
            .as_str()
            .ok_or_else(|| AtomaServiceError::InvalidBody {
                message: "Image size is not a string".to_string(),
                endpoint: endpoint.clone(),
            })?;

        // width and height are the dimensions of the image to generate
        let (width, height) = size
            .split_once('x')
            .and_then(|(w, h)| {
                let width = w.parse::<i64>().ok()?;
                let height = h.parse::<i64>().ok()?;
                Some((width, height))
            })
            .ok_or_else(|| AtomaServiceError::InvalidBody {
                message: "Invalid image size format".to_string(),
                endpoint: endpoint.clone(),
            })?;

        // n is the number of images to generate
        let n = body_json
            .get(IMAGE_N)
            .and_then(|v| v.as_u64())
            .ok_or_else(|| AtomaServiceError::InvalidBody {
                message: "Invalid or missing image count (n)".to_string(),
                endpoint: endpoint.clone(),
            })? as i64;

        // Calculate total pixels
        Ok(width * height * n)
    }
}
