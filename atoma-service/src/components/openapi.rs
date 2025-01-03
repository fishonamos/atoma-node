use axum::Router;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::handlers::chat_completions::{ChatCompletionsOpenApi, CHAT_COMPLETIONS_PATH};
use crate::handlers::embeddings::{EmbeddingsOpenApi, EMBEDDINGS_PATH};
use crate::handlers::image_generations::{ImageGenerationsOpenApi, IMAGE_GENERATIONS_PATH};
use crate::server::{HealthOpenApi, MetricsOpenApi, HEALTH_PATH, METRICS_PATH};

pub fn openapi_routes() -> Router {
    #[derive(OpenApi)]
    #[openapi(
        nest(
            (path = HEALTH_PATH, api = HealthOpenApi),
            (path = METRICS_PATH, api = MetricsOpenApi),
            (path = CHAT_COMPLETIONS_PATH, api = ChatCompletionsOpenApi),
            (path = EMBEDDINGS_PATH, api = EmbeddingsOpenApi),
            (path = IMAGE_GENERATIONS_PATH, api = ImageGenerationsOpenApi),
        ),
        tags(
            (name = "health", description = "Health check"),
            (name = "metrics", description = "Metrics"),
            (name = "chat", description = "Chat completions"),
            (name = "embeddings", description = "Embeddings"),
            (name = "images", description = "Image generations"),
        ),
        servers(
            (url = "http://localhost:8080"),
        )
    )]
    struct ApiDoc;

    // Generate the OpenAPI spec and write it to a file
    #[cfg(debug_assertions)]
    {
        use std::fs;
        use std::path::Path;

        // Generate OpenAPI spec
        let spec =
            serde_yaml::to_string(&ApiDoc::openapi()).expect("Failed to serialize OpenAPI spec");

        // Ensure the docs directory exists
        let docs_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
        fs::create_dir_all(&docs_dir).expect("Failed to create docs directory");

        // Write the spec to the file
        let spec_path = docs_dir.join("openapi.yml");
        fs::write(&spec_path, spec).expect("Failed to write OpenAPI spec to file");

        println!("OpenAPI spec written to: {:?}", spec_path);
    }

    Router::new()
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
}
