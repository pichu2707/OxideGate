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
use crate::telemetry::logger::{flatten_context_breakdown, tools_fields};
use crate::telemetry::{
    CodexQuota, MeteredBody, MetricBase, RequestMetric, SessionAttribution, SessionSource,
};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use std::sync::Arc;
use std::time::Instant;

/// Tope de longitud (en caracteres) para el `User-Agent` capturado en la
/// métrica. El valor se guarda CRUDO (sin normalizar: Claude Code se
/// identifica con algo como `claude-cli/1.2.3 (external, cli)`, cada harness
/// manda su propia cadena) pero un valor absurdamente largo volvería
/// ilegible una fila de `telemetry.jsonl`, así que se corta acá.
const MAX_CLIENT_LEN: usize = 200;

/// Lee el header `User-Agent` del request entrante, tal cual, con el tope de
/// longitud de [`MAX_CLIENT_LEN`] aplicado por CARACTERES (no por bytes, para
/// no partir un UTF-8 multibyte a la mitad). `None` si el header está ausente
/// o no es UTF-8 válido: preferimos un hueco honesto a un panic o un
/// placeholder inventado.
fn client_of(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::USER_AGENT)?.to_str().ok()?;
    if value.chars().count() > MAX_CLIENT_LEN {
        Some(value.chars().take(MAX_CLIENT_LEN).collect())
    } else {
        Some(value.to_string())
    }
}

/// Nombre del header explícito de OxideGate: máxima precedencia en
/// [`session_of`]. Quien invoca lo manda a propósito para etiquetar su
/// propia sesión, por encima de cualquier señal nativa del harness.
const SESSION_HEADER_EXPLICIT: &str = "x-oxidegate-session";

/// Nombre del header nativo de sesión que manda Claude Code. Segunda
/// precedencia en [`session_of`]: se consume como string OPACA, jamás se
/// interpreta semántica propia de Claude (mantiene el transporte agnóstico
/// del proveedor).
const SESSION_HEADER_NATIVE: &str = "x-claude-code-session-id";

/// Clave de fallback cuando ni el header explícito ni el nativo resolvieron
/// y tampoco hay un `User-Agent` legible (ausente o no UTF-8 válido).
/// Bucket nombrado y honesto, nunca un string vacío ni una identidad
/// inventada.
const UNATTRIBUTED_KEY: &str = "unattributed";

