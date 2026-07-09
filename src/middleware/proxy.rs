//! Passthrough transparente con medición (MVP 1).
//!
//! Reenvía la petición al proveedor y hace *streaming* de la respuesta de
//! vuelta SIN bufferizarla (crítico para SSE). Este módulo es transporte
//! GENÉRICO: no conoce ningún proveedor concreto, solo el trait
//! [`Provider`](crate::provider::Provider). Cada ruta instancia el proveedor
//! que le corresponde y delega en [`run`], que arma el `Incoming`, llama a
//! `provider.prepare(...)` y reenvía con `send_and_meter`, envolviendo la
//! respuesta en [`MeteredBody`] para medir TTFT, tokens exactos (`usage`) y
//! coste sin tocar el camino crítico.
use crate::provider::{self, Incoming, Outgoing, Provider};
use crate::state::AppState;
use crate::telemetry::logger::flatten_context_breakdown;
use crate::telemetry::{MeteredBody, MetricBase, RequestMetric};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use std::sync::Arc;
use std::time::Instant;

/// `POST /v1/chat/completions` → proveedor OpenAI (Chat Completions).
pub async fn handle_openai_route(state: State<Arc<AppState>>, req: Request) -> Response {
    run(&provider::OPENAI_CHAT, state, req).await
}

/// `POST /v1/messages` → proveedor Anthropic.
pub async fn handle_anthropic_route(state: State<Arc<AppState>>, req: Request) -> Response {
    run(&provider::ANTHROPIC, state, req).await
}

/// `POST /v1/responses` → OpenAI Responses API (la que usan clientes
/// modernos: Codex, SDKs nuevos).
pub async fn handle_openai_responses(state: State<Arc<AppState>>, req: Request) -> Response {
    run(&provider::OPENAI_RESPONSES, state, req).await
}

/// `POST /v1beta/models/{model}:{método}` → proveedor Google Gemini.
pub async fn handle_gemini_route(state: State<Arc<AppState>>, req: Request) -> Response {
    run(&provider::GEMINI, state, req).await
}

/// Transporte genérico compartido por las cuatro rutas.
///
/// Lee el body entero (barato: son JSON únicos, no streams), arma
/// [`Incoming`] con lo que cualquier proveedor pueda necesitar (body y,
/// para rutas path-based como Gemini, path + query), y delega en el
/// proveedor la construcción del request saliente antes de reenviar y medir.
async fn run(prov: &'static dyn Provider, State(state): State<Arc<AppState>>, req: Request) -> Response {
    let start = Instant::now();
    let (parts, body) = req.into_parts();

    // Path y query originales: solo Gemini los necesita (modelo/método van en
    // la URL), pero los leemos siempre acá porque `parts` no sobrevive más
    // allá de este punto.
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().map(str::to_string);

    // El body de estas APIs es un JSON único (no un stream), así que leerlo
    // entero es barato y le da al proveedor todo lo que necesita de una.
    let raw = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b.to_vec(),
        Err(e) => return plain_error(StatusCode::BAD_REQUEST, format!("body inválido: {e}")),
    };

    let incoming = Incoming {
        path,
        query,
        body: raw,
    };

    // `prepare_us` mide EXCLUSIVAMENTE el trabajo propio del proxy dentro de
    // `prepare` (parseo del body, `decompose`, mutación opcional): no incluye
    // ni la lectura del body del socket (ya ocurrió arriba) ni el
    // round-trip hacia el proveedor (ocurre después, en `send_and_meter`).
    let prepare_start = Instant::now();
    let out = prov.prepare(incoming, &state.config);
    let prepare_us = prepare_start.elapsed().as_micros() as u64;

    send_and_meter(prov, state, &parts.headers, out, start, prepare_us).await
}

/// Reenvía la petición ya resuelta al proveedor y envuelve la respuesta con la
/// telemetría. Compartido por las cuatro rutas: garantiza que el descarte de
/// `Accept-Encoding` y la medición valgan igual para todos los proveedores.
async fn send_and_meter(
    prov: &'static dyn Provider,
    state: Arc<AppState>,
    req_headers: &HeaderMap,
    out: Outgoing,
    start: Instant,
    prepare_us: u64,
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
            let (
                context_system_bytes,
                context_tools_bytes,
                context_history_bytes,
                context_last_turn_bytes,
                context_other_bytes,
                context_measured_bytes,
                context_messages_count,
                context_tax_ratio,
            ) = flatten_context_breakdown(out.context.as_ref());

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
                cache_read_tokens: None,
                cache_write_tokens: None,
                cost_estimate_usd: None,
                cache_control_forced: out.cache_control_forced,
                status: StatusCode::BAD_GATEWAY.as_u16(),
                ttft_ms: None,
                total_ms: start.elapsed().as_secs_f64() * 1000.0,
                tokens_per_sec: None,
                context_system_bytes,
                context_tools_bytes,
                context_history_bytes,
                context_last_turn_bytes,
                context_other_bytes,
                context_measured_bytes,
                context_messages_count,
                context_tax_ratio,
                prepare_us,
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
        cache_control_forced: out.cache_control_forced,
        context: out.context,
        prepare_us,
        provider: prov,
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

    // Envolvemos el stream: mide TTFT, escanea `usage` (delegado en el
    // proveedor) y emite la métrica al cerrarse, reenviando cada chunk
    // intacto hacia el cliente.
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
