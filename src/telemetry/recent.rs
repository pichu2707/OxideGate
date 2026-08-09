//! Ring buffer de los últimos N requests atendidos, en detalle individual.
//!
//! `RequestMetric` ya trae el detalle por request, pero hoy ese detalle solo
//! llega a `telemetry.jsonl`: no hay forma de ver en vivo qué pasó en una
//! petición puntual sin leer el archivo. Este módulo guarda una proyección
//! compacta de las últimas [`RECENT_CAPACITY`] métricas en memoria, para que
//! un consumidor (el monitor TUI, hoy; cualquier vista futura) pueda detectar
//! requests ATÍPICOS (outliers de latencia, coste o tokens) sin tocar disco.
//!
//! INVARIANTE CRÍTICA: `prompt_hash` NUNCA se expone acá — y la invariante
//! es sobre la HUELLA, no sobre todo lo que empiece por `prompt_`.
//! `prompt_bytes` SÍ se publica desde este slice: es un entero, no
//! identifica ningún prompt concreto, y era la mitad de subida que le
//! faltaba a `response_bytes` (ver §4.10). Igual que
//! documenta [`middleware::stats`](crate::middleware::stats) para los
//! agregados, esta vista tampoco filtra huellas individuales de prompt: solo
//! expone los campos de coste/latencia/identidad de ruta que ya son
//! públicamente inofensivos.
//!
//! La misma invariante aplica al desglose de herramientas por servidor
//! (`tools_by_server`), pero la línea NO está donde estuvo. Se expone la
//! etiqueta del servidor (`(native)`, `claude_ai_Gmail`, `(others)`…), el
//! conteo de bytes/cantidad, **y los nombres individuales de herramienta**:
//! `tool_names` (declaradas, dentro de cada fila de servidor) y
//! [`RecentRequest::tools_invoked`] / [`RecentRequest::server_tools_invoked`]
//! (invocadas). Lo que NUNCA sale es el CONTENIDO: ni un fragmento del
//! `input_schema`/`description` que compone una herramienta, ni el `input`
//! con el que se la llamó.
//!
//! Hasta que `tool_names` entró en el contrato (`/version` lo declara, y
//! `oxidegate-lens` lo consume), este párrafo prometía que los nombres
//! individuales NUNCA se publicaban. Dejó de ser cierto ahí y la frase se
//! quedó — la clase de doc que no se nota hasta que alguien decide exponer
//! `/requests` fuera de localhost confiando en ella. Un nombre de
//! herramienta sigue sin ser contenido de prompt: lo eligió el cliente y ya
//! se lo declaró al proveedor en texto plano. Pero es un identificador, y
//! quien lea esta vista tiene que saber que viaja.
//!
//! Es PURO: no conoce axum ni ningún framework HTTP, solo `RequestMetric`. El
//! handler que lo expone por HTTP vive en `middleware::requests`.
//!
//! Desde este slice también expone el par PEDIDO/SERVIDO de velocidad
//! (`requested_effort`, `requested_speed`, `served_speed`): son etiquetas
//! cortas de un enum documentado por el proveedor (`"low"`, `"fast"`…),
//! nunca contenido de prompt — no comprometen la invariante de privacidad de
//! arriba.
//!
//! EXCEPCIÓN A LA INVARIANTE: `client`. Es el único campo de esta estructura que
//! NO lo calcula el proxy — es el `User-Agent` crudo del cliente, sin sanear,
//! solo recortado a 200 caracteres. Lo elige quien llama, no nosotros. La capa
//! HTTP acota el daño (`HeaderValue` rechaza bytes de control, y `to_str()`
//! rechaza todo byte ≥ 0x80, así que no hay escapes de terminal ni saltos de
//! línea que rompan el JSONL), pero el contenido en sí es de terceros y viaja
//! tanto por `GET /requests` como al `telemetry.jsonl` en texto plano. Léase
//! `docs/telemetry-per-request.md` §4.3 antes de exponer este endpoint fuera de
//! localhost.
use crate::provider::{
    HooksBlock, InstructionsBlock, SkillsBlock, ToolCalls, ToolSearchSignal, ToolServerBytes,
};
use crate::telemetry::logger::RequestMetric;
use crate::telemetry::{CacheBySection, CodexQuota, SectionShare, SessionAttribution};
use serde::Serialize;
use std::collections::VecDeque;

/// Cantidad máxima de requests individuales que se recuerdan en memoria.
///
/// Una vez alcanzado el tope, cada `ingest` nuevo desaloja el request MÁS
/// VIEJO (FIFO), así el buffer siempre refleja una ventana reciente y acotada
/// sin crecer sin límite en un servidor de larga vida.
pub const RECENT_CAPACITY: usize = 200;

