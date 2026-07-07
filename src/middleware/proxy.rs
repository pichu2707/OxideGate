//! Passthrough transparente con medición (MVP 1).
//!
//! Reenvía la petición al proveedor y hace *streaming* de la respuesta de vuelta
//! SIN bufferizarla (crítico para SSE). De paso instrumenta el request: saca
//! modelo/stream/huella del prompt, y envuelve la respuesta en [`MeteredBody`]
//! para medir TTFT, tokens exactos (`usage`) y coste sin tocar el camino crítico.
//!
//! Soporta tres proveedores con dos estilos de ruta:
//!   - Anthropic / OpenAI: ruta fija, modelo y `stream` en el body JSON.
//!   - Gemini: ruta comodín `/v1beta/*`, modelo y método en la URL.
use crate::state::AppState;
use crate::telemetry::{MeteredBody, MetricBase, RequestMetric};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

/// `POST /v1/chat/completions` → proveedor estilo OpenAI.
pub async fn handle_openai_route(state: State<Arc<AppState>>, req: Request) -> Response {
    let url = format!("{}/chat/completions", state.config.target_openai_url);
    forward(state, req, "/v1/chat/completions", "openai", url).await
}

/// `POST /v1/messages` → proveedor Anthropic.
pub async fn handle_anthropic_route(state: State<Arc<AppState>>, req: Request) -> Response {
    let url = format!("{}/messages", state.config.target_anthropic_url);
    forward(state, req, "/v1/messages", "anthropic", url).await
}

/// `POST /v1/responses` → OpenAI Responses API (la que usan clientes modernos).
///
/// Modelo y `stream` van en el body (como chat/completions), pero NO inyectamos
/// `include_usage`: la Responses API ya reporta `usage` en el evento
/// `response.completed`. Por eso no reusa `forward`, que sí inyecta para OpenAI.
pub async fn handle_openai_responses(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let start = Instant::now();
    let (parts, body) = req.into_parts();
    let url = format!("{}/responses", state.config.target_openai_url);

    let raw = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b.to_vec(),
        Err(e) => return plain_error(StatusCode::BAD_REQUEST, format!("body inválido: {e}")),
    };
    let (model, stream) = model_and_stream_from_body(&raw);

    let out = Outgoing {
        url,
        route: "/v1/responses".to_string(),
        upstream: "openai",
        model,
        stream,
        prompt_hash: fingerprint(&raw),
        prompt_bytes: raw.len(),
        body: raw,
    };
    send_and_meter(state, &parts.headers, out, start).await
}

/// `POST /v1beta/models/{model}:{método}` → proveedor Google Gemini.
///
/// A diferencia de Anthropic/OpenAI, aquí el modelo y el método viven en la URL
/// (no en el body), y hay que preservar path + query string (que llevan
/// `alt=sse` y a veces la API key) al reenviar al host de Gemini.
pub async fn handle_gemini_route(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let start = Instant::now();
    let (parts, body) = req.into_parts();

    // Preservamos el path y la query originales tal cual hacia Gemini.
    let path = parts.uri.path();
    let (model, stream) = parse_gemini_path(path);
    let mut url = format!("{}{}", state.config.target_gemini_url, path);
    if let Some(query) = parts.uri.query() {
        url.push('?');
        url.push_str(query);
    }

    let raw = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b.to_vec(),
        Err(e) => return plain_error(StatusCode::BAD_REQUEST, format!("body inválido: {e}")),
    };

    let out = Outgoing {
        url,
        route: path.to_string(),
        upstream: "gemini",
        model,
        stream,
        prompt_hash: fingerprint(&raw),
        prompt_bytes: raw.len(),
        // Gemini reporta `usageMetadata` por defecto en el stream SSE: no hay que
        // inyectar nada, reenviamos el body intacto.
        body: raw,
    };
    send_and_meter(state, &parts.headers, out, start).await
}

