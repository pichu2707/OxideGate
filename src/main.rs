//! Punto de entrada (Levanta el servidor local)
mod config;
mod middleware;
mod optimizer;
mod state;
mod telemetry;

use axum::{routing::post, Router};
use config::AppConfig;
use state::AppState;
use std::net::SocketAddr;
use std::sync::Arc;
use telemetry::TelemetrySink;

#[tokio::main]
async fn main() {
    // Inicializamos la telemetría interna por consola
    tracing_subscriber::fmt::init();

    // Cargamos la configuración independiente de OxideGate
    let config = AppConfig::load();

    // Aseguramos que nuestra carpeta de datos exista de forma interna
    if !config.storage_dir.exists() {
        std::fs::create_dir_all(&config.storage_dir).unwrap_or_default();
    }

    println!("🚀 OxideGate inicializado en local.");
    println!(
        "📦 Almacenamiento de telemetría nativa en: {:?}",
        config.storage_dir
    );
    if config.has_opencode_env() {
        println!("🔍 Entorno OpenCode detectado en el sistema.");
    }

    // Arrancamos la task de telemetría (escribe fuera del camino crítico).
    let telemetry = TelemetrySink::spawn(config.storage_dir.clone());

    let port = config.local_port;
    let state = AppState {
        config: Arc::new(config),
        http: reqwest::Client::new(),
        telemetry,
    };

    // Definimos las rutas espejo del proxy para capturar las peticiones
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(middleware::proxy::handle_openai_route),
        )
        .route(
            "/v1/messages",
            post(middleware::proxy::handle_anthropic_route),
        )
        // OpenAI Responses API (clientes modernos: Codex, SDKs nuevos).
        .route(
            "/v1/responses",
            post(middleware::proxy::handle_openai_responses),
        )
        // Ruta comodín de Gemini: captura `/v1beta/models/{model}:{método}`.
        .route(
            "/v1beta/*rest",
            post(middleware::proxy::handle_gemini_route),
        )
        .with_state(Arc::new(state));

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();

    println!("🛰️  Escuchando en http://{addr}");
    axum::serve(listener, app).await.unwrap();
}
