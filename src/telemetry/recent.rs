//! Ring buffer de los últimos N requests atendidos, en detalle individual.
//!
//! `RequestMetric` ya trae el detalle por request, pero hoy ese detalle solo
//! llega a `telemetry.jsonl`: no hay forma de ver en vivo qué pasó en una
//! petición puntual sin leer el archivo. Este módulo guarda una proyección
//! compacta de las últimas [`RECENT_CAPACITY`] métricas en memoria, para que
//! un consumidor (el monitor TUI, hoy; cualquier vista futura) pueda detectar
//! requests ATÍPICOS (outliers de latencia, coste o tokens) sin tocar disco.
//!
//! INVARIANTE CRÍTICA: `prompt_hash` NUNCA se expone acá. Igual que
//! documenta [`middleware::stats`](crate::middleware::stats) para los
//! agregados, esta vista tampoco filtra huellas individuales de prompt: solo
//! expone los campos de coste/latencia/identidad de ruta que ya son
//! públicamente inofensivos.
//!
//! La misma invariante aplica al desglose de herramientas por servidor
//! (`tools_by_server`): lo que se expone es la ETIQUETA del servidor
//! (`(native)`, `claude_ai_Gmail`, `(others)`…) y un conteo de bytes/cantidad
//! de herramientas, NUNCA el nombre individual de cada herramienta ni ningún
//! fragmento del `input_schema`/`description` que la compone. Un nombre de
//! servidor no es contenido de prompt: no filtra nada que el propio cliente
//! no le haya declarado ya al proveedor en texto plano.
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
use crate::provider::ToolServerBytes;
use crate::telemetry::logger::RequestMetric;
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
/// original, PERO deliberadamente omite `prompt_hash` y `prompt_bytes`: el
/// primero por la invariante de privacidad (ninguna huella individual sale
/// de este módulo), el segundo porque es un detalle de implementación del
/// tamaño del body que no aporta a detectar outliers.
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
    /// Microsegundos que el proxy pasó dentro de `Provider::prepare`
    /// (parseo, `decompose` y mutación opcional del body). No incluye la
    /// lectura del body del socket ni el round-trip upstream.
    pub prepare_us: u64,
}

impl From<&RequestMetric> for RecentRequest {
    /// Copia campo a campo desde `RequestMetric`, excluyendo `prompt_hash` y
    /// `prompt_bytes` a propósito (ver invariante de privacidad en el header
    /// del módulo).
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
            tools_by_server: m.tools_by_server.clone(),
            tools_overhead_bytes: m.tools_overhead_bytes,
            prepare_us: m.prepare_us,
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
            tools_by_server: Some(vec![ToolServerBytes {
                server: "claude_ai_Gmail".to_string(),
                kind: crate::provider::ToolServerKind::Mcp,
                tools: 2,
                bytes: 30,
                deferred_tools: 0,
            }]),
            tools_overhead_bytes: Some(4),
            prepare_us: 42,
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
                deferred_tools: 0,
            }])
        );
        assert_eq!(row.tools_overhead_bytes, Some(4));
        assert_eq!(row.prepare_us, 42);
    }

    /// `RecentRequest` NUNCA debe exponer `prompt_hash` ni `prompt_bytes`
    /// (invariante de privacidad documentada en el header del módulo): lo
    /// verificamos a nivel de JSON serializado, no solo por inspección del
    /// tipo, para que un futuro `#[serde(flatten)]` accidental no cuele estas
    /// claves sin que ningún test lo note.
    #[test]
    fn recent_request_no_expone_prompt_hash_ni_prompt_bytes() {
        let mut recent = RecentRequests::default();
        recent.ingest(&base_metric("t1"));

        let row = &recent.snapshot()[0];
        let json = serde_json::to_string(row).unwrap();

        assert!(!json.contains("prompt_hash"), "no debe exponer prompt_hash");
        assert!(!json.contains("prompt_bytes"), "no debe exponer prompt_bytes");
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
}
