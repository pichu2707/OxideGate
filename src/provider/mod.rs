//! Conocimiento por-proveedor: cada proveedor sabe preparar su propio request
//! saliente y extraer su propio `usage` de la respuesta.
//!
//! Antes este conocimiento vivía desparramado entre `middleware/proxy.rs`
//! (construcción de la URL, lectura de modelo/stream, mutación del body) y
//! `telemetry/metered.rs` (extracción de `usage` hardcodeando los tres
//! proveedores). Acá se concentra en un solo lugar por proveedor, detrás del
//! trait [`Provider`]: quien agregue un proveedor nuevo solo toca este
//! módulo, sin tocar el transporte genérico ni la mecánica de medición.
pub mod anthropic;
pub mod gemini;
pub mod openai;

use crate::config::AppConfig;
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub use anthropic::ANTHROPIC;
pub use gemini::GEMINI;
pub use openai::{OPENAI_CHAT, OPENAI_RESPONSES};

/// Lo que el proxy sabe del request entrante, antes de saber a qué proveedor
/// pertenece.
///
/// Cubre tanto rutas donde modelo/stream viven en el body JSON (Anthropic,
/// OpenAI) como la ruta comodín de Gemini, donde viven en el path. Cada
/// proveedor toma de acá solo lo que necesita e ignora el resto.
pub struct Incoming {
    /// Path original del request (p. ej.
    /// `/v1beta/models/gemini-1.5-flash:streamGenerateContent`). Los
    /// proveedores con ruta fija (Anthropic, OpenAI) lo ignoran.
    pub path: String,
    /// Query string original, si la hay (Gemini la usa para `alt=sse` y a
    /// veces la API key).
    pub query: Option<String>,
    /// Body crudo, tal cual llegó del cliente, sin parsear todavía.
    pub body: Vec<u8>,
}

/// Petición ya resuelta y lista para reenviar al proveedor, con todo lo que
/// la métrica necesita conocer de antemano (antes de que fluya la
/// respuesta).
pub struct Outgoing {
    /// URL completa del proveedor a la que se reenvía la petición.
    pub url: String,
    /// Ruta local del proxy que atendió el request (se guarda en la métrica).
    pub route: String,
    /// Nombre corto del proveedor (`anthropic`, `openai`, `gemini`).
    pub upstream: &'static str,
    /// Modelo solicitado, si se pudo determinar.
    pub model: Option<String>,
    /// `true` si el cliente pidió streaming (SSE).
    pub stream: bool,
    /// Huella no criptográfica del body ORIGINAL (antes de cualquier mutación).
    pub prompt_hash: String,
    /// Tamaño en bytes del body ORIGINAL.
    pub prompt_bytes: usize,
    /// Body que efectivamente se reenvía al proveedor (puede diferir del
    /// original si el proveedor necesitó mutarlo, p. ej. OpenAI streaming).
    pub body: Vec<u8>,
}

/// Acumulador de tokens medidos desde la respuesta del proveedor.
///
/// `Default` deja todo en `None` (nada medido aún). Se actualiza de forma
/// incremental: cada llamada a [`Provider::extract_usage`] pisa los campos
/// que sí trae el valor JSON dado, y deja el resto como estaban ("último
/// gana" para proveedores que reportan `usage` acumulativo).
///
/// Los campos de caché se guardan CRUDOS, tal como los reporta cada
/// proveedor, sin normalizar ni restar de `input_tokens`. Cada familia
/// contabiliza la caché distinto (subconjunto del input vs. aparte); ese
/// conocimiento vive enteramente en `telemetry::pricing`, no acá.
#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    /// Tokens de entrada, exactos y crudos tal como los reporta el proveedor
    /// (puede incluir los de caché, según la familia: ver `pricing`).
    pub input_tokens: Option<u64>,
    /// Tokens de salida, exactos y crudos tal como los reporta el proveedor.
    pub output_tokens: Option<u64>,
    /// Tokens servidos desde caché (lectura, tarifa reducida). Crudo: cada
    /// familia decide si es subconjunto de `input_tokens` o va aparte.
    pub cache_read_tokens: Option<u64>,
    /// Tokens escritos a caché (creación, sobreprecio). Solo lo reportan
    /// algunos proveedores (p. ej. Anthropic); el resto lo deja en `None`.
    pub cache_write_tokens: Option<u64>,
}

/// Contrato que debe cumplir cada proveedor: dueño de ambas puntas del
/// dialecto, la ida (armar el request saliente) y la vuelta (leer el
/// `usage` de la respuesta).
pub trait Provider: Send + Sync {
    /// Nombre corto y estable del proveedor. Se usa como `upstream` en la
    /// métrica y en los mensajes de error de upstream.
    fn name(&self) -> &'static str;

    /// Construye el request saliente (URL, route, modelo, stream, body
    /// posiblemente mutado) a partir del request entrante.
    fn prepare(&self, incoming: Incoming, cfg: &AppConfig) -> Outgoing;

    /// Actualiza los contadores de tokens a partir de un valor JSON que
    /// contiene, en algún lado, el `usage` del proveedor. No hace nada si
    /// `value` no trae un `usage` reconocible para este proveedor.
    fn extract_usage(&self, value: &Value, usage: &mut Usage);
}

/// Huella no criptográfica (hash de 64 bits en hex) del body del request.
///
/// No busca resistencia a colisiones: solo queremos que "mismo prompt ⇒
/// misma huella" para detectar peticiones redundantes de forma barata.
pub fn fingerprint(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Lee `model` y `stream` de un body JSON (formato Anthropic messages,
/// OpenAI chat/completions y OpenAI Responses comparten esta forma). Si el
/// body no es JSON válido, devuelve `(None, false)`: el proveedor reenvía el
/// body intacto y no se miden tokens de ese request.
pub(crate) fn model_and_stream_from_body(raw: &[u8]) -> (Option<String>, bool) {
    match serde_json::from_slice::<Value>(raw) {
        Ok(v) => (
            v.get("model").and_then(|m| m.as_str()).map(str::to_string),
            v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
        ),
        Err(_) => (None, false),
    }
}