/// Lee un header de atribución de sesión (`X-OxideGate-Session` o
/// `x-claude-code-session-id`) por nombre. Un header presente pero vacío
/// (tras `trim`) se trata como ausente — nunca produce `Some("")` — para que
/// [`session_of`] pueda caer limpiamente al siguiente nivel de precedencia.
fn attribution_header(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(name)?.to_str().ok()?;
    if raw.trim().is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

/// Resuelve la [`SessionAttribution`] de un request por precedencia de tres
/// niveles, leyendo EXCLUSIVAMENTE cabeceras de request (nunca la respuesta
/// del upstream, a diferencia de [`CodexQuota::from_headers`]):
///
/// 1. `X-OxideGate-Session` presente y no vacío → [`SessionSource::Explicit`].
/// 2. `x-claude-code-session-id` presente y no vacío → [`SessionSource::Native`].
/// 3. Fallback → [`SessionSource::Unattributed`], con `key` = `User-Agent`
///    (reusando [`client_of`]: mismo tope de longitud y criterio de UTF-8
///    válido) o la constante [`UNATTRIBUTED_KEY`] si no hay `User-Agent`
///    legible.
///
/// Hermana de [`client_of`] (misma categoría: transporte agnóstico de
/// proveedor, ambas leen el mismo `&HeaderMap` del request entrante). Es
/// pura y síncrona: no depende de la respuesta del upstream, así que resuelve
/// idéntico tanto en el camino de éxito como en el de error de
/// `send_and_meter`.
///
/// **Invariante de privacidad**: lee SOLO las tres cabeceras de arriba,
/// jamás `Authorization`, `x-api-key` ni `x-goog-api-key`. La `key` resuelta
/// es siempre una etiqueta opaca, nunca una credencial.
fn session_of(headers: &HeaderMap) -> SessionAttribution {
    if let Some(key) = attribution_header(headers, SESSION_HEADER_EXPLICIT) {
        return SessionAttribution {
            source: SessionSource::Explicit,
            key,
        };
    }
    if let Some(key) = attribution_header(headers, SESSION_HEADER_NATIVE) {
        return SessionAttribution {
            source: SessionSource::Native,
            key,
        };
    }
    // Reusamos `client_of` para el fallback (mismo tope de longitud y
    // criterio de UTF-8 válido). Filtramos también un `User-Agent` vacío:
    // `client_of` no sanea esa forma por su cuenta, pero la invariante
    // "jamás Some(\"\")" de este eje sí lo exige.
    let key = client_of(headers)
        .filter(|ua| !ua.is_empty())
        .unwrap_or_else(|| UNATTRIBUTED_KEY.to_string());
    SessionAttribution {
        source: SessionSource::Unattributed,
        key,
    }
}

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
    // Capturado ANTES de reenviar: identifica qué harness originó el
    // request (ver `client_of`). No se muta ni se usa para decidir nada acá,
    // solo viaja hasta la métrica.
    let client = client_of(req_headers);

    // Resuelto ANTES de reenviar, igual que `client`: `session_of` solo lee
    // cabeceras del request entrante, así que resuelve idéntico tanto si el
    // upstream responde como si falla (ver doc de `session_of`).
    let session = session_of(req_headers);

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
            let (tools_by_server, tools_overhead_bytes) = tools_fields(
                out.context.as_ref(),
                out.tools_by_server,
                out.tools_overhead_bytes,
            );

            state.telemetry.record(RequestMetric {
                timestamp: chrono::Utc::now().to_rfc3339(),
                route: out.route,
                upstream: out.upstream.to_string(),
                model: out.model,
                prompt_hash: out.prompt_hash,
                stream: out.stream,
                // Movemos `client` acá sin clonar: esta rama SIEMPRE hace
                // `return` antes de llegar al uso de más abajo (éxito), así
                // que ambos usos son mutuamente excluyentes en tiempo de
                // ejecución y el análisis de flujo del compilador lo permite.
                client,
                // Mismo patrón de `move` que `client`: `session` ya se
                // resolvió arriba a partir de `req_headers`, así que el
                // fallback honesto de `session_of` se aplica de forma
                // natural acá, sin caso especial para el fallo de upstream.
                session,
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
                tools_by_server,
                tools_overhead_bytes,
                prepare_us,
                requested_effort: out.requested_effort,
                requested_speed: out.requested_speed,
                served_speed: None,
                // No hubo respuesta del upstream que inspeccionar: sin
                // `resp`, no hay cabeceras `x-codex-*` que leer. `None`
                // honesto, no un dato inventado.
                codex_quota: None,
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
        client,
        session,
        prompt_bytes: out.prompt_bytes,
        status: status.as_u16(),
        cache_control_forced: out.cache_control_forced,
        context: out.context,
        tools_by_server: out.tools_by_server,
        tools_overhead_bytes: out.tools_overhead_bytes,
        prepare_us,
        requested_effort: out.requested_effort,
        requested_speed: out.requested_speed,
        provider: prov,
        // `resp` está vivo acá: `resp.status()` ya se leyó arriba (préstamo
        // inmutable) y el bucle de copia de cabeceras a la respuesta
        // saliente todavía no corrió. `from_headers` hace lookups puntuales
        // `get("x-codex-…")` — no recorre ni bufferiza — así que esto no es
        // un segundo pase costoso ni toca el stream SSE.
        codex_quota: CodexQuota::from_headers(resp.headers()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    /// Inserta un header sintético en un `HeaderMap` de prueba.
    fn insert(headers: &mut HeaderMap, name: &'static str, value: &str) {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_str(value).expect("valor de cabecera de prueba válido"),
        );
    }

    /// Scenario (spec §"X-OxideGate-Session presente y no vacío"): gana
    /// sobre `x-claude-code-session-id` aunque ambos estén presentes.
    #[test]
    fn explicit_gana_sobre_native_cuando_ambos_presentes() {
        let mut headers = HeaderMap::new();
        insert(&mut headers, "x-oxidegate-session", "claude-1");
        insert(&mut headers, "x-claude-code-session-id", "native-session-9");

        let session = session_of(&headers);

        assert_eq!(session.source, SessionSource::Explicit);
        assert_eq!(session.key, "claude-1");
    }

    /// Scenario (spec §"X-OxideGate-Session ausente, x-claude-code-session-id
    /// presente"): sin el header explícito, gana el nativo.
    #[test]
    fn native_gana_cuando_explicit_esta_ausente() {
        let mut headers = HeaderMap::new();
        insert(&mut headers, "x-claude-code-session-id", "native-session-9");

        let session = session_of(&headers);

        assert_eq!(session.source, SessionSource::Native);
        assert_eq!(session.key, "native-session-9");
    }

    /// Scenario (spec §"Ambos headers ausentes"): sin ningún header de
    /// atribución, cae al fallback con el `User-Agent` como valor.
    #[test]
    fn fallback_con_user_agent_cuando_ningun_header_de_atribucion_presente() {
        let mut headers = HeaderMap::new();
        insert(
            &mut headers,
            "user-agent",
            "claude-cli/1.2.3 (external, cli)",
        );

        let session = session_of(&headers);

        assert_eq!(session.source, SessionSource::Unattributed);
        assert_eq!(session.key, "claude-cli/1.2.3 (external, cli)");
    }

    /// Scenario (spec §"X-OxideGate-Session presente pero vacío,
    /// x-claude-code-session-id presente"): un header vacío se trata como
    /// ausente y la resolución cae al siguiente nivel, nunca al string vacío.
    #[test]
    fn explicit_vacio_cae_a_native_nunca_string_vacio() {
        let mut headers = HeaderMap::new();
        insert(&mut headers, "x-oxidegate-session", "");
        insert(&mut headers, "x-claude-code-session-id", "native-session-9");

        let session = session_of(&headers);

        assert_eq!(session.source, SessionSource::Native);
        assert_eq!(session.key, "native-session-9");
        assert_ne!(session.key, "");
    }

    /// Scenario (spec §"Ambos headers de atribución presentes pero
    /// vacíos"): ambos caen al fallback con el `User-Agent`, nunca un
    /// string vacío.
    #[test]
    fn ambos_headers_de_atribucion_vacios_caen_al_fallback_nunca_string_vacio() {
        let mut headers = HeaderMap::new();
        insert(&mut headers, "x-oxidegate-session", "");
        insert(&mut headers, "x-claude-code-session-id", "");
        insert(&mut headers, "user-agent", "gemini-cli/0.9");

        let session = session_of(&headers);

        assert_eq!(session.source, SessionSource::Unattributed);
        assert_eq!(session.key, "gemini-cli/0.9");
        assert_ne!(session.key, "");
    }

    /// Scenario (spec §"Ambos headers ausentes", caso sin `User-Agent`): el
    /// fallback sin ningún `User-Agent` presente resuelve a la constante
    /// `"unattributed"`, nunca el string vacío ni `None`.
    #[test]
    fn fallback_sin_user_agent_usa_constante_unattributed() {
        let headers = HeaderMap::new();

        let session = session_of(&headers);

        assert_eq!(session.source, SessionSource::Unattributed);
        assert_eq!(session.key, UNATTRIBUTED_KEY);
    }

    /// Prueba de invariante de privacidad: una credencial (`Authorization`)
    /// presente junto al header explícito de sesión nunca contamina la
    /// `key` resuelta — `session_of` lee EXCLUSIVAMENTE las tres cabeceras
    /// de transporte, jamás cabeceras de auth.
    #[test]
    fn credencial_presente_no_contamina_la_key_resuelta() {
        let mut headers = HeaderMap::new();
        insert(&mut headers, "authorization", "Bearer super-secreto-123");
        insert(&mut headers, "x-oxidegate-session", "claude-1");

        let session = session_of(&headers);

        assert_eq!(session.key, "claude-1");
        assert!(!session.key.contains("super-secreto-123"));
    }

    /// Invariante explícita "nunca `Some(\"\")`": recorre los casos de saneo
    /// (headers vacíos) y afirma además que `key` nunca es `String::new()`
    /// en ninguna rama, sea cual sea la fuente que terminó resolviendo.
    #[test]
    fn key_nunca_es_string_vacio_en_ninguna_rama_de_saneo() {
        let mut solo_native_vacio_con_native_valido = HeaderMap::new();
        insert(
            &mut solo_native_vacio_con_native_valido,
            "x-oxidegate-session",
            "",
        );
        insert(
            &mut solo_native_vacio_con_native_valido,
            "x-claude-code-session-id",
            "native-session-9",
        );
        assert_ne!(session_of(&solo_native_vacio_con_native_valido).key, "");

        let mut ambos_vacios = HeaderMap::new();
        insert(&mut ambos_vacios, "x-oxidegate-session", "");
        insert(&mut ambos_vacios, "x-claude-code-session-id", "");
        assert_ne!(session_of(&ambos_vacios).key, "");
    }
}
