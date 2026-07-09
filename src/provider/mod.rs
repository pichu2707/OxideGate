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
use serde::Serialize;
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
    /// `true` si `prepare` inyectó un breakpoint de `cache_control` a nivel
    /// raíz del body (palanca A del optimizador, solo Anthropic). Viaja hasta
    /// la métrica para correlacionar la inyección con los
    /// `cache_read_tokens` resultantes. `false` en el resto de los
    /// proveedores y en cualquier caso donde Anthropic no haya mutado nada.
    pub cache_control_forced: bool,
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

/// Descomposición del body de un request por componente, medida en BYTES.
///
/// Motivación: medimos que ~78% del costo del tráfico real es "maquinaria de
/// contexto" (releer y reescribir el prefijo del prompt: system, tools,
/// historial) y solo ~3% es input nuevo. `Outgoing::prompt_bytes` da un solo
/// número plano: sabemos que el body es grande, pero no QUÉ es grande. Este
/// tipo responde eso, componente por componente.
///
/// **CONTRATO DE MEDICIÓN — leer antes de usar este tipo:**
///
/// 1. Medimos BYTES, nunca tokens. Los proveedores solo reportan un TOTAL de
///    tokens (`usage.input_tokens`), jamás un desglose por componente.
///    Repartir ese total proporcionalmente a bytes asumiría que un esquema
///    de herramientas (JSON denso, mucha puntuación) tokeniza igual que
///    prosa natural, lo cual es falso: la relación bytes-por-token varía
///    según el contenido. Un conteo de bytes honesto vale más que un conteo
///    de tokens inventado (mismo principio que ya aplica el proyecto:
///    preferimos un hueco honesto a un cero falso).
/// 2. Cada campo se mide re-serializando el fragmento de JSON correspondiente
///    con `serde_json::to_vec(...).len()`. Eso es la longitud del JSON
///    CANÓNICO que produce `serde_json`, NO los bytes exactos que trajo el
///    cliente en el body original: no se preserva el espaciado ni el orden
///    de claves original. Por lo tanto `measured_bytes` en general va a
///    diferir levemente de `Outgoing::prompt_bytes` (que sí es el tamaño
///    exacto sobre el cable). Las razones (`ratio`) calculadas DENTRO de este
///    tipo son consistentes entre sí porque todos los componentes se miden
///    de la misma manera; nunca hay que mezclar `measured_bytes` con
///    `prompt_bytes` en un mismo cociente.
///
/// NOTA DE ALCANCE: este tipo y `Provider::decompose` son, a propósito, el
/// límite de ESTA porción de trabajo: describen y calculan el desglose por
/// proveedor pero todavía no se conectan a `RequestMetric`, `/requests` ni
/// `/stats` (esa integración es una porción separada). Por eso el binario en
/// producción todavía no los invoca fuera de los tests; no es código
/// abandonado, es la superficie pública que consumirá la próxima porción. Los
/// `#[allow(dead_code)]` de esta sección documentan justamente eso.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Default)]
#[allow(dead_code)]
pub struct ContextBreakdown {
    /// Bytes del prompt de sistema / instrucciones.
    pub system_bytes: usize,
    /// Bytes de los esquemas de herramientas.
    pub tools_bytes: usize,
    /// Bytes del historial: todos los mensajes MENOS el último.
    pub history_bytes: usize,
    /// Bytes del último mensaje: el turno nuevo que motiva esta petición.
    pub last_turn_bytes: usize,
    /// Bytes del resto de campos del body (model, temperature, max_tokens…).
    pub other_bytes: usize,
    /// Suma de los cinco campos anteriores.
    pub measured_bytes: usize,
    /// Número de mensajes del historial completo (incluyendo el último).
    pub messages_count: usize,
}

impl ContextBreakdown {
    /// Fracción del body que corresponde al PREFIJO ESTABLE que se
    /// re-envía y se re-lee en cada turno: `(system + tools + history) /
    /// measured`. Es el "impuesto de contexto": cuánta ceremonia estable
    /// (prompt de sistema, esquemas de herramientas, historial) paga cada
    /// request, medida sobre el total del body.
    ///
    /// `other_bytes` (campos de control a nivel raíz como `model`,
    /// `max_tokens`, `temperature`, `stream`) queda DELIBERADAMENTE FUERA
    /// del numerador: son metadata de transporte/control, no contexto,
    /// aunque también se reenvíen en cada turno. Sí permanece en el
    /// denominador, porque el denominador es el body medido completo.
    ///
    /// Por lo tanto esta ratio NO es simplemente `1 - last_turn / measured`:
    /// se cumple `context_tax_ratio + (last_turn + other) / measured == 1.0`.
    /// No "corregir" esto para que sea el complemento de `last_turn`: sería
    /// cambiar qué se mide, no un bug.
    ///
    /// `None` si `measured_bytes` es cero (nada medido: dividir daría `NaN`,
    /// y preferimos un hueco honesto a un cero falso) o si el cociente
    /// resultante no es finito (guarda defensiva; con `usize` no debería
    /// ocurrir, pero no confiamos en eso silenciosamente).
    #[allow(dead_code)] // ver "nota de alcance" en el doc de `ContextBreakdown`
    pub fn context_tax_ratio(&self) -> Option<f64> {
        if self.measured_bytes == 0 {
            return None;
        }
        let tax = self.system_bytes + self.tools_bytes + self.history_bytes;
        let ratio = tax as f64 / self.measured_bytes as f64;
        ratio.is_finite().then_some(ratio)
    }
}

