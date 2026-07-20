//! Punto de entrada (Levanta el servidor local)
mod config;
mod middleware;
mod optimizer;
mod provider;
mod state;
mod telemetry;

use axum::{
    routing::{get, post},
    Router,
};
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
        // Responses API de Codex (`pi`): mismo dialecto que la de arriba,
        // pero reenviada a chatgpt.com/backend-api/codex en vez de
        // api.openai.com. Body a veces comprimido en zstd.
        .route(
            "/v1/codex/responses",
            post(middleware::proxy::handle_openai_codex_responses),
        )
        // Ruta comodín de Gemini: captura `/v1beta/models/{model}:{método}`.
        .route(
            "/v1beta/*rest",
            post(middleware::proxy::handle_gemini_route),
        )
        // Liveness barata: no depende de AppState ni toma locks de
        // telemetría. La usa el plugin de OpenCode para decidir si redirige
        // tráfico de Codex hacia acá antes de tocar nada más pesado.
        .route("/health", get(middleware::health::handle_health))
        // Agregación en vivo por (proveedor, modelo): qué optimizar ahora.
        .route("/stats", get(middleware::stats::handle_stats))
        // Detalle en vivo de los últimos requests individuales: qué request
        // puntual es atípico (outlier de coste/latencia).
        .route("/requests", get(middleware::requests::handle_requests))
        .with_state(Arc::new(state));

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();

    println!("🛰️  Escuchando en http://{addr}");
    println!("💚 Liveness en http://{addr}/health");
    println!("📊 Estadísticas en vivo por modelo en http://{addr}/stats");
    println!("🧾 Últimos requests en vivo en http://{addr}/requests");
    axum::serve(listener, app).await.unwrap();
}
