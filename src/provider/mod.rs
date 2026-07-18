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
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

pub use anthropic::ANTHROPIC;
pub use gemini::GEMINI;
pub use openai::{OPENAI_CHAT, OPENAI_CODEX_RESPONSES, OPENAI_RESPONSES};

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
    /// Valor crudo del header `Content-Encoding` del request entrante
    /// (p. ej. `"zstd"`, `"gzip"`), o `None` si el cliente no lo mandó.
    /// Nunca se usa para decidir qué se reenvía (`Outgoing::body` siempre
    /// viaja con `body` tal cual, comprimido o no): solo alimenta
    /// [`maybe_decompress`], que un `Provider::prepare` puede usar para medir
    /// telemetría (`prompt_hash`, `prompt_bytes`, `context`,
    /// `tools_by_server`) sobre el JSON LÓGICO en vez del wire comprimido.
    pub content_encoding: Option<String>,
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
    /// Desglose del body por componente (ver [`ContextBreakdown`]), calculado
    /// UNA sola vez en `prepare` a partir del mismo `Value` ya parseado que se
    /// usó para leer `model`/`stream` (y, si corresponde, para mutar el
    /// body). `None` si el body no parseó como JSON o no era un objeto: viaja
    /// tal cual hasta la métrica final.
    pub context: Option<ContextBreakdown>,
    /// Desglose de `tools` por servidor MCP (ver [`ToolServerBytes`]),
    /// calculado en `prepare` a partir del mismo `Value` ya parseado que
    /// `context` (nunca vuelve a llamar a [`parse_body`] ni clona el body).
    /// Vacío (`Vec::new()`) tanto si el body no parseó / no era un objeto,
    /// como si SÍ era un objeto pero no declaró herramientas (`tools`
    /// ausente, no-array, o `[]`): `Outgoing` no distingue por sí solo esos
    /// dos casos (para eso está `context`, que sí distingue "no pude ni
    /// mirar" de "miré y no había"). La métrica final
    /// (`telemetry::logger::RequestMetric::tools_by_server`) SÍ recupera esa
    /// distinción combinando este campo con `context.is_some()`.
    pub tools_by_server: Vec<ToolServerBytes>,
    /// Bytes de `tools` no atribuidos a ningún servidor (ver
    /// [`tools_overhead_bytes`]): `context.tools_bytes -
    /// suma(tools_by_server)`, calculado con ese mismo helper. `0` en los
    /// mismos casos donde `tools_by_server` queda vacío (nada que restar, o
    /// no hay `context` del que sacar `tools_bytes`).
    pub tools_overhead_bytes: usize,
    /// Nivel de esfuerzo de razonamiento pedido por el cliente
    /// (`output_config.effort`: `"low"` | `"medium"` | `"high"` | `"xhigh"` |
    /// `"max"`), leído del MISMO `Value` ya parseado en `prepare` (nunca un
    /// segundo parseo). Es una palanca de VELOCIDAD, no de coste: menos
    /// tokens de "thinking" ⇒ menos tiempo de generación, que es el 82% del
    /// tiempo ocupado medido en tráfico real (ver `docs/`). `None` si
    /// `output_config` está ausente, si `effort` está ausente dentro de él, o
    /// si `effort` no es un string (p. ej. un número) — nunca se inventa un
    /// valor a partir de un tipo inesperado.
    ///
    /// Dialecto de Anthropic únicamente: OpenAI y Gemini devuelven siempre
    /// `None` acá (ver la nota en sus respectivos `prepare`).
    pub requested_effort: Option<String>,
    /// Modo de velocidad pedido por el cliente: campo `speed` a nivel RAÍZ
    /// del body (no anidado, a diferencia de `effort`), valor `"fast"` en el
    /// beta de Anthropic (Opus 4.8 / 4.7). Hasta ~2.5x tokens de salida por
    /// segundo, a tarifa premium. `None` si `speed` está ausente en la raíz o
    /// no es un string.
    ///
    /// Dialecto de Anthropic únicamente: OpenAI y Gemini devuelven siempre
    /// `None` acá. Ver [`Usage::speed`] para el campo COMPLEMENTARIO del lado
    /// de la respuesta (qué velocidad sirvió REALMENTE el proveedor, que
    /// puede diferir de esta si el modo `fast` está rate-limiteado).
    pub requested_speed: Option<String>,
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
#[derive(Debug, Default, Clone)]
pub struct Usage {
    /// Tokens de entrada, exactos y crudos tal como los reporta el proveedor
    /// (puede incluir los de caché, según la familia: ver `pricing`).
    pub input_tokens: Option<u64>,
    /// Tokens de salida, exactos y crudos tal como los reporta el proveedor.
    pub output_tokens: Option<u64>,
    /// Tokens servidos desde caché (lectura, tarifa reducida). Crudo: cada
    /// familia decide si es subconjunto de `input_tokens` o va aparte.
    pub cache_read_tokens: Option<u64>,
    /// Tokens escritos a caché (creación). Lo reportan Anthropic (a sobreprecio,
    /// y se factura como tal) y la Responses API de OpenAI (que hoy lo manda en
    /// `0` y no se factura aparte, ver `pricing.rs`); el resto lo deja en `None`.
    pub cache_write_tokens: Option<u64>,
    /// Velocidad con la que el proveedor SIRVIÓ REALMENTE la respuesta
    /// (`usage.speed`, string), leída con la misma semántica "último gana"
    /// que el resto de los campos de `Usage`. Complementa a
    /// [`Outgoing::requested_speed`]: el modo `fast` de Anthropic tiene su
    /// propio rate limit, así que un request puede PEDIR `"fast"` y ser
    /// servido a velocidad `"standard"` — este campo es la única forma de
    /// saberlo.
    ///
    /// ESTADO: Anthropic DOCUMENTA este campo en `usage.speed`, pero a la
    /// fecha de este slice NUNCA se observó en tráfico real de este proyecto
    /// (el modo `fast` no se ejercitó todavía). `None` acá significa "el
    /// proveedor no lo reportó", NUNCA "sirvió a velocidad estándar": no hay
    /// forma de distinguir "campo ausente porque el proveedor no manda esta
    /// beta" de "campo ausente porque de verdad sirvió estándar" hasta que se
    /// observe el campo presente al menos una vez. Solo Anthropic lo llena;
    /// OpenAI y Gemini lo dejan siempre en `None`.
    pub speed: Option<String>,
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
/// Se calcula en `Provider::prepare` a partir del `Value` ya parseado del
/// body (una sola vez por request, ver [`parse_body`]) y viaja aplanado hasta
/// `RequestMetric` (`context_system_bytes`, `context_tools_bytes`, …, ver
/// `telemetry::logger`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Default)]
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
    ///
    /// NOTA DE ASIMETRÍA: cuando `measured_bytes == 0`, esta ratio es `None`
    /// mientras que los siete campos en bytes de `ContextBreakdown` quedan en
    /// `Some(0)` una vez aplanados en `RequestMetric` (ver
    /// `telemetry::logger::flatten_context_breakdown`). Es correcto y a
    /// propósito: "no medimos nada" (bytes en cero, sabido con certeza) es
    /// distinto de "no podemos calcular una fracción" (`None`, división por
    /// cero evitada).
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
pub(crate) fn measure_value(value: &Value) -> usize {
    serde_json::to_vec(value).map(|b| b.len()).unwrap_or(0)
}

/// Bytes de la clave `key` dentro de `obj`, o `0` si la clave no está
/// presente. Usado para los campos "todo o nada" del desglose (`system`,
/// `tools`, `instructions`, `systemInstruction`).
pub(crate) fn measure_key(obj: &serde_json::Map<String, Value>, key: &str) -> usize {
    obj.get(key).map(measure_value).unwrap_or(0)
}

/// Suma en bytes de todas las claves de `obj` EXCEPTO las listadas en
/// `exclude`. Cubre el campo `other_bytes` del desglose: todo lo que no es
/// system/tools/historial (model, temperature, max_tokens, top_p…).
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
pub(crate) fn array_field<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> &'a [Value] {
    match obj.get(key) {
        Some(Value::Array(items)) => items.as_slice(),
        _ => &[],
    }
}

