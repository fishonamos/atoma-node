use axum::Router;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::handlers::{
    almost_filled_stacks, attestation_disputes, nodes, stacks, subscriptions, tasks,
};

pub fn openapi_routes() -> Router {
    /// OpenAPI documentation for the Atoma daemon API.
    #[derive(OpenApi)]
    #[openapi(
        nest(
            (path = almost_filled_stacks::ALMOST_FILLED_STACKS_PATH, api = almost_filled_stacks::AlmostFilledStacksOpenApi, tags = ["almost_filled_stacks"]),
            (path = attestation_disputes::ATTESTATION_DISPUTES_PATH, api = attestation_disputes::AttestationDisputesOpenApi, tags = ["attestation_disputes"]),
            (path = nodes::NODES_PATH, api = nodes::NodesOpenApi, tags = ["nodes"]),
            (path = stacks::STACKS_PATH, api = stacks::StacksOpenApi, tags = ["stacks"]),
            (path = subscriptions::SUBSCRIPTIONS_PATH, api = subscriptions::SubscriptionsOpenApi, tags = ["subscriptions"]),
            (path = tasks::TASKS_PATH, api = tasks::TasksOpenApi, tags = ["tasks"])
        ),
        tags(
            (name = "almost_filled_stacks", description = "Almost filled stacks management"),
            (name = "attestation_disputes", description = "Attestation disputes management"),
            (name = "nodes", description = "Nodes management"),
            (name = "stacks", description = "Stacks management"),
            (name = "subscriptions", description = "Subscriptions management"),
            (name = "tasks", description = "Tasks management")
        ),
        servers(
            (url = "http://localhost:8080", description = "Local development server")
        )
    )]
    struct ApiDoc;

    // Generate the OpenAPI spec and write it to a file in debug mode
    #[cfg(debug_assertions)]
    {
        use std::fs;
        use std::path::Path;

        let spec =
            serde_yaml::to_string(&ApiDoc::openapi()).expect("Failed to serialize OpenAPI spec");

        let docs_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
        fs::create_dir_all(&docs_dir).expect("Failed to create docs directory");

        let spec_path = docs_dir.join("openapi.yml");
        fs::write(&spec_path, spec).expect("Failed to write OpenAPI spec to file");

        println!("OpenAPI spec written to: {:?}", spec_path);
    }

    Router::new()
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
}
