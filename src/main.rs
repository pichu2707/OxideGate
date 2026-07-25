//! Punto de entrada (Levanta el servidor local)
mod config;
mod middleware;
mod optimizer;
mod provider;
mod state;
mod telemetry;

use axum::{
    Router,
    routing::{get, post},
};
use config::AppConfig;
use state::AppState;
use std::net::SocketAddr;
use std::sync::Arc;
use telemetry::TelemetrySink;

/// Busca un flag saltándose SIEMPRE `argv[0]`: un binario invocado por un
/// path que contenga `-h` no es una petición de ayuda.
fn wants_flag(args: &[String], long: &str, short: &str) -> bool {
    args.iter().skip(1).any(|a| a == long || a == short)
}

fn usage_text() -> String {
    format!(
        "oxidegate {version} — proxy local que mide el coste real de contexto

USO:
    oxidegate                Levanta el proxy y se queda escuchando
    oxidegate --help         Muestra esta ayuda
    oxidegate --version      Muestra la versión

VARIABLES DE ENTORNO:
    OXIDEGATE_PORT   Puerto de escucha (por defecto 8080). Conviene cambiarlo:
                     Apache, Tomcat y compañía suelen tener el 8080 ocupado, y
                     un cliente apuntando a su servidor no da ningún error
                     evidente.

RUTAS QUE SIRVE:
    GET  /health     Liveness barata. Los clientes la sondean para decidir si
                     enrutan por aquí; si devuelve 404 caen al proveedor
                     directo en silencio, sin error y sin log.
    GET  /stats      Agregado en vivo por (proveedor, modelo).
    GET  /requests   Detalle de los últimos requests individuales.

VER TAMBIÉN:
    oxidegate-monitor        Panel en vivo sobre este proxy (--once para
                             un volcado de texto plano sin TUI).
",
        version = env!("CARGO_PKG_VERSION")
    )
}

fn version_text() -> String {
    format!("oxidegate {}", env!("CARGO_PKG_VERSION"))
}

#[tokio::main]
async fn main() {
    // Ayuda y versión se responden ANTES de tocar nada: ni configuración, ni
    // carpeta de datos, ni bind. `oxidegate --help` con una instancia ya
    // corriendo panicaba con AddrInUse — justo el momento en el que más falta
    // hace poder leer la ayuda.
    let args: Vec<String> = std::env::args().collect();
    if wants_flag(&args, "--help", "-h") {
        print!("{}", usage_text());
        return;
    }
    if wants_flag(&args, "--version", "-V") {
        println!("{}", version_text());
        return;
    }

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

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn argv(rest: &[&str]) -> Vec<String> {
        std::iter::once("oxidegate".to_string())
            .chain(rest.iter().map(|s| s.to_string()))
            .collect()
    }

    #[test]
    fn asks_for_help_with_either_spelling() {
        assert!(wants_flag(&argv(&["--help"]), "--help", "-h"));
        assert!(wants_flag(&argv(&["-h"]), "--help", "-h"));
        assert!(!wants_flag(&argv(&[]), "--help", "-h"));
    }

    #[test]
    fn argv_zero_is_never_a_flag() {
        // Un binario invocado por un path que contenga "-h" no debe
        // confundirse con una petición de ayuda. Se salta argv[0] siempre.
        let disguised = vec!["-h".to_string()];
        assert!(!wants_flag(&disguised, "--help", "-h"));
    }

    #[test]
    fn usage_names_the_port_knob_and_every_route_it_serves() {
        let text = usage_text();
        // El puerto es lo primero que un usuario necesita cambiar: el default
        // 8080 lo suelen ocupar Apache y compañía.
        assert!(
            text.contains("OXIDEGATE_PORT"),
            "falta OXIDEGATE_PORT: {text}"
        );
        // /health es lo que sondean los clientes antes de enrutar. Si no se
        // documenta, un 404 ahí es indistinguible de "el proxy no arranca".
        for route in ["/health", "/stats", "/requests"] {
            assert!(text.contains(route), "falta la ruta {route}: {text}");
        }
    }

    #[test]
    fn version_text_carries_the_crate_version() {
        assert!(version_text().contains(env!("CARGO_PKG_VERSION")));
    }
}