/// Etiqueta para herramientas NATIVAS: nombres que no siguen el patrón
/// `mcp__<server>__<tool>`, o que empiezan con `mcp__` pero no tienen un
/// segundo separador `__` válido (ver [`server_of`]). Un `name` faltante o
/// no-string NUNCA cae acá: se omite en [`Provider::tool_entries`] antes de
/// llegar a este punto, para no inflar el bucket nativo con datos ajenos.
const NATIVE_TOOLS_LABEL: &str = "(native)";

/// Etiqueta del bucket de desborde de [`group_tools_by_server`]: servidores
/// MCP distintos que aparecen después de agotar el cupo [`MAX_TOOL_SERVERS`].
const OTHERS_LABEL: &str = "(others)";

/// Tope de servidores MCP distintos que [`group_tools_by_server`] trackea de
/// forma INDIVIDUAL dentro de un mismo request.
///
/// El body es entrada controlada por quien llama al proxy: cualquier cliente
/// puede mandar nombres de herramienta arbitrarios, y agrupar en un
/// `HashMap` keyeado por un substring de ese body — sin cota — es un vector
/// de crecimiento de memoria en el camino crítico del request. Mismo
/// espíritu que `MAX_DISTINCT_PROMPTS_PER_MODEL` en `telemetry::stats`:
/// preferimos una cota honesta y documentada a un OOM.
///
/// A diferencia de aquel cap (que SATURA: deja de admitir huellas nuevas y
/// marca el resultado como cota inferior), acá el desborde SIGUE contándose:
/// todo servidor más allá del cupo colapsa en un único bucket
/// [`OTHERS_LABEL`], así que la cantidad de herramientas y los bytes
/// reportados por [`group_tools_by_server`] siempre suman el total exacto de
/// la entrada — se pierde el desglose fino más allá del cupo, nunca un byte
/// ni una herramienta.
const MAX_TOOL_SERVERS: usize = 32;

/// Naturaleza del cubo al que se atribuye una herramienta. Distingue por
/// TIPO, no por una cadena mágica: un servidor MCP llamado literalmente
/// `(native)` (o `(others)`) es un servidor MCP, no una herramienta nativa
/// ni el bucket de desborde, aunque su nombre de display coincida con el
/// sentinel. [`group_tools_by_server`] keyea su mapa por `(ToolServerKind,
/// &str)`, así que dos filas con el mismo `server` mostrado pero distinto
/// `kind` NUNCA se fusionan.
///
/// Orden total (`Ord` derivado del orden de declaración de variantes, usado
/// como desempate final en [`group_tools_by_server`] cuando dos filas
/// empatan en bytes Y en nombre de servidor): `Native < Mcp < Others`. Se
/// eligió ese orden porque refleja "especificidad decreciente": `Native` es
/// el único cubo con un origen fijo y sin nombre de servidor real; `Mcp` son
/// servidores concretos identificados por el cliente; `Others` es
/// enteramente sintético (producto del cupo agotado, sin identidad propia),
/// así que va último.
///
/// Serializa en minúsculas (`"native"`, `"mcp"`, `"others"`) vía
/// `#[serde(rename_all = "lowercase")]`: es la forma que consume
/// `RequestMetric::tools_by_server` en el JSONL y cualquier UI que lo lea.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolServerKind {
    /// Herramienta nativa: nombre que no sigue el patrón `mcp__<server>__<tool>`.
    Native,
    /// Herramienta declarada por un servidor MCP identificado en el nombre.
    Mcp,
    /// Bucket de desborde: servidor MCP distinto que apareció después de
    /// agotar [`MAX_TOOL_SERVERS`].
    Others,
}

/// Bytes de las herramientas del body agrupadas por servidor MCP que las
/// declara. Ver [`Provider::tools_by_server`] y [`group_tools_by_server`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolServerBytes {
    /// Servidor propietario. `mcp__claude_ai_Gmail` -> `claude_ai_Gmail`.
    /// Las herramientas nativas (sin prefijo `mcp__`) caen en `NATIVE_TOOLS_LABEL`.
    /// Nombre de DISPLAY solamente: dos filas pueden compartir este valor
    /// (p. ej. un servidor MCP llamado literalmente `(native)` y el bucket
    /// nativo genuino) y aun así ser cubos distintos; usar [`Self::kind`]
    /// para distinguirlos, nunca comparar solo por `server`.
    pub server: String,
    /// Tipo de cubo (nativo, servidor MCP identificado, o desborde). Ver
    /// [`ToolServerKind`].
    pub kind: ToolServerKind,
    /// Cantidad de herramientas atribuidas a este servidor.
    pub tools: usize,
    /// Suma de los bytes de cada herramienta de este servidor.
    pub bytes: usize,
    /// Cuántas de las `tools` de ESTE servidor traían `defer_loading: true`
    /// en su propia definición dentro del body ENTRANTE. OBSERVACIÓN PURA,
    /// leída tool por tool en [`group_tools_by_server`] — nunca una
    /// inferencia ni una decisión del proxy (OxideGate no implementa
    /// todavía la mutación de `defer_loading`, ver
    /// `docs/optimizer-tool-search.md` §4).
    ///
    /// - `deferred_tools == tools`: servidor totalmente diferido.
    /// - `deferred_tools == 0`: servidor NADA diferido — sus `bytes` son
    ///   reales y desconectables.
    /// - `0 < deferred_tools < tools`: diferido PARCIAL.
    ///
    /// **DOMINIO: tokens de contexto, no bytes de cable.** Este campo
    /// registra si la definición trae la marca `defer_loading` — un dato
    /// sobre lo que el CLIENTE declaró en el body, nunca sobre cuántos bytes
    /// viajaron por el cable. `defer_loading: true` en una tool no implica
    /// que sus bytes se hayan ahorrado en el request (el mecanismo de la API
    /// AÑADE, no retiene — ver `docs/optimizer-tool-search.md` §2.2): la
    /// definición marcada sigue viajando completa, y el ahorro real (si lo
    /// hay) es de tokens de contexto en el modelo, no de bytes en el body de
    /// ESTA petición. Un consumidor que lea `deferred_tools > 0` y concluya
    /// "estos bytes no viajaron" comete el mismo error que este proyecto ya
    /// midió y corrigió una vez (`docs/optimizer-tool-search.md` §3.2): no
    /// mezclar la marca con el ahorro de cable.
    pub deferred_tools: usize,
}