/// Bytes de re-serializar un `Value` con `serde_json::to_vec`. Ver el
/// contrato de medición en [`ContextBreakdown`]: es longitud de JSON
/// canónico, no bytes de wire. Serializar puede fallar solo por errores de
/// tipos no soportados por `serde_json` (no aplica a `Value`, que siempre
/// serializa); igual no arriesgamos panic y devolvemos 0 en ese caso.
#[allow(dead_code)] // ver "nota de alcance" en el doc de `ContextBreakdown`
pub(crate) fn measure_value(value: &Value) -> usize {
    serde_json::to_vec(value).map(|b| b.len()).unwrap_or(0)
}

/// Bytes de la clave `key` dentro de `obj`, o `0` si la clave no está
/// presente. Usado para los campos "todo o nada" del desglose (`system`,
/// `tools`, `instructions`, `systemInstruction`).
#[allow(dead_code)] // ver "nota de alcance" en el doc de `ContextBreakdown`
pub(crate) fn measure_key(obj: &serde_json::Map<String, Value>, key: &str) -> usize {
    obj.get(key).map(measure_value).unwrap_or(0)
}

/// Suma en bytes de todas las claves de `obj` EXCEPTO las listadas en
/// `exclude`. Cubre el campo `other_bytes` del desglose: todo lo que no es
/// system/tools/historial (model, temperature, max_tokens, top_p…).
#[allow(dead_code)] // ver "nota de alcance" en el doc de `ContextBreakdown`
pub(crate) fn measure_other(obj: &serde_json::Map<String, Value>, exclude: &[&str]) -> usize {
    obj.iter()
        .filter(|(k, _)| !exclude.contains(&k.as_str()))
        .map(|(_, v)| measure_value(v))
        .sum()
}

/// Divide una secuencia de mensajes/turnos en `(history_bytes,
/// last_turn_bytes, count)`.
///
/// Regla compartida por Anthropic (`messages`), OpenAI Responses (`input`
/// como array) y Gemini (`contents`): todos los elementos MENOS el último
/// van a `history_bytes`, el último va a `last_turn_bytes`. Secuencia vacía
/// ⇒ `(0, 0, 0)`, sin pánic. Un solo elemento ⇒ `(0, bytes_del_elemento, 1)`.
///
/// Genérico sobre cualquier iterador de referencias a `Value` para que sirva
/// tanto con un slice directo (`&[Value]`) como con una selección filtrada
/// (p. ej. los mensajes de OpenAI Chat que no son `system`/`developer`).
#[allow(dead_code)] // ver "nota de alcance" en el doc de `ContextBreakdown`
pub(crate) fn split_history_and_last_turn<'a, I>(items: I) -> (usize, usize, usize)
where
    I: IntoIterator<Item = &'a Value>,
{
    let items: Vec<&Value> = items.into_iter().collect();
    let n = items.len();
    if n == 0 {
        return (0, 0, 0);
    }
    let history_bytes: usize = items[..n - 1].iter().map(|v| measure_value(v)).sum();
    let last_turn_bytes = measure_value(items[n - 1]);
    (history_bytes, last_turn_bytes, n)
}