/// Extrae `(modelo, es_stream)` del path de Gemini.
///
/// El path tiene la forma `/v1beta/models/{model}:{método}`, donde el método
/// `streamGenerateContent` indica streaming y `generateContent` no.
fn parse_gemini_path(path: &str) -> (Option<String>, bool) {
    match path.split("/models/").nth(1) {
        Some(tail) => {
            let mut it = tail.splitn(2, ':');
            let model = it.next().filter(|s| !s.is_empty()).map(str::to_string);
            let stream = it.next().unwrap_or("").contains("stream");
            (model, stream)
        }
        None => (None, false),
    }
}

/// Petición ya resuelta y lista para reenviar al proveedor, con todo lo que la
/// métrica necesita conocer de antemano.
struct Outgoing {
    url: String,
    route: String,
    upstream: &'static str,
    model: Option<String>,
    stream: bool,
    prompt_hash: String,
    prompt_bytes: usize,
    body: Vec<u8>,
}

/// Lo que sabemos del request tras inspeccionar su body (rutas Anthropic/OpenAI).
struct PreparedRequest {
    body: Vec<u8>,
    model: Option<String>,
    stream: bool,
    prompt_hash: String,
    prompt_bytes: usize,
}

/// Inspecciona el body del request y prepara el que se enviará al proveedor.
///
/// - Lee `model` y `stream` del JSON.
/// - Calcula la huella sobre los bytes ORIGINALES (antes de cualquier mutación).
/// - Para OpenAI en streaming inyecta `stream_options.include_usage = true`, sin
///   lo cual OpenAI no reporta `usage` en el stream y perderíamos los tokens de
///   salida exactos. Anthropic ya manda `usage` por defecto: no se toca.
fn prepare_request(raw: Vec<u8>, upstream: &str) -> PreparedRequest {
    let prompt_bytes = raw.len();
    let prompt_hash = fingerprint(&raw);

    // Si el body no es JSON válido, lo reenviamos tal cual y no medimos tokens.
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return PreparedRequest {
            body: raw,
            model: None,
            stream: false,
            prompt_hash,
            prompt_bytes,
        };
    };

    let model = value
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());
    let stream = value
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    // Inyección solo para OpenAI en streaming.
    let body = if upstream == "openai" && stream {
        value["stream_options"]["include_usage"] = serde_json::Value::Bool(true);
        serde_json::to_vec(&value).unwrap_or(raw)
    } else {
        raw
    };

    PreparedRequest {
        body,
        model,
        stream,
        prompt_hash,
        prompt_bytes,
    }
}

/// Huella no criptográfica (hash de 64 bits en hex) del body del request.
///
/// No busca resistencia a colisiones: solo queremos que "mismo prompt ⇒ misma
/// huella" para detectar peticiones redundantes de forma barata.
fn fingerprint(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Lee `model` y `stream` de un body JSON (formato chat/completions y Responses).
/// Si el body no es JSON válido, devuelve `(None, false)`.
fn model_and_stream_from_body(raw: &[u8]) -> (Option<String>, bool) {
    match serde_json::from_slice::<serde_json::Value>(raw) {
        Ok(v) => (
            v.get("model").and_then(|m| m.as_str()).map(str::to_string),
            v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
        ),
        Err(_) => (None, false),
    }
}

/// Ruta de Anthropic/OpenAI: lee el body, lo prepara y delega en `send_and_meter`.
async fn forward(
    State(state): State<Arc<AppState>>,
    req: Request,
    route: &str,
    upstream: &'static str,
    url: String,
) -> Response {
    let start = Instant::now();
    let (parts, body) = req.into_parts();

    // El body de estas APIs es un JSON único (no un stream), así que leerlo
    // entero es barato y nos da modelo, flag de stream y tamaño de una.
    let raw = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b.to_vec(),
        Err(e) => return plain_error(StatusCode::BAD_REQUEST, format!("body inválido: {e}")),
    };
    let prepared = prepare_request(raw, upstream);

    let out = Outgoing {
        url,
        route: route.to_string(),
        upstream,
        model: prepared.model,
        stream: prepared.stream,
        prompt_hash: prepared.prompt_hash,
        prompt_bytes: prepared.prompt_bytes,
        body: prepared.body,
    };
    send_and_meter(state, &parts.headers, out, start).await
}