/// Clasifica `tool_name` en `(ToolServerKind, servidor_o_sentinel)`. Pura: no
/// mide bytes, solo parsea el nombre. Fuente única de verdad del parseo:
/// [`server_of`] delega acá y descarta el `ToolServerKind`.
///
/// Los nombres de herramienta MCP siguen el patrón `mcp__<server>__<tool>`.
/// El nombre de la herramienta en sí puede contener `__` (p. ej.
/// `mcp__srv__do__thing`, donde la herramienta es `do__thing`), así que NO
/// alcanza con partir por TODOS los `__`: hace falta el equivalente de
/// `splitn(3, "__")`, donde el primer segmento debe ser literalmente
/// `"mcp"`, el segundo es el servidor, y el tercero es "todo lo demás" (la
/// herramienta, sin volver a partir aunque contenga `__`).
///
/// Casos borde, decididos y probados en `tests::server_of_casos_borde`:
/// - `"mcp__"` (no hay tercer segmento tras el segundo `__`): nativa.
/// - `"mcp__srv"` (sin segundo `__` en absoluto): nativa. Un nombre que
///   empieza con `mcp__` pero no tiene un segundo separador NO es un nombre
///   MCP válido (mismo caso que el `mcp__weird` del contrato de la tarea).
/// - `"mcp__srv__"` (segundo `__` SÍ presente, herramienta vacía): SÍ cuenta
///   como MCP válido, servidor `"srv"`, herramienta `""`. El separador está
///   presente; que el nombre de la herramienta quede vacío no invalida al
///   servidor.
/// - `"__x__y"` (no empieza con el literal `mcp__`, el primer segmento antes
///   del primer `__` es la cadena vacía, no `"mcp"`): nativa.
/// - `""`: nativa (no hay ni siquiera un primer segmento `"mcp"`).
///
/// IMPORTANTE (ver [`ToolServerKind`]): que el segmento de servidor sea
/// literalmente `"(native)"` o `"(others)"` NO lo convierte en nativo ni en
/// desborde; sigue siendo `Mcp` con ese nombre de servidor. La colisión de
/// cadenas de display se resuelve en [`group_tools_by_server`], que keyea
/// por el `ToolServerKind` devuelto acá, no por el string solo.
pub fn classify(tool_name: &str) -> (ToolServerKind, &str) {
    let mut segments = tool_name.splitn(3, "__");
    match (segments.next(), segments.next(), segments.next()) {
        (Some("mcp"), Some(server), Some(_)) if !server.is_empty() => {
            (ToolServerKind::Mcp, server)
        }
        _ => (ToolServerKind::Native, NATIVE_TOOLS_LABEL),
    }
}

/// Servidor MCP dueño de `tool_name`, o [`NATIVE_TOOLS_LABEL`] si no se
/// reconoce ninguno. Pura: no mide bytes, solo clasifica el nombre.
///
/// Envoltorio de compatibilidad sobre [`classify`], que conserva el
/// `ToolServerKind`: `server_of` existía antes de distinguir por tipo y su
/// contrato documentado (solo el segmento, sin tipo) se mantiene tal cual
/// para quien ya lo use solo para display. [`group_tools_by_server`] usa
/// `classify` directamente, no `server_of`, porque necesita el tipo para no
/// colisionar buckets.
///
/// NOTA DE ALCANCE: a diferencia del resto de los ítems de este bloque,
/// `server_of` SIGUE sin consumidor en `main()` incluso después de este
/// slice de wiring: ningún proveedor ni ninguna capa de telemetría lo llama,
/// solo lo ejercitan los tests (`server_of_casos_borde`). Se conserva
/// `#[allow(dead_code)]`, a diferencia de sus vecinos, porque de verdad no
/// tiene consumidor todavía — no es un descuido, es el único ítem de este
/// módulo del que eso sigue siendo cierto.
#[allow(dead_code)]
pub fn server_of(tool_name: &str) -> &str {
    classify(tool_name).1
}