/// Lee un campo array del body de forma tolerante: si la clave está ausente,
/// no es un array, o el valor no es JSON válido para este propósito,
/// devuelve un slice vacío en vez de entrar en pánico. Cubre `messages`
/// (Anthropic, OpenAI Chat), `contents` (Gemini) e `input`-como-array
/// (OpenAI Responses).
#[allow(dead_code)] // ver "nota de alcance" en el doc de `ContextBreakdown`
pub(crate) fn array_field<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> &'a [Value] {
    match obj.get(key) {
        Some(Value::Array(items)) => items.as_slice(),
        _ => &[],
    }
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

    /// Descompone el body de la petición por componente (ver
    /// [`ContextBreakdown`]). `None` si el body no es un objeto JSON o el
    /// dialecto no se reconoce (nunca hace panic).
    ///
    /// Sin implementación por defecto A PROPÓSITO: cada proveedor conoce su
    /// propio dialecto (dónde vive el system prompt, si hay un campo
    /// `messages` o una forma distinta) y debe decidir conscientemente cómo
    /// mapearlo. Un default que devolviera `None` en silencio dejaría pasar
    /// un proveedor nuevo sin desglose y nadie lo notaría hasta mirar los
    /// números en producción.
    ///
    /// COSTO: corre en el camino crítico del request, sobre bodies de hasta
    /// ~350 KB. Toma `&Value` (nunca clona el body completo) y solo
    /// re-serializa los fragmentos que necesita medir (`system`, `tools`,
    /// cada mensaje del historial): el costo es proporcional al tamaño de
    /// esos fragmentos, no al del body entero más de lo necesario.
    #[allow(dead_code)] // ver "nota de alcance" en el doc de `ContextBreakdown`
    fn decompose(&self, body: &Value) -> Option<ContextBreakdown>;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Un desglose completamente en cero (nada medido todavía) debe devolver
    /// `None` en la ratio, nunca `NaN`: dividir 0/0 en `f64` da `NaN`, que es
    /// justo lo que este método existe para evitar.
    #[test]
    fn context_tax_ratio_none_en_desglose_vacio() {
        let breakdown = ContextBreakdown::default();
        assert_eq!(breakdown.context_tax_ratio(), None);
    }

    /// Con una fracción conocida, `context_tax_ratio` debe devolver
    /// exactamente `(system + tools + history) / measured`.
    #[test]
    fn context_tax_ratio_calcula_la_fraccion_esperada() {
        let breakdown = ContextBreakdown {
            system_bytes: 100,
            tools_bytes: 50,
            history_bytes: 150,
            last_turn_bytes: 200,
            other_bytes: 0,
            measured_bytes: 500,
            messages_count: 4,
        };
        // (100 + 50 + 150) / 500 = 0.6
        assert!((breakdown.context_tax_ratio().unwrap() - 0.6).abs() < 1e-9);
    }

    /// Si el prefijo estable (system + tools + history) es cero, la ratio
    /// debe dar cero, no `None`: acá `measured_bytes` sí es positivo. La
    /// fixture deliberadamente deja `other_bytes = 20` de 320 bytes totales
    /// para probar que ese resto (metadata de control, no prefijo estable)
    /// no contamina el numerador: la ratio da 0.0 exacto, no 20/320.
    #[test]
    fn context_tax_ratio_cero_cuando_no_hay_prefijo_estable() {
        let breakdown = ContextBreakdown {
            system_bytes: 0,
            tools_bytes: 0,
            history_bytes: 0,
            last_turn_bytes: 300,
            other_bytes: 20,
            measured_bytes: 320,
            messages_count: 1,
        };
        assert_eq!(breakdown.context_tax_ratio(), Some(0.0));
    }

    /// `split_history_and_last_turn` sobre una secuencia vacía no debe hacer
    /// panic: debe devolver ceros limpios.
    #[test]
    fn split_history_and_last_turn_vacio_no_panica() {
        let items: Vec<Value> = vec![];
        assert_eq!(split_history_and_last_turn(items.iter()), (0, 0, 0));
    }

    /// Con un solo elemento, todo va a `last_turn_bytes` y no hay historial.
    #[test]
    fn split_history_and_last_turn_un_solo_elemento() {
        let items = [serde_json::json!({"role": "user", "content": "hola"})];
        let (history, last, count) = split_history_and_last_turn(items.iter());
        assert_eq!(history, 0);
        assert_eq!(count, 1);
        assert_eq!(last, measure_value(&items[0]));
    }

    /// Con varios elementos, todos menos el último van al historial.
    #[test]
    fn split_history_and_last_turn_varios_elementos() {
        let items = [
            serde_json::json!({"role": "user", "content": "uno"}),
            serde_json::json!({"role": "assistant", "content": "dos"}),
            serde_json::json!({"role": "user", "content": "tres"}),
        ];
        let (history, last, count) = split_history_and_last_turn(items.iter());
        assert_eq!(count, 3);
        assert_eq!(last, measure_value(&items[2]));
        assert_eq!(
            history,
            measure_value(&items[0]) + measure_value(&items[1])
        );
    }

    /// `array_field` sobre una clave ausente o de otro tipo devuelve slice
    /// vacío en vez de entrar en pánico.
    #[test]
    fn array_field_tolerante_a_ausente_o_tipo_incorrecto() {
        let obj = serde_json::json!({"messages": "no es un array", "other": 1});
        let obj = obj.as_object().unwrap();
        assert!(array_field(obj, "messages").is_empty());
        assert!(array_field(obj, "ausente").is_empty());
    }
}