/// Proyección compacta de un [`RequestMetric`] para exposición en vivo.
///
/// Copia fielmente los campos de identidad, coste y latencia de la métrica
/// original, PERO deliberadamente omite `prompt_hash`: ninguna huella
/// individual sale de este módulo.
///
/// `prompt_bytes` estuvo omitido junto a él, con otro motivo —«un detalle de
/// implementación que no aporta a detectar outliers»— escrito cuando esta
/// vista servía solo para cazar filas atípicas. Desde que `/requests` es el
/// contrato público del ecosistema y publica `response_bytes`, esa asimetría
/// dejaba a un consumidor pudiendo responder cuántos bytes BAJARON y no
/// cuántos subieron. Ahora se publica (§4.10).
///
/// No calcula nada derivado (sin `gen_ms`, sin tokens/s, sin lógica de
/// outlier): eso es responsabilidad de la vista que consuma el snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct RecentRequest {
    /// Instante en que se emitió la métrica original (RFC 3339, UTC).
    pub timestamp: String,
    /// Ruta local del proxy que atendió el request (`/v1/messages`, …).
    pub route: String,
    /// Proveedor destino (`anthropic`, `openai`).
    pub upstream: String,
    /// Modelo solicitado. `None` si no venía en el body del request.
    pub model: Option<String>,
    /// `true` si el cliente pidió respuesta en streaming (SSE).
    pub stream: bool,
    /// `User-Agent` del cliente que originó el request. Ver
    /// `telemetry::logger::RequestMetric::client` para el contrato completo
    /// (crudo, topeado en longitud). `None` si el header no vino.
    pub client: Option<String>,
    /// Código de estado HTTP devuelto al cliente.
    pub status: u16,
    /// Tokens de entrada exactos, tal como los reporta el proveedor.
    pub input_tokens: Option<u64>,
    /// Tokens de salida exactos, tal como los reporta el proveedor.
    pub output_tokens: Option<u64>,
    /// Tokens servidos desde caché (lectura). `None` si no se midió.
    pub cache_read_tokens: Option<u64>,
    /// Tokens escritos a caché (creación). `None` si el proveedor no lo reporta.
    pub cache_write_tokens: Option<u64>,
    /// Coste estimado en USD según la tabla de precios. `None` si no calculable.
    pub cost_estimate_usd: Option<f64>,
    /// `true` si OxideGate inyectó el breakpoint de `cache_control` en este request.
    pub cache_control_forced: bool,
    /// Nivel de esfuerzo de razonamiento PEDIDO por el cliente
    /// (`output_config.effort`). Dialecto exclusivo de Anthropic: `None` en
    /// OpenAI/Gemini o si el campo estaba ausente/no era un string. Ver
    /// `telemetry::logger::RequestMetric::requested_effort`.
    pub requested_effort: Option<String>,
    /// Modo de velocidad PEDIDO por el cliente (`speed` a nivel raíz).
    /// SEPARADO a propósito de `served_speed`: el modo `fast` de Anthropic
    /// tiene su propio rate limit, así que puede pedirse `"fast"` y servirse
    /// `"standard"`. Ver `telemetry::logger::RequestMetric::requested_speed`.
    pub requested_speed: Option<String>,
    /// Velocidad con la que el proveedor SIRVIÓ REALMENTE la respuesta
    /// (`usage.speed`). DOCUMENTADA por Anthropic, NO OBSERVADA todavía en
    /// tráfico real: `None` significa "no reportada", nunca "estándar". Ver
    /// `telemetry::logger::RequestMetric::served_speed`.
    pub served_speed: Option<String>,
    /// Time To First Token en ms. `None` si no aplica (p. ej. sin streaming).
    pub ttft_ms: Option<f64>,
    /// Latencia total en ms, desde el request hasta el cierre de la respuesta.
    pub total_ms: f64,

    // --- Desglose de contexto (ver `provider::ContextBreakdown` y
    //     `telemetry::logger::RequestMetric`) ---
    /// Bytes del prompt de sistema. BYTES, nunca tokens (re-serialización
    /// canónica JSON). `None` si no se pudo calcular el desglose.
    pub context_system_bytes: Option<usize>,
    /// Bytes de los esquemas de herramientas.
    pub context_tools_bytes: Option<usize>,
    /// Bytes del historial (todos los mensajes menos el último).
    pub context_history_bytes: Option<usize>,
    /// Bytes del último mensaje (el turno nuevo).
    pub context_last_turn_bytes: Option<usize>,
    /// Bytes del resto de campos de control a nivel raíz.
    pub context_other_bytes: Option<usize>,
    /// Suma de los cinco campos de contexto anteriores. Mismo contrato de
    /// medición que en `RequestMetric::context_measured_bytes`: es tamaño de
    /// JSON canónico re-serializado, no tamaño de wire. Este tipo ni siquiera
    /// expone `prompt_bytes` (ver invariante de privacidad del módulo), así
    /// que no hay ningún otro campo con el que pueda confundirse o mezclarse.
    pub context_measured_bytes: Option<usize>,
    /// Número de mensajes del historial completo (incluyendo el último).
    pub context_messages_count: Option<usize>,
    /// `(system + tools + history) / measured`. `None` si `measured` es cero
    /// o si no se pudo calcular el desglose (asimetría documentada en
    /// `ContextBreakdown::context_tax_ratio`).
    pub context_tax_ratio: Option<f64>,
    /// Qué cubo del contexto cayó dentro del prefijo cacheado, ESTIMADO.
    ///
    /// Objeto ANIDADO a propósito: los `context_*_bytes` de arriba son medición
    /// directa, esto es una estimación derivada de convertir tokens a bytes.
    /// La estructura mantiene visible esa frontera para que una lente no pueda
    /// pintarlas en la misma columna sin darse cuenta. Lleva dentro su propio
    /// `method` versionado. Ver `telemetry::cache_attribution`.
    pub cache_by_section: Option<CacheBySection>,
    /// Fracción del input PAGADO por sección, ESTIMADA sobre `cache_by_section`.
    /// Fracciones de 0 a 1, nunca dinero. Ver §4.12.
    pub input_share_by_section: Option<SectionShare>,
    /// Desglose de `tools` por servidor MCP (ver
    /// `telemetry::logger::RequestMetric::tools_by_server` para el contrato
    /// completo `None`/`Some(vec![])`). Expone SOLO etiqueta de servidor +
    /// conteos (ver invariante de privacidad en el header del módulo): jamás
    /// nombres de herramienta individuales ni fragmentos de su esquema.
    ///
    /// IMPLICACIÓN DE MEMORIA: este ring buffer guarda hasta
    /// [`RECENT_CAPACITY`] filas; cada una carga ahora un `Vec` de hasta
    /// `provider::MAX_TOOL_SERVERS + 1` entradas (el cupo de servidores
    /// trackeados individualmente más el bucket de desborde), en vez de un
    /// campo de tamaño fijo. El buffer sigue acotado en cantidad de FILAS,
    /// pero el tamaño de CADA fila ya no es constante.
    pub tools_by_server: Option<Vec<ToolServerBytes>>,
    /// Bytes de `tools` no atribuidos a ningún servidor. Mismo contrato
    /// `None`/`Some` que `tools_by_server`.
    pub tools_overhead_bytes: Option<usize>,
    /// Señal de carga diferida de herramientas (`tool_search`) del dialecto
    /// OpenAI/Codex Responses (ver
    /// `telemetry::logger::RequestMetric::tool_search` para el contrato
    /// completo). El diferenciador eager-vs-lazy por cliente. No compromete la
    /// invariante de privacidad del módulo: solo lleva un booleano y un conteo,
    /// jamás nombres de herramienta ni fragmentos de su esquema.
    pub tool_search: Option<ToolSearchSignal>,
    /// Señal de honestidad sobre la atribución de `tools_by_server` (ver
    /// `telemetry::logger::RequestMetric::tools_flattened`). `Some(true)` avisa
    /// de que el cubo `(native)` de esta fila puede ocultar MCP aplanado
    /// (`pi`/`opencode`). No compromete la invariante de privacidad: es solo un
    /// booleano estructural, jamás nombres de herramienta ni de servidor.
    pub tools_flattened: Option<bool>,
    /// Listado de skills declarado en el body: `{declared, listing_bytes,
    /// format}`, o `null`. Se paga en CADA petición, se invoque una skill o
    /// no. `null` = no se reconoció ningún listado, NUNCA "cero skills".
    pub skills: Option<SkillsBlock>,
    /// Bloque de instrucciones del usuario declarado en el body:
    /// `{bytes, format}`, o `null`. Se paga en CADA petición. `null` = no se
    /// reconoció ningún bloque, NUNCA "el usuario no tiene instrucciones".
    ///
    /// No compromete la invariante de privacidad del módulo: lleva un entero y
    /// una etiqueta de dialecto, **jamás una línea del contenido** del fichero
    /// —que es texto privado del usuario— ni una huella de él.
    pub instructions: Option<InstructionsBlock>,
    /// Salida de los hooks de `SessionStart`: el 29% del peaje fijo. `None`
    /// significa que no se reconocio el bloque, NUNCA que no haya hooks.
    pub hooks: Option<HooksBlock>,
    /// Nivel de esfuerzo que IMPUSO la palanca B, o `null` si el proxy no
    /// intervino. Se lee junto a `requested_effort` (lo que pidió el cliente),
    /// nunca en su lugar: es lo que impide confundir un ahorro del cliente con
    /// una intervención del medidor. Ver §4.14.
    ///
    /// No compromete la invariante de privacidad del módulo: es una etiqueta
    /// de un enum documentado por el proveedor, no contenido de prompt.
    pub effort_forced: Option<String>,
    /// Invocaciones de herramienta observadas en la respuesta. `None` = este
    /// proveedor no tiene extractor (hoy solo lo tiene Anthropic); `Some` con
    /// listas vacías = se escaneó y no hubo ninguna. Son afirmaciones
    /// distintas y no deben fundirse.
    ///
    /// No compromete la invariante de privacidad del módulo: un nombre de
    /// herramienta es un identificador declarado por el propio cliente —el
    /// mismo string que ya se publica en `tool_names`—, no contenido de
    /// prompt. El `input` de la llamada, que SÍ lo sería, no se mide.
    ///
    /// Lleva dentro `complete` y los `*_total` para que el lector sepa si la
    /// lista es un prefijo (turno abortado) o está recortada por el cupo.
    /// Ver `telemetry::logger::RequestMetric::tool_calls`.
    #[serde(default)]
    pub tool_calls: Option<ToolCalls>,
    /// Bytes del body que MANDÓ EL CLIENTE, en su forma lógica. La mitad de
    /// subida que le faltaba a [`Self::response_bytes`].
    ///
    /// **Tres cosas que NO es**, y las tres importan (contrato completo en
    /// `docs/telemetry-per-request.md` §4.10):
    ///
    /// 1. **No es el tamaño de wire.** En `/v1/codex/responses` y `/v1beta/*`
    ///    se mide sobre el body ya DESCOMPRIMIDO (ver
    ///    `provider::maybe_decompress`): si el cliente comprimió —`pi` manda
    ///    zstd— por el cable subieron menos bytes que los que dice este campo.
    /// 2. **No es lo que subió al proveedor.** Se calcula sobre el body
    ///    ORIGINAL, antes de cualquier mutación. Con
    ///    [`Self::cache_control_forced`] en `true` el body reenviado es MAYOR
    ///    que este número — ese booleano es lo que delata la intervención.
    /// 3. **No es la suma del desglose.** `context_measured_bytes` es JSON
    ///    canónico re-serializado, medido en otro punto del pipeline. Los dos
    ///    NUNCA deben combinarse en un mismo cociente (§4.1).
    ///
    /// No compromete la invariante de privacidad del módulo: es un entero, no
    /// una huella. `prompt_hash` sigue sin salir de acá.
    pub prompt_bytes: usize,
    /// Bytes del cuerpo de la respuesta, SIN COMPRIMIR (el proxy descarta
    /// `Accept-Encoding`). `None` si no hubo respuesta. Ver §4.9.
    pub response_bytes: Option<usize>,
    /// Microsegundos que el proxy pasó dentro de `Provider::prepare`
    /// (parseo, `decompose` y mutación opcional del body). No incluye la
    /// lectura del body del socket ni el round-trip upstream.
    pub prepare_us: u64,
    /// Estado de la cuota de suscripción de Codex (ver
    /// `telemetry::logger::RequestMetric::codex_quota` para el contrato
    /// completo). `None` para tráfico sin cabeceras `x-codex-*` (Anthropic,
    /// Gemini, OpenAI vía API key) o cuando el upstream falló antes de
    /// responder. No compromete la invariante de privacidad del módulo: no
    /// hay contenido de prompt en ningún campo de `CodexQuota`, solo estado
    /// de cuota (porcentajes, ventanas, timestamps de reseteo).
    pub codex_quota: Option<CodexQuota>,
    /// Sesión resuelta por precedencia de cabeceras del request (ver
    /// `telemetry::logger::RequestMetric::session` para el contrato completo
    /// de honestidad `source`+`key`). Nunca `Option`: la peor rama es el
    /// bucket `Unattributed`, un fallback honesto, no una ausencia.
    ///
    /// No compromete la invariante de privacidad del módulo: `key` es
    /// siempre una etiqueta opaca (el header de atribución crudo o el
    /// `User-Agent` de fallback), jamás contenido de prompt ni una
    /// credencial — el resolver (`middleware::proxy::session_of`) lee
    /// exclusivamente `X-OxideGate-Session`, `x-claude-code-session-id` y
    /// `User-Agent`.
    pub session: SessionAttribution,
}