/// Reenvía la petición ya resuelta al proveedor y envuelve la respuesta con la
/// telemetría. Compartido por las tres rutas: garantiza que el descarte de
/// `Accept-Encoding` y la medición valgan igual para todos los proveedores.
async fn send_and_meter(
    state: Arc<AppState>,
    req_headers: &HeaderMap,
    out: Outgoing,
    start: Instant,
) -> Response {
    // Reconstruimos la petición copiando las cabeceras originales (auth,
    // content-type, anthropic-version, x-goog-api-key…).
    let mut outbound = state.http.post(&out.url).body(out.body);
    for (name, value) in req_headers.iter() {
        // HOST/CONTENT_LENGTH los recalcula reqwest. ACCEPT_ENCODING se descarta
        // a propósito: si dejamos que el proveedor comprima (gzip/br) la
        // respuesta, nuestro escáner SSE leería bytes comprimidos y NO podría
        // extraer el `usage`. Pidiéndola sin comprimir la medimos en texto
        // plano; el cliente la recibe igual (sin `content-encoding`).
        if name == header::HOST
            || name == header::CONTENT_LENGTH
            || name == header::ACCEPT_ENCODING
        {
            continue;
        }
        outbound = outbound.header(name, value);
    }

    let resp = match outbound.send().await {
        Ok(r) => r,
        Err(e) => {
            // No perdemos el evento: registramos el fallo de upstream con lo que
            // sabemos, aunque no haya tokens ni respuesta que medir.
            state.telemetry.record(RequestMetric {
                timestamp: chrono::Utc::now().to_rfc3339(),
                route: out.route,
                upstream: out.upstream.to_string(),
                model: out.model,
                prompt_hash: out.prompt_hash,
                stream: out.stream,
                prompt_bytes: out.prompt_bytes,
                input_tokens: None,
                output_tokens: None,
                cost_estimate_usd: None,
                status: StatusCode::BAD_GATEWAY.as_u16(),
                ttft_ms: None,
                total_ms: start.elapsed().as_secs_f64() * 1000.0,
                tokens_per_sec: None,
            });
            return plain_error(
                StatusCode::BAD_GATEWAY,
                format!("upstream {}: {e}", out.upstream),
            );
        }
    };

    let status = resp.status();

    // Base de la métrica: todo lo conocido antes de que fluya la respuesta.
    let base = MetricBase {
        timestamp: chrono::Utc::now().to_rfc3339(),
        route: out.route,
        upstream: out.upstream.to_string(),
        model: out.model,
        prompt_hash: out.prompt_hash,
        stream: out.stream,
        prompt_bytes: out.prompt_bytes,
        status: status.as_u16(),
    };

    // Copiamos las cabeceras ANTES de consumir `resp`: `bytes_stream` toma
    // posesión de la respuesta.
    let mut builder = Response::builder().status(status);
    for (name, value) in resp.headers().iter() {
        if name == header::CONTENT_LENGTH || name == header::TRANSFER_ENCODING {
            continue;
        }
        builder = builder.header(name, value);
    }

    // Envolvemos el stream: mide TTFT, escanea `usage` y emite la métrica al
    // cerrarse, reenviando cada chunk intacto hacia el cliente.
    let metered = MeteredBody::new(resp.bytes_stream(), state.telemetry.clone(), base, start);

    builder
        .body(Body::from_stream(metered))
        .unwrap_or_else(|e| plain_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn plain_error(status: StatusCode, msg: String) -> Response {
    Response::builder()
        .status(status)
        .body(Body::from(msg))
        .expect("respuesta de error siempre construible")
}