/// Agrupa herramientas por servidor MCP, midiendo cada una con
/// [`measure_value`]. Compartido por los cuatro dialectos: una vez que cada
/// proveedor produce sus `(nombre, valor)` vía [`Provider::tool_entries`], el
/// agrupamiento es idéntico para todos — no hay conocimiento de dialecto acá
/// adentro.
///
/// Orden de salida DETERMINÍSTICO: bytes DESCENDENTE, empatando por nombre de
/// servidor ASCENDENTE y, si TAMBIÉN empatan en nombre (posible ahora que
/// `Native`/`Others` pueden compartir display con un `Mcp` homónimo, ver
/// [`ToolServerKind`]), por `kind` según el orden total documentado en
/// [`ToolServerKind`]. Los tests dependen de este orden, y también lo hará
/// cualquier UI futura que liste estos totales.
///
/// Cupo: hasta [`MAX_TOOL_SERVERS`] cubos `(ToolServerKind, servidor)`
/// distintos se trackean de forma individual (por orden de aparición); el
/// resto colapsa en el bucket [`ToolServerKind::Others`] /
/// [`OTHERS_LABEL`]. La cantidad de herramientas y la suma de bytes del
/// resultado siempre suman exactamente el total de la entrada (ver
/// [`MAX_TOOL_SERVERS`] para la comparación con el cap de
/// `telemetry::stats`). El bucket nativo cuenta contra este mismo cupo,
/// igual que cualquier servidor MCP: si el cupo ya se agotó antes de ver la
/// primera herramienta nativa, esa herramienta también colapsa en
/// `Others` — no es un caso especial.
///
/// Toma un iterador de referencias (nunca clona `body` ni los `Value` de
/// cada herramienta): el costo es proporcional a los fragmentos que mide,
/// no al body entero.
///
/// COSTO DE ALOCACIÓN: la clave interna del acumulador es `(ToolServerKind,
/// &'a str)` — un slice TOMADO PRESTADO de `name` (o el `&'static str` de
/// [`NATIVE_TOOLS_LABEL`]/[`OTHERS_LABEL`]), nunca un `String` nuevo por
/// herramienta. Solo se aloca un `String` una vez por FILA de salida (al
/// construir cada `ToolServerBytes`), nunca dentro del loop por-herramienta:
/// un body con 76 herramientas de 1 solo servidor hace 1 alocación de
/// `String`, no 76.
///
/// Además de bytes y cantidad, cada tool se inspecciona por su propia clave
/// `defer_loading` (ver [`ToolServerBytes::deferred_tools`]): si vale
/// literalmente `true`, cuenta para el servidor al que esa tool pertenece.
/// Es una lectura genérica sobre CUALQUIER `Value` de tool, sin conocimiento
/// de dialecto: Anthropic es el único que declara esa clave en la práctica
/// (`docs/optimizer-tool-search.md` §8), así que en OpenAI/Gemini —cuyas
/// tools nunca traen `defer_loading`— este conteo da `0` para todos los
/// servidores, sin necesitar un `if` por proveedor: alcanzado por ausencia
/// estructural del campo, no por un valor forzado.
pub fn group_tools_by_server<'a>(
    entries: impl Iterator<Item = (&'a str, &'a Value)>,
) -> Vec<ToolServerBytes> {
    let mut totals: HashMap<(ToolServerKind, &'a str), (usize, usize, usize)> = HashMap::new();

    for (name, value) in entries {
        let bytes = measure_value(value);
        let deferred = value
            .get("defer_loading")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let (kind, raw_server) = classify(name);

        let key = if totals.contains_key(&(kind, raw_server)) || totals.len() < MAX_TOOL_SERVERS {
            (kind, raw_server)
        } else {
            (ToolServerKind::Others, OTHERS_LABEL)
        };

        let entry = totals.entry(key).or_insert((0, 0, 0));
        entry.0 += 1;
        entry.1 += bytes;
        if deferred {
            entry.2 += 1;
        }
    }

    let mut rows: Vec<ToolServerBytes> = totals
        .into_iter()
        .map(|((kind, server), (tools, bytes, deferred_tools))| ToolServerBytes {
            server: server.to_string(),
            kind,
            tools,
            bytes,
            deferred_tools,
        })
        .collect();

    rows.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then_with(|| a.server.cmp(&b.server))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    rows
}

/// Bytes del array `tools` que no se atribuyen a ninguna fila de
/// `by_server`. `tools_bytes - sum(bytes por servidor)`.
///
/// TRES contribuyentes distintos caen acá, no uno solo:
/// 1. Estructura JSON del array en sí: los corchetes `[` `]` y las comas
///    separadoras entre elementos (aplica a los cuatro dialectos).
/// 2. Envoltorios sin atribución propia: en Gemini, [`Provider::tool_entries`]
///    mide las declaraciones INDIVIDUALES (`functionDeclarations[i]`), nunca
///    el objeto wrapper que las contiene (`{"functionDeclarations": [...]}`).
///    Los bytes de la clave `"functionDeclarations"`, sus corchetes de array
///    propios y las llaves `{...}` de cada wrapper no pertenecen a ninguna
///    declaración individual y por lo tanto no están en `by_server`: caen
///    acá. Anthropic/OpenAI no tienen este contribuyente porque cada
///    herramienta ES el elemento del array `tools`, sin wrapper intermedio.
/// 3. Herramientas huérfanas: una entrada sin `name` (o con `name` no-string)
///    se omite por completo en [`Provider::tool_entries`] (ver su contrato:
///    nunca se atribuye a [`NATIVE_TOOLS_LABEL`] para no inflarlo con datos
///    ajenos), así que sus bytes tampoco están en `by_server` y también
///    quedan absorbidos acá.
///
/// NO puede ir legítimamente negativo: `by_server` se construye midiendo
/// FRAGMENTOS del mismo array cuyo total serializado es `tools_bytes` (cada
/// fragmento individual pesa menos que el array completo que la contiene),
/// así que la resta siempre debería dar `>= 0`. Aun así usamos
/// `saturating_sub` en vez de una resta directa: preferimos devolver `0` a
/// entrar en pánico si algún día esa invariante se rompe (p. ej. un cambio
/// futuro que mida `by_server` con otra fuente de bytes que no sea
/// `tools_bytes`).
///
/// La aritmética no cambia por documentar estos tres contribuyentes: sigue
/// siendo la misma resta de siempre, solo se precisa QUÉ compone el
/// resultado.
pub fn tools_overhead_bytes(tools_bytes: usize, by_server: &[ToolServerBytes]) -> usize {
    let attributed: usize = by_server.iter().map(|s| s.bytes).sum();
    tools_bytes.saturating_sub(attributed)
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
    ///
    /// `body` debe ser el `Value` que ya devolvió [`parse_body`] para este
    /// mismo request: `decompose` nunca vuelve a parsear bytes crudos.
    fn decompose(&self, body: &Value) -> Option<ContextBreakdown>;

    /// Devuelve `(nombre, valor)` de cada herramienta declarada en el body.
    /// `None` si el body no es un objeto o el dialecto no declara
    /// herramientas.
    ///
    /// Sin implementación por defecto A PROPÓSITO (mismo criterio que
    /// `decompose`): cada proveedor sabe dónde viven sus nombres de
    /// herramienta (`tools[].name`, `tools[].function.name`,
    /// `functionDeclarations[].name`…) y debe decidirlo conscientemente. Un
    /// default que devolviera `None` en silencio dejaría pasar un proveedor
    /// nuevo sin desglose por servidor y nadie lo notaría hasta mirar los
    /// números en producción.
    ///
    /// CONTRATO sobre `tools` ausente vs. vacío: `tools` ausente ⇒ `None`
    /// (el dialecto no declaró NADA de herramientas para este request).
    /// `tools: []` ⇒ `Some(vec![])` (SÍ declaró herramientas, son cero): no
    /// son el mismo caso y no deben confundirse.
    ///
    /// Una herramienta sin `name` (o con `name` que no es string) se OMITE
    /// de la lista devuelta, nunca se atribuye a [`NATIVE_TOOLS_LABEL`]:
    /// atribuirla ahí inflaría el bucket nativo con datos que no le
    /// pertenecen.
    ///
    /// Nunca clona `body`: toma `&Value` y devuelve referencias con el mismo
    /// lifetime, igual que el resto de las funciones de este módulo.
    fn tool_entries<'a>(&self, body: &'a Value) -> Option<Vec<(&'a str, &'a Value)>>;

    /// Desglosa `tools` por servidor MCP. Vacío si el body no declara
    /// herramientas (`tool_entries` devuelve `None`).
    ///
    /// Implementación por defecto SÍ disponible (a diferencia de
    /// `decompose` y `tool_entries`): una vez que el proveedor dice DÓNDE
    /// están sus herramientas, agruparlas por servidor es exactamente la
    /// misma operación para los cuatro dialectos
    /// ([`group_tools_by_server`]) — no hay conocimiento de dialecto que
    /// decidir acá.
    fn tools_by_server(&self, body: &Value) -> Vec<ToolServerBytes> {
        match self.tool_entries(body) {
            Some(entries) => group_tools_by_server(entries.into_iter()),
            None => Vec::new(),
        }
    }
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

/// Parsea el body crudo a un `Value` JSON. Punto de entrada ÚNICO para pasar
/// de bytes a `Value` en el camino de `prepare`: cada proveedor lo llama
/// EXACTAMENTE UNA VEZ por request, y reutiliza el `Value` resultante (por
/// referencia) para leer `model`/`stream`, para `decompose` y, si hace falta
/// mutar el body (Anthropic `force_cache_control`, OpenAI `stream_options`),
/// para esa mutación también. `None` si `raw` no es JSON válido; nunca hace
/// panic.
///
/// El tipo de retorno (`Option<Value>`, no `&[u8]`) es lo que hace estructural
/// evitar un segundo parseo accidental: una vez que se tiene el `Value`, ya
/// no hace falta volver a tocar los bytes crudos para nada relacionado con
/// modelo/stream/desglose/mutación. Esto NO es una garantía del compilador:
/// nada impide que un `prepare` futuro llame a `parse_body` una segunda vez
/// sobre el mismo `raw`; la garantía es de diseño (un solo `let parsed =
/// parse_body(...)` por `prepare`, reutilizado por referencia), no de tipos.
pub(crate) fn parse_body(raw: &[u8]) -> Option<Value> {
    serde_json::from_slice::<Value>(raw).ok()
}

/// Descomprime `body` para medición LÓGICA (telemetría), nunca para lo que
/// se reenvía al proveedor: `Outgoing::body` siempre viaja con el body
/// crudo tal cual llegó del cliente, comprimido o no (ver el contrato en
/// [`Incoming::content_encoding`]).
///
/// Reconoce dos codificaciones, comparando `encoding` insensible a
/// mayúsculas (`"zstd"`/`"ZSTD"`/`"Zstd"` son equivalentes):
/// - `"zstd"` (RFC 8878): la que manda la Responses API de Codex.
/// - `"gzip"`: la otra codificación de contenido habitual en HTTP.
///
/// Cualquier otro valor de `encoding` (`"br"`, `"identity"`, un dialecto que
/// este proxy todavía no sabe medir) o `None` (el cliente no mandó
/// `Content-Encoding`) devuelve una COPIA de `body` sin tocar.
///
/// **FALLBACK SEGURO, nunca panic**: si la descompresión falla — el body
/// está corrupto, truncado, o declarado zstd/gzip pero en realidad no lo
/// es — esta función NO propaga el error: devuelve una copia de `body` tal
/// cual. Preferimos medir sobre bytes crudos (mal etiquetados como
/// "descomprimidos" pero al menos presentes) a abortar la medición de todo
/// el request por un body mal formado. Mismo espíritu que [`parse_body`]
/// (`Option`, nunca `Result` propagado) y que el resto de este módulo:
/// preferimos un hueco honesto (medir sobre lo que hay) a un panic.
pub fn maybe_decompress(body: &[u8], encoding: Option<&str>) -> Vec<u8> {
    match encoding.map(str::to_ascii_lowercase).as_deref() {
        Some("zstd") => zstd::decode_all(body).unwrap_or_else(|_| body.to_vec()),
        Some("gzip") => {
            let mut out = Vec::new();
            let mut decoder = flate2::read::GzDecoder::new(body);
            match std::io::Read::read_to_end(&mut decoder, &mut out) {
                Ok(_) => out,
                Err(_) => body.to_vec(),
            }
        }
        _ => body.to_vec(),
    }
}

/// Lee `model` y `stream` de un `Value` YA PARSEADO (formato Anthropic
/// messages, OpenAI chat/completions y OpenAI Responses comparten esta
/// forma). Si `value` no trae esas claves (o no es un objeto), cada campo
/// cae a su default (`None`/`false`); nunca hace panic.
///
/// Toma `&Value`, no bytes crudos: el parseo ya ocurrió en [`parse_body`].
/// Cuando `parse_body` devuelve `None` (body no-JSON), el llamador usa
/// `(None, false)` directamente sin invocar esta función.
pub(crate) fn model_and_stream_from_value(value: &Value) -> (Option<String>, bool) {
    (
        value.get("model").and_then(|m| m.as_str()).map(str::to_string),
        value.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
    )
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

    /// `server_of` sobre todos los casos borde documentados: presencia y
    /// ausencia del segundo separador `__`, nombre que empieza con `mcp__`
    /// pero le falta un segmento, cadena vacía, y una herramienta cuyo
    /// nombre propio contiene `__` (debe ignorarse para la clasificación:
    /// el tercer segmento de `splitn(3, "__")` no se vuelve a partir).
    #[test]
    fn server_of_casos_borde() {
        assert_eq!(
            server_of("mcp__claude_ai_Gmail__search_threads"),
            "claude_ai_Gmail"
        );
        // El nombre de la herramienta contiene "__": debe ir entero al
        // tercer segmento, sin afectar la detección del servidor.
        assert_eq!(server_of("mcp__srv__do__thing"), "srv");
        assert_eq!(server_of("Read"), NATIVE_TOOLS_LABEL);
        assert_eq!(server_of("mcp__"), NATIVE_TOOLS_LABEL);
        assert_eq!(server_of("mcp__srv"), NATIVE_TOOLS_LABEL);
        // Segundo "__" SÍ presente (aunque la herramienta quede vacía): es
        // un nombre MCP válido con servidor "srv".
        assert_eq!(server_of("mcp__srv__"), "srv");
        assert_eq!(server_of("__x__y"), NATIVE_TOOLS_LABEL);
        assert_eq!(server_of(""), NATIVE_TOOLS_LABEL);
    }

    /// Iterador vacío ⇒ vector vacío, sin panic.
    #[test]
    fn group_tools_by_server_vacio_para_iterador_vacio() {
        let entries: Vec<(&str, &Value)> = vec![];
        assert!(group_tools_by_server(entries.into_iter()).is_empty());
    }

    /// Orden determinístico: bytes descendente y, en caso de empate,
    /// servidor ascendente. Se fuerza el empate con dos nombres de la MISMA
    /// longitud ("zebra"/"alpha", 5 letras cada uno) y el mismo padding, y
    /// se verifica primero que en efecto midieron igual (para que el test
    /// no dependa de una casualidad no verificada).
    #[test]
    fn group_tools_by_server_orden_deterministico_con_empate() {
        let tool_zebra = serde_json::json!({"name": "mcp__zebra__x", "padding": "1234"});
        let tool_alpha = serde_json::json!({"name": "mcp__alpha__y", "padding": "1234"});
        assert_eq!(measure_value(&tool_zebra), measure_value(&tool_alpha));

        let entries = vec![
            ("mcp__zebra__x", &tool_zebra),
            ("mcp__alpha__y", &tool_alpha),
        ];
        let rows = group_tools_by_server(entries.into_iter());

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].server, "alpha");
        assert_eq!(rows[1].server, "zebra");
    }

    /// `classify` distingue por tipo, no solo por segmento: un servidor MCP
    /// cuyo nombre coincide textualmente con un sentinel sigue siendo `Mcp`.
    #[test]
    fn classify_distingue_kind_de_string_de_display() {
        assert_eq!(
            classify("mcp__claude_ai_Gmail__search_threads"),
            (ToolServerKind::Mcp, "claude_ai_Gmail")
        );
        assert_eq!(classify("Read"), (ToolServerKind::Native, NATIVE_TOOLS_LABEL));
        // Un servidor MCP llamado literalmente "(native)" es Mcp, NUNCA Native.
        assert_eq!(
            classify("mcp__(native)__thing"),
            (ToolServerKind::Mcp, "(native)")
        );
        // Mismo razonamiento con el sentinel de desborde.
        assert_eq!(
            classify("mcp__(others)__thing"),
            (ToolServerKind::Mcp, "(others)")
        );
    }

    /// PRUEBA DE MORDIDA (bug real, no hipotético): un servidor MCP cuyo
    /// segmento es literalmente el sentinel de nativas (`(native)`) NO debe
    /// fusionarse con el bucket de herramientas nativas genuinas. Antes del
    /// fix, ambas entradas colapsaban en una sola fila porque
    /// `group_tools_by_server` keyeaba por la cadena de display, no por tipo
    /// de origen (verificado: este test FALLA contra el código pre-fix con
    /// `rows.len() == 1`). Con el fix, deben ser DOS filas con el mismo
    /// `server` mostrado pero `kind` distinto.
    #[test]
    fn group_tools_by_server_native_y_mcp_homonimo_no_colisionan() {
        let read = serde_json::json!({"name": "Read"});
        let homonimo = serde_json::json!({"name": "mcp__(native)__thing"});
        let entries = vec![("Read", &read), ("mcp__(native)__thing", &homonimo)];

        let rows = group_tools_by_server(entries.into_iter());

        assert_eq!(
            rows.len(),
            2,
            "un servidor MCP llamado (native) no debe fusionarse con el bucket nativo genuino"
        );
        assert!(rows.iter().all(|r| r.server == NATIVE_TOOLS_LABEL));
        assert!(rows.iter().any(|r| r.kind == ToolServerKind::Native));
        assert!(rows.iter().any(|r| r.kind == ToolServerKind::Mcp));
    }

    /// Mismo bug, versión `(others)`: un servidor MCP real llamado
    /// literalmente `(others)` (tracked individualmente, sin desbordar) y un
    /// desborde GENUINO (un servidor 33.° distinto tras agotar el cupo) no
    /// deben fusionarse solo porque ambos muestran `"(others)"` como
    /// `server`. Antes del fix colapsaban en una sola fila (misma clave de
    /// `String` para ambos); con el fix son dos filas con `kind` distinto.
    #[test]
    fn group_tools_by_server_others_literal_y_desborde_genuino_no_colisionan() {
        let literal_others = serde_json::json!({"name": "mcp__(others)__x"});
        let mut entries: Vec<(&str, &Value)> = vec![("mcp__(others)__x", &literal_others)];

        // 31 servidores reales más para completar el cupo de 32 junto con el
        // servidor literal "(others)" de arriba.
        let names: Vec<String> = (0..31).map(|i| format!("mcp__srv{i:02}__tool")).collect();
        let values: Vec<Value> = (0..31).map(|i| serde_json::json!({"n": i})).collect();
        entries.extend(names.iter().zip(values.iter()).map(|(n, v)| (n.as_str(), v)));

        // Servidor 33.°, distinto de todos los anteriores: cupo ya agotado
        // (32 trackeados), así que desborda genuinamente a `Others`.
        let overflow_tool = serde_json::json!({"name": "mcp__overflow_srv__tool"});
        entries.push(("mcp__overflow_srv__tool", &overflow_tool));

        let rows = group_tools_by_server(entries.into_iter());

        // 32 trackeados individualmente (incluye el literal "(others)") + 1
        // bucket de desborde genuino.
        assert_eq!(rows.len(), MAX_TOOL_SERVERS + 1);

        let others_rows: Vec<&ToolServerBytes> =
            rows.iter().filter(|r| r.server == OTHERS_LABEL).collect();
        assert_eq!(
            others_rows.len(),
            2,
            "el servidor MCP literal (others) y el desborde genuino deben ser filas separadas"
        );
        assert!(others_rows.iter().any(|r| r.kind == ToolServerKind::Mcp));
        assert!(others_rows.iter().any(|r| r.kind == ToolServerKind::Others));
    }

    /// Más de `MAX_TOOL_SERVERS` servidores distintos: el desborde colapsa
    /// en `OTHERS_LABEL`, pero la cantidad de herramientas y la suma de
    /// bytes deben seguir cerrando exactamente con el total de la entrada
    /// (nunca se pierde un byte ni una herramienta, solo el desglose fino).
    #[test]
    fn group_tools_by_server_desborda_a_others_con_40_servidores() {
        let names: Vec<String> = (0..40).map(|i| format!("mcp__srv{i:02}__tool")).collect();
        let values: Vec<Value> = (0..40).map(|i| serde_json::json!({"n": i})).collect();
        let entries: Vec<(&str, &Value)> = names
            .iter()
            .zip(values.iter())
            .map(|(n, v)| (n.as_str(), v))
            .collect();

        let total_tools = entries.len();
        let total_bytes: usize = entries.iter().map(|(_, v)| measure_value(v)).sum();

        let rows = group_tools_by_server(entries.into_iter());

        // 32 servidores reales trackeados individualmente + 1 bucket de
        // desborde para los 8 restantes.
        assert_eq!(rows.len(), MAX_TOOL_SERVERS + 1);
        let others = rows
            .iter()
            .find(|r| r.server == OTHERS_LABEL)
            .expect("debe existir el bucket de desborde");
        assert_eq!(others.tools, 40 - MAX_TOOL_SERVERS);

        let summed_tools: usize = rows.iter().map(|r| r.tools).sum();
        let summed_bytes: usize = rows.iter().map(|r| r.bytes).sum();
        assert_eq!(summed_tools, total_tools);
        assert_eq!(summed_bytes, total_bytes);
    }

    // -----------------------------------------------------------------
    // ToolServerBytes::deferred_tools — el campo que corrige la conflación
    // de un booleano body-wide que este proyecto tuvo y eliminó
    // (`client_defer_loading`, `docs/optimizer-tool-search.md`, defecto
    // encontrado en revisión adversarial ronda 3): un booleano body-wide no
    // puede afirmar nada sobre UN servidor puntual. Estos tres tests son el
    // contrato mínimo: un servidor totalmente diferido, uno nada diferido, y
    // el caso que de verdad importa — un body MIXTO donde ambos coexisten.
    // -----------------------------------------------------------------

    /// Todas las tools de un servidor traen `defer_loading: true`:
    /// `deferred_tools` debe igualar `tools` exactamente.
    #[test]
    fn deferred_tools_servidor_totalmente_diferido() {
        let entries_json = serde_json::json!([
            {"name": "mcp__srv__a", "defer_loading": true},
            {"name": "mcp__srv__b", "defer_loading": true},
            {"name": "mcp__srv__c", "defer_loading": true}
        ]);
        let tools = entries_json.as_array().unwrap();
        let entries: Vec<(&str, &Value)> = tools
            .iter()
            .map(|t| (t.get("name").unwrap().as_str().unwrap(), t))
            .collect();

        let rows = group_tools_by_server(entries.into_iter());

        assert_eq!(rows.len(), 1);
        let srv = &rows[0];
        assert_eq!(srv.server, "srv");
        assert_eq!(srv.tools, 3);
        assert_eq!(srv.deferred_tools, 3, "totalmente diferido: deferred_tools == tools");
    }

    /// Ninguna tool del servidor trae `defer_loading` (ni siquiera la clave
    /// está presente): `deferred_tools` debe dar `0`, nunca confundirse con
    /// "no sabemos" — es la lectura de "estos bytes son reales y
    /// desconectables" que el defecto original le negaba al consumidor.
    #[test]
    fn deferred_tools_servidor_nada_diferido() {
        let entries_json = serde_json::json!([
            {"name": "mcp__srv__a"},
            {"name": "mcp__srv__b", "defer_loading": false}
        ]);
        let tools = entries_json.as_array().unwrap();
        let entries: Vec<(&str, &Value)> = tools
            .iter()
            .map(|t| (t.get("name").unwrap().as_str().unwrap(), t))
            .collect();

        let rows = group_tools_by_server(entries.into_iter());

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tools, 2);
        assert_eq!(rows[0].deferred_tools, 0, "nada diferido: bytes reales y desconectables");
    }

    /// EL CASO QUE MOTIVA EL CAMPO: un body con DOS servidores MCP donde uno
    /// difiere sus tools y el otro manda su esquema completo sin diferir
    /// nada. El booleano body-wide que este proyecto eliminó
    /// (`client_defer_loading`) daba `true` para el body ENTERO con que UN
    /// servidor calificara — escondiendo que el servidor B no difirió
    /// absolutamente nada. Acá se verifica que `deferred_tools` SÍ distingue
    /// ambos servidores dentro del mismo body, que es la garantía
    /// estructural que reemplaza al booleano conflacionado.
    #[test]
    fn deferred_tools_body_mixto_un_servidor_diferido_y_otro_no() {
        let entries_json = serde_json::json!([
            {"name": "mcp__servidor_a__x", "defer_loading": true},
            {"name": "mcp__servidor_a__y", "defer_loading": true},
            {"name": "mcp__servidor_b__z", "description": "esquema completo, sin diferir"}
        ]);
        let tools = entries_json.as_array().unwrap();
        let entries: Vec<(&str, &Value)> = tools
            .iter()
            .map(|t| (t.get("name").unwrap().as_str().unwrap(), t))
            .collect();

        let rows = group_tools_by_server(entries.into_iter());

        let server_a = rows.iter().find(|r| r.server == "servidor_a").expect("servidor_a presente");
        assert_eq!(server_a.tools, 2);
        assert_eq!(server_a.deferred_tools, 2, "servidor_a: totalmente diferido");

        let server_b = rows.iter().find(|r| r.server == "servidor_b").expect("servidor_b presente");
        assert_eq!(server_b.tools, 1);
        assert_eq!(
            server_b.deferred_tools, 0,
            "servidor_b: NADA diferido — sus bytes son reales y desconectables, aunque servidor_a sí difiera"
        );
    }

    /// PIN DE BYTES (`docs/optimizer-tool-search.md` §2.3): marcar una tool
    /// con `defer_loading: true` CUESTA bytes, nunca los ahorra. Medido en el
    /// cable, byte a byte, tres veces de forma independiente: la resta entre
    /// la misma tool con y sin la marca da exactamente 21 —
    /// `len(",\"defer_loading\":true") == 21`. El servidor primitivo ANEXA la
    /// marca a una definición que sigue viajando completa; no la retiene.
    ///
    /// Este test compara la MISMA tool servida por `group_tools_by_server`
    /// con y sin la clave, sobre el campo `bytes` (no `n_bytes` de telemetría,
    /// pero el mismo camino de medición: `measure_value` vía
    /// `serde_json::to_vec`, dominio bytes-de-cable). Si algún día la
    /// contabilidad empezara a EXCLUIR del conteo las tools diferidas (el
    /// error de categoría que este proyecto ya corrigió una vez: un hecho de
    /// dominio-tokens contaminando un número de dominio-bytes), este test
    /// tiene que fallar.
    #[test]
    fn deferred_tools_marca_defer_loading_suma_21_bytes_y_nunca_los_resta() {
        let sin_marcar = serde_json::json!({"name": "mcp__srv__tool", "description": "algo"});
        let mut con_marca = sin_marcar.clone();
        con_marca["defer_loading"] = serde_json::json!(true);

        let entries_sin: Vec<(&str, &Value)> = vec![("mcp__srv__tool", &sin_marcar)];
        let entries_con: Vec<(&str, &Value)> = vec![("mcp__srv__tool", &con_marca)];

        let rows_sin = group_tools_by_server(entries_sin.into_iter());
        let rows_con = group_tools_by_server(entries_con.into_iter());

        assert_eq!(rows_sin.len(), 1);
        assert_eq!(rows_con.len(), 1);
        assert_eq!(rows_sin[0].deferred_tools, 0, "sin la marca: deferred_tools debe dar 0");
        assert_eq!(rows_con[0].deferred_tools, 1, "con la marca: deferred_tools debe dar 1");

        let bytes_sin = rows_sin[0].bytes;
        let bytes_con = rows_con[0].bytes;

        assert!(
            bytes_con >= bytes_sin,
            "defer_loading NUNCA debe reducir los bytes de un servidor: marca, no retiene \
             (bytes_con={bytes_con}, bytes_sin={bytes_sin})"
        );
        assert_eq!(
            bytes_con - bytes_sin,
            21,
            "marcar defer_loading debe sumar EXACTAMENTE 21 bytes — len(',\"defer_loading\":true')"
        );
    }

    /// `tools` ausente ⇒ `None`; `tools: []` ⇒ `Some(vec![])`. Son casos
    /// DISTINTOS y no deben confundirse: el primero es "el dialecto no dijo
    /// nada de herramientas", el segundo es "sí dijo, y son cero".
    #[test]
    fn tool_entries_ausente_da_none_pero_vacio_da_some_vacio() {
        let sin_tools: Value = serde_json::from_str(r#"{"model": "x"}"#).unwrap();
        assert_eq!(ANTHROPIC.tool_entries(&sin_tools), None);

        let tools_vacio: Value = serde_json::from_str(r#"{"tools": []}"#).unwrap();
        assert_eq!(ANTHROPIC.tool_entries(&tools_vacio), Some(vec![]));

        let tools_no_array: Value = serde_json::from_str(r#"{"tools": "no es un array"}"#).unwrap();
        assert_eq!(ANTHROPIC.tool_entries(&tools_no_array), None);

        let no_objeto: Value = serde_json::from_str("[1,2,3]").unwrap();
        assert_eq!(ANTHROPIC.tool_entries(&no_objeto), None);
    }

    /// `tools_by_server` sobre un body sin herramientas o no-objeto debe
    /// devolver un vector vacío, nunca panic.
    #[test]
    fn tools_by_server_vacio_cuando_no_hay_tools() {
        let sin_tools: Value = serde_json::from_str(r#"{"model": "x"}"#).unwrap();
        assert!(ANTHROPIC.tools_by_server(&sin_tools).is_empty());

        let no_objeto: Value = serde_json::from_str("[1,2,3]").unwrap();
        assert!(ANTHROPIC.tools_by_server(&no_objeto).is_empty());
    }

    /// Una herramienta sin `name` (o con `name` no-string) debe omitirse por
    /// completo: ni cuenta como entrada de `tool_entries`, ni infla el
    /// bucket `NATIVE_TOOLS_LABEL`.
    #[test]
    fn tool_entries_omite_herramienta_sin_name() {
        let body: Value = serde_json::from_str(
            r#"{
                "tools": [
                    {"name": "Read"},
                    {"description": "sin name, debe omitirse"},
                    {"name": 42}
                ]
            }"#,
        )
        .unwrap();

        let entries = ANTHROPIC.tool_entries(&body).expect("tools es un array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "Read");

        let by_server = ANTHROPIC.tools_by_server(&body);
        let native = by_server
            .iter()
            .find(|s| s.server == NATIVE_TOOLS_LABEL)
            .expect("debe existir el bucket nativo");
        assert_eq!(
            native.tools, 1,
            "las herramientas sin name no deben inflar el bucket nativo"
        );
    }

    /// `tools_overhead_bytes` sobre un body Anthropic realista: debe ser
    /// positivo (los corchetes y comas del array SÍ pesan algo) y cerrar
    /// exactamente con `tools_bytes - sum(bytes por servidor)`.
    #[test]
    fn tools_overhead_bytes_positivo_y_exacto_en_body_realista() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "claude-3-5-sonnet",
                "tools": [
                    {"name": "Read", "description": "lee un archivo"},
                    {"name": "Write", "description": "escribe un archivo"},
                    {"name": "mcp__claude_ai_Gmail__search_threads", "description": "busca hilos"},
                    {"name": "mcp__claude_ai_Gmail__get_message", "description": "trae un mensaje"}
                ],
                "messages": [{"role": "user", "content": "hola"}]
            }"#,
        )
        .unwrap();

        let bd = ANTHROPIC.decompose(&body).expect("body es objeto");
        let by_server = ANTHROPIC.tools_by_server(&body);
        let overhead = tools_overhead_bytes(bd.tools_bytes, &by_server);

        let sum: usize = by_server.iter().map(|s| s.bytes).sum();
        assert!(overhead > 0);
        assert_eq!(overhead, bd.tools_bytes - sum);
    }

    // -----------------------------------------------------------------
    // maybe_decompress — descompresión LÓGICA (solo para medición) del
    // body de la Responses API de Codex, que llega con `content-encoding:
    // zstd` (a veces gzip). El body que se REENVÍA (`Outgoing::body`) nunca
    // pasa por acá: sigue siendo `incoming.body` crudo, ver
    // `openai::tests::codex_prepare_mide_sobre_zstd_pero_reenvia_crudo`.
    // -----------------------------------------------------------------

    /// `zstd`: comprime un JSON fixture con `zstd::encode_all` y verifica que
    /// `maybe_decompress` recupera exactamente los bytes originales.
    #[test]
    fn maybe_decompress_zstd_recupera_bytes_originales() {
        let original = br#"{"model":"gpt-5","input":"hola"}"#;
        let comprimido = zstd::encode_all(&original[..], 0).expect("zstd comprime el fixture");

        let recuperado = maybe_decompress(&comprimido, Some("zstd"));

        assert_eq!(recuperado, original);
    }

    /// `gzip`: mismo contrato, con `flate2::write::GzEncoder` como fixture.
    #[test]
    fn maybe_decompress_gzip_recupera_bytes_originales() {
        use std::io::Write;
        let original = br#"{"model":"gpt-5","input":"hola"}"#;
        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(original).expect("gzip comprime el fixture");
        let comprimido = encoder.finish().expect("gzip cierra el stream");

        let recuperado = maybe_decompress(&comprimido, Some("gzip"));

        assert_eq!(recuperado, original);
    }

    /// Sin `content-encoding` (`None`), el body se devuelve tal cual, sin
    /// intentar ninguna descompresión.
    #[test]
    fn maybe_decompress_sin_encoding_devuelve_body_intacto() {
        let body = br#"{"model":"gpt-5"}"#;
        assert_eq!(maybe_decompress(body, None), body.to_vec());
    }

    /// Un `content-encoding` desconocido (ni zstd ni gzip) también devuelve
    /// el body intacto: no es un error, es simplemente una codificación que
    /// este proxy no sabe medir todavía.
    #[test]
    fn maybe_decompress_encoding_desconocido_devuelve_body_intacto() {
        let body = br#"{"model":"gpt-5"}"#;
        assert_eq!(maybe_decompress(body, Some("br")), body.to_vec());
    }

    /// Un body declarado `zstd` pero en realidad corrupto (no es zstd
    /// válido) NUNCA debe hacer panic: `maybe_decompress` cae al fallback
    /// seguro y devuelve el body crudo tal cual llegó.
    #[test]
    fn maybe_decompress_zstd_corrupto_no_panica_y_cae_a_fallback() {
        let corrupto = b"esto no es un frame zstd valido";
        assert_eq!(maybe_decompress(corrupto, Some("zstd")), corrupto.to_vec());
    }

    /// Mismo fallback para `gzip` corrupto.
    #[test]
    fn maybe_decompress_gzip_corrupto_no_panica_y_cae_a_fallback() {
        let corrupto = b"esto tampoco es un stream gzip valido";
        assert_eq!(maybe_decompress(corrupto, Some("gzip")), corrupto.to_vec());
    }

    /// `Content-Encoding` insensible a mayúsculas (`"ZSTD"`, como podría
    /// mandar un cliente): debe reconocerlo igual que en minúsculas.
    #[test]
    fn maybe_decompress_encoding_insensible_a_mayusculas() {
        let original = br#"{"a":1}"#;
        let comprimido = zstd::encode_all(&original[..], 0).expect("zstd comprime el fixture");
        assert_eq!(maybe_decompress(&comprimido, Some("ZSTD")), original);
    }
}