impl From<&RequestMetric> for RecentRequest {
    /// Copia campo a campo desde `RequestMetric`, excluyendo `prompt_hash` a
    /// propósito (ver invariante de privacidad en el header del módulo).
    ///
    /// `prompt_bytes` SÍ se copia, desde #51. La invariante es sobre la
    /// HUELLA, no sobre todo lo que empiece por `prompt_`: un contador de
    /// bytes no identifica ningún prompt ni correlaciona dos peticiones con el
    /// mismo contenido, que es justo lo que `prompt_hash` permite.
    fn from(m: &RequestMetric) -> Self {
        Self {
            timestamp: m.timestamp.clone(),
            route: m.route.clone(),
            upstream: m.upstream.clone(),
            model: m.model.clone(),
            stream: m.stream,
            client: m.client.clone(),
            status: m.status,
            input_tokens: m.input_tokens,
            output_tokens: m.output_tokens,
            cache_read_tokens: m.cache_read_tokens,
            cache_write_tokens: m.cache_write_tokens,
            cost_estimate_usd: m.cost_estimate_usd,
            cache_control_forced: m.cache_control_forced,
            requested_effort: m.requested_effort.clone(),
            requested_speed: m.requested_speed.clone(),
            served_speed: m.served_speed.clone(),
            ttft_ms: m.ttft_ms,
            total_ms: m.total_ms,
            context_system_bytes: m.context_system_bytes,
            context_tools_bytes: m.context_tools_bytes,
            context_history_bytes: m.context_history_bytes,
            context_last_turn_bytes: m.context_last_turn_bytes,
            context_other_bytes: m.context_other_bytes,
            context_measured_bytes: m.context_measured_bytes,
            context_messages_count: m.context_messages_count,
            context_tax_ratio: m.context_tax_ratio,
            cache_by_section: m.cache_by_section,
            input_share_by_section: m.input_share_by_section,
            tools_by_server: m.tools_by_server.clone(),
            tools_overhead_bytes: m.tools_overhead_bytes,
            tool_search: m.tool_search.clone(),
            tools_flattened: m.tools_flattened,
            skills: m.skills,
            instructions: m.instructions,
            hooks: m.hooks,
            effort_forced: m.effort_forced.clone(),
            tool_calls: m.tool_calls.clone(),
            prompt_bytes: m.prompt_bytes,
            response_bytes: m.response_bytes,
            prepare_us: m.prepare_us,
            codex_quota: m.codex_quota.clone(),
            session: m.session.clone(),
        }
    }
}

/// Buffer en memoria de los últimos [`RECENT_CAPACITY`] requests.
///
/// Vive detrás de un `Arc<RwLock<_>>` compartido entre la task de drenaje
/// (que llama `ingest`) y el handler de `/requests` (que llama `snapshot`),
/// exactamente igual que [`StatsRegistry`](crate::telemetry::stats::StatsRegistry).
/// Este tipo en sí mismo no sabe nada de locks ni de axum.
#[derive(Debug, Default)]
pub struct RecentRequests {
    buffer: VecDeque<RecentRequest>,
}

impl RecentRequests {
    /// Incorpora una métrica al buffer, proyectándola a [`RecentRequest`].
    ///
    /// El request nuevo se agrega al final (orden cronológico: más viejo
    /// primero, más nuevo al final). Si al agregar se supera
    /// [`RECENT_CAPACITY`], se desaloja el request MÁS VIEJO (`pop_front`)
    /// para mantener el tope de memoria constante.
    pub fn ingest(&mut self, m: &RequestMetric) {
        self.buffer.push_back(RecentRequest::from(m));
        if self.buffer.len() > RECENT_CAPACITY {
            self.buffer.pop_front();
        }
    }

    /// Construye una copia del estado actual del buffer, en orden
    /// cronológico (más viejo primero, más nuevo al final). El consumidor
    /// decide si quiere invertir el orden para mostrar "más reciente arriba":
    /// esta función no toma esa decisión de presentación.
    pub fn snapshot(&self) -> Vec<RecentRequest> {
        self.buffer.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `session` es un campo REQUERIDO de `RequestMetric` (nunca `Option`, ver
    // `telemetry::session`) y ahora también se proyecta a `RecentRequest`
    // (ver tests de proyección/round-trip más abajo).
    use crate::telemetry::SessionSource;

    /// Construye una métrica mínima, variando el `timestamp` para poder
    /// distinguir requests entre sí en los asserts de orden.
    fn base_metric(timestamp: &str) -> RequestMetric {
        RequestMetric {
            timestamp: timestamp.to_string(),
            route: "/v1/messages".to_string(),
            upstream: "anthropic".to_string(),
            model: Some("claude-opus-4".to_string()),
            prompt_hash: "0000000000000001".to_string(),
            stream: false,
            client: Some("claude-cli/1.2.3 (external, cli)".to_string()),
            prompt_bytes: 100,
            input_tokens: Some(10),
            output_tokens: Some(5),
            cache_read_tokens: None,
            cache_write_tokens: None,
            cost_estimate_usd: Some(0.01),
            cache_control_forced: false,
            requested_effort: Some("high".to_string()),
            requested_speed: None,
            served_speed: None,
            status: 200,
            ttft_ms: Some(50.0),
            total_ms: 100.0,
            tokens_per_sec: Some(20.0),
            context_system_bytes: Some(10),
            context_tools_bytes: Some(5),
            context_history_bytes: Some(15),
            context_last_turn_bytes: Some(20),
            context_other_bytes: Some(2),
            context_measured_bytes: Some(52),
            context_messages_count: Some(3),
            context_tax_ratio: Some(30.0 / 52.0),
            cache_by_section: None,
            input_share_by_section: None,
            tools_by_server: Some(vec![ToolServerBytes {
                server: "claude_ai_Gmail".to_string(),
                kind: crate::provider::ToolServerKind::Mcp,
                tools: 2,
                bytes: 30,
                tool_names: Vec::new(),
                deferred_tools: 0,
            }]),
            tools_overhead_bytes: Some(4),
            tool_search: None,
            tools_flattened: None,
            skills: None,
            instructions: None,
            hooks: None,
            effort_forced: None,
            response_bytes: None,
            prepare_us: 42,
            codex_quota: None,
            session: SessionAttribution {
                source: SessionSource::Unattributed,
                key: "unattributed".to_string(),
            },
            tool_calls: None,
        }
    }

    /// `CodexQuota` de prueba con los doce campos en `Some`, usada por los
    /// tests de proyección y round-trip serde de `codex_quota`.
    fn fixture_codex_quota() -> CodexQuota {
        CodexQuota {
            plan_type: Some("pro".to_string()),
            active_limit: Some("primary".to_string()),
            credits_balance: Some("12.50".to_string()),
            primary_used_percent: Some(4),
            secondary_used_percent: Some(12),
            primary_window_minutes: Some(300),
            secondary_window_minutes: Some(10080),
            primary_reset_after_seconds: Some(1800),
            primary_reset_at: Some(1_732_000_000),
            secondary_reset_at: Some(1_732_600_000),
            credits_has_credits: Some(false),
            credits_unlimited: Some(false),
        }
    }

    #[test]
    fn ingest_preserva_orden_cronologico() {
        let mut recent = RecentRequests::default();
        recent.ingest(&base_metric("t1"));
        recent.ingest(&base_metric("t2"));
        recent.ingest(&base_metric("t3"));

        let snapshot = recent.snapshot();
        let timestamps: Vec<&str> = snapshot.iter().map(|r| r.timestamp.as_str()).collect();
        assert_eq!(timestamps, vec!["t1", "t2", "t3"]);
    }

    #[test]
    fn buffer_topea_en_capacidad_y_desaloja_el_mas_viejo() {
        let mut recent = RecentRequests::default();
        for i in 0..(RECENT_CAPACITY + 10) {
            recent.ingest(&base_metric(&format!("t{i}")));
        }

        let snapshot = recent.snapshot();
        assert_eq!(snapshot.len(), RECENT_CAPACITY);
        // El más viejo que sobrevive es "t10" (se desalojaron t0..t9).
        assert_eq!(snapshot.first().unwrap().timestamp, "t10");
        // El más nuevo es el último ingestado.
        assert_eq!(
            snapshot.last().unwrap().timestamp,
            format!("t{}", RECENT_CAPACITY + 9)
        );
    }

    #[test]
    fn snapshot_devuelve_una_copia_independiente() {
        let mut recent = RecentRequests::default();
        recent.ingest(&base_metric("t1"));

        let snapshot = recent.snapshot();
        recent.ingest(&base_metric("t2"));

        // El snapshot tomado antes del segundo ingest no debe verse afectado.
        assert_eq!(snapshot.len(), 1);
        assert_eq!(recent.snapshot().len(), 2);
    }

    #[test]
    fn proyeccion_copia_campos_fielmente_incluyendo_none() {
        let mut m = base_metric("t1");
        m.model = None;
        m.client = None;
        m.cache_read_tokens = None;
        m.cache_write_tokens = None;
        m.cost_estimate_usd = None;
        m.ttft_ms = None;
        m.cache_control_forced = true;
        m.status = 500;
        m.context_system_bytes = None;
        m.context_tools_bytes = None;
        m.context_history_bytes = None;
        m.context_last_turn_bytes = None;
        m.context_other_bytes = None;
        m.context_measured_bytes = None;
        m.context_messages_count = None;
        m.context_tax_ratio = None;
        m.tools_by_server = None;
        m.tools_overhead_bytes = None;

        let mut recent = RecentRequests::default();
        recent.ingest(&m);

        let snapshot = recent.snapshot();
        let row = &snapshot[0];
        assert_eq!(row.timestamp, "t1");
        assert_eq!(row.route, "/v1/messages");
        assert_eq!(row.upstream, "anthropic");
        assert_eq!(row.model, None);
        assert!(!row.stream);
        assert_eq!(row.client, None);
        assert_eq!(row.status, 500);
        assert_eq!(row.input_tokens, Some(10));
        assert_eq!(row.output_tokens, Some(5));
        assert_eq!(row.cache_read_tokens, None);
        assert_eq!(row.cache_write_tokens, None);
        assert_eq!(row.cost_estimate_usd, None);
        assert!(row.cache_control_forced);
        assert_eq!(row.ttft_ms, None);
        assert_eq!(row.total_ms, 100.0);
        assert_eq!(row.context_system_bytes, None);
        assert_eq!(row.context_tools_bytes, None);
        assert_eq!(row.context_history_bytes, None);
        assert_eq!(row.context_last_turn_bytes, None);
        assert_eq!(row.context_other_bytes, None);
        assert_eq!(row.context_measured_bytes, None);
        assert_eq!(row.context_messages_count, None);
        assert_eq!(row.context_tax_ratio, None);
        assert_eq!(row.tools_by_server, None);
        assert_eq!(row.tools_overhead_bytes, None);
        assert_eq!(row.prepare_us, 42);
        assert_eq!(row.codex_quota, None);
        assert_eq!(row.session, base_metric("t1").session);
    }

    /// La proyección copia `codex_quota` fielmente cuando SÍ hay cuota
    /// (`Some`), campo a campo, igual que ya se verifica para
    /// `tools_by_server` en `proyeccion_copia_campos_de_contexto_cuando_hay_desglose`.
    #[test]
    fn proyeccion_copia_codex_quota_fielmente_cuando_esta_presente() {
        let mut m = base_metric("t1");
        m.codex_quota = Some(fixture_codex_quota());

        let mut recent = RecentRequests::default();
        recent.ingest(&m);

        let row = &recent.snapshot()[0];
        assert_eq!(row.codex_quota, Some(fixture_codex_quota()));
    }

    /// La proyección copia `session` fielmente cuando `source =
    /// SessionSource::Explicit` (clave asignada explícitamente por quien
    /// invoca vía `X-OxideGate-Session`).
    #[test]
    fn proyeccion_copia_session_fielmente_cuando_es_explicit() {
        let mut m = base_metric("t1");
        m.session = SessionAttribution {
            source: SessionSource::Explicit,
            key: "claude-1".to_string(),
        };

        let mut recent = RecentRequests::default();
        recent.ingest(&m);

        let row = &recent.snapshot()[0];
        assert_eq!(row.session.source, SessionSource::Explicit);
        assert_eq!(row.session.key, "claude-1");
    }

    /// La proyección copia `session` fielmente cuando `source =
    /// SessionSource::Native` (id de sesión nativo de Claude Code, sin
    /// header explícito de OxideGate).
    #[test]
    fn proyeccion_copia_session_fielmente_cuando_es_native() {
        let mut m = base_metric("t1");
        m.session = SessionAttribution {
            source: SessionSource::Native,
            key: "native-session-9".to_string(),
        };

        let mut recent = RecentRequests::default();
        recent.ingest(&m);

        let row = &recent.snapshot()[0];
        assert_eq!(row.session.source, SessionSource::Native);
        assert_eq!(row.session.key, "native-session-9");
    }

    /// La proyección copia `session` fielmente cuando `source =
    /// SessionSource::Unattributed` (ningún header de atribución presente,
    /// fallback al `User-Agent` crudo).
    #[test]
    fn proyeccion_copia_session_fielmente_cuando_es_unattributed() {
        let mut m = base_metric("t1");
        m.session = SessionAttribution {
            source: SessionSource::Unattributed,
            key: "claude-cli/1.2.3 (external, cli)".to_string(),
        };

        let mut recent = RecentRequests::default();
        recent.ingest(&m);

        let row = &recent.snapshot()[0];
        assert_eq!(row.session.source, SessionSource::Unattributed);
        assert_eq!(row.session.key, "claude-cli/1.2.3 (external, cli)");
    }

    /// Round-trip serde de `RecentRequest` con `session.source = Explicit`
    /// preserva `source` y `key` en el JSON, mismo patrón que
    /// `round_trip_serde_con_codex_quota_presente`.
    #[test]
    fn round_trip_serde_con_session_presente() {
        let mut m = base_metric("t1");
        m.session = SessionAttribution {
            source: SessionSource::Explicit,
            key: "claude-1".to_string(),
        };

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let row = &recent.snapshot()[0];
        let json = serde_json::to_string(row).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["session"]["source"], "explicit");
        assert_eq!(parsed["session"]["key"], "claude-1");
    }

    /// Round-trip serde de `RecentRequest` con `codex_quota: Some(..)`
    /// preserva todos los campos anidados, mismo patrón que
    /// `round_trip_serde_con_tools_by_server_presente`.
    #[test]
    fn round_trip_serde_con_codex_quota_presente() {
        let mut m = base_metric("t1");
        m.codex_quota = Some(fixture_codex_quota());

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let row = &recent.snapshot()[0];
        let json = serde_json::to_string(row).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["codex_quota"]["plan_type"], "pro");
        assert_eq!(parsed["codex_quota"]["active_limit"], "primary");
        assert_eq!(parsed["codex_quota"]["credits_balance"], "12.50");
        assert_eq!(parsed["codex_quota"]["primary_used_percent"], 4);
        assert_eq!(parsed["codex_quota"]["secondary_used_percent"], 12);
        assert_eq!(parsed["codex_quota"]["primary_window_minutes"], 300);
        assert_eq!(parsed["codex_quota"]["secondary_window_minutes"], 10080);
        assert_eq!(parsed["codex_quota"]["primary_reset_after_seconds"], 1800);
        assert_eq!(parsed["codex_quota"]["primary_reset_at"], 1_732_000_000i64);
        assert_eq!(
            parsed["codex_quota"]["secondary_reset_at"],
            1_732_600_000i64
        );
        assert_eq!(parsed["codex_quota"]["credits_has_credits"], false);
        assert_eq!(parsed["codex_quota"]["credits_unlimited"], false);
    }

    /// Round-trip serde con `codex_quota: None` serializa a `null`, mismo
    /// patrón que `round_trip_serde_con_client_none`.
    #[test]
    fn round_trip_serde_con_codex_quota_none() {
        let m = base_metric("t1"); // codex_quota ya es None por defecto

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let row = &recent.snapshot()[0];
        let json = serde_json::to_string(row).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert!(parsed["codex_quota"].is_null());
    }

    /// Cuando SÍ hay desglose calculado, la proyección debe copiarlo fiel
    /// campo a campo (no solo el caso `None`).
    #[test]
    fn proyeccion_copia_campos_de_contexto_cuando_hay_desglose() {
        let m = base_metric("t1");
        let mut recent = RecentRequests::default();
        recent.ingest(&m);

        let row = &recent.snapshot()[0];
        assert_eq!(row.context_system_bytes, Some(10));
        assert_eq!(row.context_tools_bytes, Some(5));
        assert_eq!(row.context_history_bytes, Some(15));
        assert_eq!(row.context_last_turn_bytes, Some(20));
        assert_eq!(row.context_other_bytes, Some(2));
        assert_eq!(row.context_measured_bytes, Some(52));
        assert_eq!(row.context_messages_count, Some(3));
        assert_eq!(row.context_tax_ratio, Some(30.0 / 52.0));
        assert_eq!(
            row.tools_by_server,
            Some(vec![ToolServerBytes {
                server: "claude_ai_Gmail".to_string(),
                kind: crate::provider::ToolServerKind::Mcp,
                tools: 2,
                bytes: 30,
                tool_names: Vec::new(),
                deferred_tools: 0,
            }])
        );
        assert_eq!(row.tools_overhead_bytes, Some(4));
        assert_eq!(row.prepare_us, 42);
    }

    /// `RecentRequest` NUNCA debe exponer `prompt_hash` (invariante de
    /// privacidad documentada en el header del módulo): lo verificamos a
    /// nivel de JSON serializado, no solo por inspección del tipo, para que
    /// un futuro `#[serde(flatten)]` accidental no cuele esa clave sin que
    /// ningún test lo note.
    ///
    /// La invariante es sobre la HUELLA, no sobre todo prefijo `prompt_`:
    /// `prompt_bytes` es un entero que no identifica ningún prompt concreto
    /// y desde este slice se publica a propósito (§4.10). Este test lo
    /// afirma en positivo para que quitarlo vuelva a fallar acá.
    #[test]
    fn recent_request_expone_los_bytes_pero_nunca_la_huella() {
        let mut recent = RecentRequests::default();
        recent.ingest(&base_metric("t1"));

        let row = &recent.snapshot()[0];
        let json = serde_json::to_string(row).unwrap();

        assert!(!json.contains("prompt_hash"), "no debe exponer prompt_hash");
        assert!(json.contains("prompt_bytes"), "debe exponer prompt_bytes");
    }

    /// `RequestMetric` (con el desglose de herramientas presente) y
    /// `RecentRequest` (su proyección) deben sobrevivir un round-trip por
    /// `serde_json` sin perder el campo anidado `tools_by_server`.
    #[test]
    fn round_trip_serde_con_tools_by_server_presente() {
        let m = base_metric("t1");

        let metric_json = serde_json::to_string(&m).unwrap();
        assert!(metric_json.contains("\"tools_by_server\""));
        assert!(metric_json.contains("\"claude_ai_Gmail\""));
        assert!(metric_json.contains("\"mcp\""));

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let row = &recent.snapshot()[0];
        let recent_json = serde_json::to_string(row).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&recent_json).unwrap();
        assert_eq!(parsed["tools_by_server"][0]["server"], "claude_ai_Gmail");
        assert_eq!(parsed["tools_by_server"][0]["kind"], "mcp");
        assert_eq!(parsed["tools_overhead_bytes"], 4);
    }

    /// Con los tres campos de esfuerzo/velocidad presentes (`Some`), tanto
    /// `RequestMetric` como su proyección `RecentRequest` deben serializarlos
    /// con sus valores exactos — round-trip vía `serde_json::to_string` +
    /// reparseo a `Value`, mismo patrón que
    /// `round_trip_serde_con_tools_by_server_presente`.
    #[test]
    fn round_trip_serde_con_effort_y_speed_presentes() {
        let mut m = base_metric("t1");
        m.requested_effort = Some("xhigh".to_string());
        m.requested_speed = Some("fast".to_string());
        m.served_speed = Some("fast".to_string());

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let row = &recent.snapshot()[0];
        let json = serde_json::to_string(row).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["requested_effort"], "xhigh");
        assert_eq!(parsed["requested_speed"], "fast");
        assert_eq!(parsed["served_speed"], "fast");
    }

    /// Con los tres campos ausentes (`None`, el caso hoy más común: todavía
    /// no se observó tráfico con `fast` ni con `effort` explícito), deben
    /// serializar a `null`, nunca desaparecer del JSON ni fallar.
    #[test]
    fn round_trip_serde_con_effort_y_speed_none() {
        let mut m = base_metric("t1");
        m.requested_effort = None;
        m.requested_speed = None;
        m.served_speed = None;

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let row = &recent.snapshot()[0];
        let json = serde_json::to_string(row).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert!(parsed["requested_effort"].is_null());
        assert!(parsed["requested_speed"].is_null());
        assert!(parsed["served_speed"].is_null());
    }

    /// La señal `tool_search` LAZY (`Some { used: true, deferred_loaded }`)
    /// debe proyectarse desde `RequestMetric` a `RecentRequest` y sobrevivir
    /// el round-trip serde con `used` y `deferred_loaded` exactos — el dato
    /// que `oxidegate-lens` lee para decir "este cliente es lazy". Mismo
    /// patrón que `round_trip_serde_con_effort_y_speed_presentes`.
    #[test]
    fn round_trip_serde_con_tool_search_lazy() {
        let mut m = base_metric("t1");
        m.tool_search = Some(ToolSearchSignal {
            used: true,
            deferred_loaded: 3,
        });

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let row = &recent.snapshot()[0];
        assert_eq!(
            row.tool_search, m.tool_search,
            "la proyección copia el campo"
        );

        let json = serde_json::to_string(row).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["tool_search"]["used"], true);
        assert_eq!(parsed["tool_search"]["deferred_loaded"], 3);
    }

    /// La señal `tools_flattened` (`Some(true)` = `(native)` no verificable en
    /// pi/opencode) debe proyectarse a `RecentRequest` y sobrevivir el
    /// round-trip serde — el dato que `oxidegate-lens` lee para no confundir el
    /// cubo `(native)` aplanado con tools genuinamente nativas.
    #[test]
    fn round_trip_serde_con_tools_flattened_true() {
        let mut m = base_metric("t1");
        m.tools_flattened = Some(true);

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let row = &recent.snapshot()[0];
        assert_eq!(
            row.tools_flattened,
            Some(true),
            "la proyección copia el campo"
        );

        let json = serde_json::to_string(row).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["tools_flattened"], true);
    }

    /// Con `tools_flattened` ausente (`None`) debe serializar a `null`, nunca
    /// desaparecer del JSON ni fallar.
    #[test]
    fn round_trip_serde_con_tools_flattened_none() {
        let mut m = base_metric("t1");
        m.tools_flattened = None;

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let row = &recent.snapshot()[0];
        let json = serde_json::to_string(row).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["tools_flattened"].is_null());
    }

    /// Con `tool_search` ausente (`None`: el caso de Anthropic/Gemini/Chat, o
    /// un body que no parseó) debe serializar a `null`, nunca desaparecer del
    /// JSON ni fallar.
    #[test]
    fn round_trip_serde_con_tool_search_none() {
        let mut m = base_metric("t1");
        m.tool_search = None;

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let row = &recent.snapshot()[0];
        let json = serde_json::to_string(row).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["tool_search"].is_null());
    }

    /// Mismo round-trip, con el campo en `None`: debe serializar a `null`,
    /// nunca desaparecer ni fallar.
    #[test]
    fn round_trip_serde_con_tools_by_server_none() {
        let mut m = base_metric("t1");
        m.tools_by_server = None;
        m.tools_overhead_bytes = None;

        let metric_json = serde_json::to_string(&m).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&metric_json).unwrap();
        assert!(parsed["tools_by_server"].is_null());
        assert!(parsed["tools_overhead_bytes"].is_null());

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let row = &recent.snapshot()[0];
        let recent_json = serde_json::to_string(row).unwrap();
        let parsed_recent: serde_json::Value = serde_json::from_str(&recent_json).unwrap();
        assert!(parsed_recent["tools_by_server"].is_null());
        assert!(parsed_recent["tools_overhead_bytes"].is_null());
    }

    /// Con `client` presente, tanto `RequestMetric` como su proyección
    /// `RecentRequest` deben serializarlo con el valor exacto — mismo patrón
    /// que `round_trip_serde_con_effort_y_speed_presentes`.
    #[test]
    fn round_trip_serde_con_client_presente() {
        let m = base_metric("t1");

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let row = &recent.snapshot()[0];
        let json = serde_json::to_string(row).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["client"], "claude-cli/1.2.3 (external, cli)");
    }

    /// Con `client` ausente (`None`, el caso de un cliente que no manda
    /// `User-Agent` o cuyo header no era UTF-8 válido), debe serializar a
    /// `null`, nunca desaparecer del JSON ni fallar.
    #[test]
    fn round_trip_serde_con_client_none() {
        let mut m = base_metric("t1");
        m.client = None;

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let row = &recent.snapshot()[0];
        let json = serde_json::to_string(row).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert!(parsed["client"].is_null());
    }

    // --- Snapshot del contrato publicado por `GET /requests` ---

    /// Recolecta TODA clave del JSON, incluidas las anidadas.
    ///
    /// Anidadas también a propósito: `tool_names` no vive en la raíz de la
    /// fila sino dentro de `tools_by_server`, y es justo el campo cuya
    /// ausencia dejó a `oxidegate-lens` sin hacer nada contra un proxy 0.3.1.
    fn claves_recursivas(v: &serde_json::Value, out: &mut std::collections::BTreeSet<String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, sub) in map {
                    out.insert(k.clone());
                    claves_recursivas(sub, out);
                }
            }
            serde_json::Value::Array(items) => {
                for sub in items {
                    claves_recursivas(sub, out);
                }
            }
            _ => {}
        }
    }

    /// Snapshot de las claves de PRIMER NIVEL de una fila de `/requests`.
    ///
    /// Este test no juzga si el contrato es bueno: solo hace que cambiarlo
    /// cueste tocarlo aquí. Renombrar un campo, cambiarle el tipo o quitarlo
    /// rompe a `oxidegate-monitor` y a `oxidegate-lens` en silencio, y hasta
    /// ahora no había nada que lo contara antes que un usuario.
    ///
    /// **Añadir** un campo es aditivo: se agrega a esta lista y ya. **Quitar
    /// o renombrar** es ruptura y obliga a subir
    /// [`CONTRACT_VERSION`](crate::middleware::version::CONTRACT_VERSION).
    #[test]
    fn las_claves_de_requests_no_cambian_sin_querer() {
        let mut recent = RecentRequests::default();
        recent.ingest(&base_metric("t1"));
        let fila = serde_json::to_value(&recent.snapshot()[0]).unwrap();

        let publicadas: Vec<&str> = fila
            .as_object()
            .expect("cada fila de /requests es un objeto")
            .keys()
            .map(String::as_str)
            .collect();

        let esperadas = [
            "cache_by_section",
            "cache_control_forced",
            "cache_read_tokens",
            "cache_write_tokens",
            "client",
            "codex_quota",
            "context_history_bytes",
            "context_last_turn_bytes",
            "context_measured_bytes",
            "context_messages_count",
            "context_other_bytes",
            "context_system_bytes",
            "context_tax_ratio",
            "context_tools_bytes",
            "cost_estimate_usd",
            "effort_forced",
            "hooks",
            "input_share_by_section",
            "input_tokens",
            "instructions",
            "model",
            "output_tokens",
            "prepare_us",
            "prompt_bytes",
            "requested_effort",
            "requested_speed",
            "response_bytes",
            "route",
            "served_speed",
            "session",
            "skills",
            "status",
            "stream",
            "timestamp",
            "tool_calls",
            "tool_search",
            "tools_by_server",
            "tools_flattened",
            "tools_overhead_bytes",
            "total_ms",
            "ttft_ms",
            "upstream",
        ];

        assert_eq!(
            publicadas, esperadas,
            "el contrato de /requests cambió. Si es ADITIVO, actualiza esta \
             lista. Si RENOMBRA, QUITA o cambia el tipo de un campo, sube \
             ademas CONTRACT_VERSION en middleware::version y anótalo en \
             docs/telemetry-per-request.md §8."
        );
    }

    /// `prompt_hash` y `prompt_bytes` NO están en el contrato publicado. La
    /// invariante de privacidad del módulo, afirmada sobre el JSON real y no
    /// solo sobre la definición del tipo.
    #[test]
    fn el_contrato_de_requests_nunca_publica_la_huella_del_prompt() {
        let mut recent = RecentRequests::default();
        recent.ingest(&base_metric("t1"));
        let fila = serde_json::to_value(&recent.snapshot()[0]).unwrap();

        let mut claves = std::collections::BTreeSet::new();
        claves_recursivas(&fila, &mut claves);

        // `prompt_bytes` YA NO está en esta lista: es un entero, no una huella,
        // y desde este slice se publica a propósito (ver el test de abajo). Lo
        // que la invariante protege es la HUELLA individual del prompt, y esa
        // sigue sin salir de acá.
        assert!(
            !claves.contains("prompt_hash"),
            "`prompt_hash` se filtró a /requests: {claves:?}"
        );
    }

    /// `prompt_bytes` se publica, y lleva el tamaño real — no un cero ni un
    /// derivado del desglose.
    ///
    /// Es la mitad de subida que le faltaba a `response_bytes`: hasta ahora
    /// una lente podía responder cuántos bytes BAJARON y no cuántos subieron,
    /// aunque el número llevara meses escribiéndose al `telemetry.jsonl`.
    #[test]
    fn requests_publica_los_bytes_de_subida() {
        let mut m = base_metric("t1");
        m.prompt_bytes = 4242;

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let fila = serde_json::to_value(&recent.snapshot()[0]).unwrap();

        assert_eq!(
            fila["prompt_bytes"], 4242,
            "no publica los bytes de subida: {fila}"
        );
    }

    /// El desglose y el tamaño del body son mediciones DISTINTAS, tomadas en
    /// puntos distintos del pipeline, y publicarlas juntas hace muy fácil
    /// olvidarlo. Este test fija que no se exige que coincidan: si alguien
    /// "arreglara" la diferencia igualándolas, estaría falseando una de las
    /// dos. Ver §4.1 y §4.10 de `docs/telemetry-per-request.md`.
    #[test]
    fn los_bytes_de_subida_no_son_la_suma_del_desglose() {
        let mut m = base_metric("t1");
        m.prompt_bytes = 100;
        m.context_measured_bytes = Some(52);

        let mut recent = RecentRequests::default();
        recent.ingest(&m);
        let fila = serde_json::to_value(&recent.snapshot()[0]).unwrap();

        assert_eq!(fila["prompt_bytes"], 100);
        assert_eq!(fila["context_measured_bytes"], 52);
        assert_ne!(
            fila["prompt_bytes"], fila["context_measured_bytes"],
            "se publicaron como si fueran la misma medición"
        );
    }

    /// Toda capacidad que `/version` anuncia existe de verdad en el JSON.
    ///
    /// Es lo que hace que `fields` sea una respuesta y no una promesa: sin
    /// este test, `/version` podría anunciar un campo que el proxy no publica
    /// y el consumidor volvería a quedarse sin saber si el hueco es del proxy
    /// o del dato.
    #[test]
    fn version_no_anuncia_campos_que_requests_no_publique() {
        let mut recent = RecentRequests::default();
        recent.ingest(&base_metric("t1"));
        let fila = serde_json::to_value(&recent.snapshot()[0]).unwrap();

        let mut claves = std::collections::BTreeSet::new();
        claves_recursivas(&fila, &mut claves);

        for campo in crate::middleware::version::FIELDS {
            assert!(
                claves.contains(campo),
                "/version anuncia `{campo}` y /requests no lo publica"
            );
        }
    }
}
