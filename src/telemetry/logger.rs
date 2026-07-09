//! Escritor de telemetría fuera del camino crítico.
//!
//! El handler solo hace `sink.record(...)` (un `send` a un canal, no bloquea).
//! Una task en background serializa a JSONL y escribe a disco, y de paso
//! alimenta el [`StatsRegistry`](crate::telemetry::stats::StatsRegistry) y el
//! [`RecentRequests`](crate::telemetry::recent::RecentRequests) compartidos
//! para que `/stats` y `/requests` puedan leer, respectivamente, la
//! agregación y el detalle reciente en vivo sin tocar el JSONL. Así el I/O de
//! log NUNCA se suma a la latencia que le devolvemos a gentle-ai.
use crate::provider::{ContextBreakdown, ToolServerBytes};
use crate::telemetry::{RecentRequests, StatsRegistry};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// Una fila de telemetría por request atendido.
///
/// Agrupa tres ejes que en agentes están correlacionados: identidad (para
/// detectar redundancias), coste (tokens y USD) y latencia (los tres tiempos
/// que de verdad importan en streaming). Los campos son `Option` cuando el dato
/// puede faltar legítimamente (p. ej. el proveedor no mandó `usage`, o el modelo
/// no está en la tabla de precios): preferimos un hueco honesto a un cero falso.
#[derive(Debug, Serialize)]
pub struct RequestMetric {
    // --- Identidad ---
    /// Instante en que se emite la métrica (RFC 3339, UTC).
    pub timestamp: String,
    /// Ruta local del proxy que atendió el request (`/v1/messages`, …).
    pub route: String,
    /// Proveedor destino (`anthropic`, `openai`).
    pub upstream: String,
    /// Modelo solicitado, leído del body del request. `None` si no venía.
    pub model: Option<String>,
    /// Huella (hash no criptográfico) del body del request. Igual huella ⇒
    /// mismo prompt: base para detectar peticiones duplicadas o redundantes.
    pub prompt_hash: String,
    /// `true` si el cliente pidió respuesta en streaming (SSE).
    pub stream: bool,

    // --- Coste ---
    /// Tamaño en bytes del body del request (sombra barata del tamaño real).
    pub prompt_bytes: usize,
    /// Tokens de entrada exactos, tal como los reporta el proveedor en `usage`.
    pub input_tokens: Option<u64>,
    /// Tokens de salida exactos, tal como los reporta el proveedor en `usage`.
    pub output_tokens: Option<u64>,
    /// Tokens servidos desde caché (lectura, tarifa reducida), crudos tal
    /// como los reporta el proveedor. `None` si no se midió o el proveedor
    /// no reportó caché en este request.
    pub cache_read_tokens: Option<u64>,
    /// Tokens escritos a caché (creación, sobreprecio), crudos. Solo lo
    /// reportan algunos proveedores (p. ej. Anthropic); `None` en el resto.
    pub cache_write_tokens: Option<u64>,
    /// Coste estimado en USD según la tabla de precios. `None` si no calculable.
    pub cost_estimate_usd: Option<f64>,
    /// `true` si OxideGate inyectó el breakpoint de `cache_control` en este
    /// request (palanca A del optimizador, solo Anthropic). Permite
    /// correlacionar la inyección con los `cache_read_tokens` resultantes de
    /// las llamadas repetidas. `false` si la palanca estaba apagada, el
    /// cliente ya gestionaba su propio caching, o el proveedor no aplica.
    pub cache_control_forced: bool,

    // --- Latencia ---
    /// Código de estado HTTP devuelto al cliente.
    pub status: u16,
    /// Time To First Token: ms desde que recibimos el request hasta el PRIMER
    /// chunk de la respuesta. La métrica que siente el usuario en streaming.
    pub ttft_ms: Option<f64>,
    /// Latencia total: ms desde el request hasta que el stream se cierra.
    pub total_ms: f64,
    /// Velocidad de generación (tokens de salida por segundo). `None` si no
    /// tenemos tokens o el tramo de generación fue nulo.
    pub tokens_per_sec: Option<f64>,

    // --- Desglose de contexto (ver `provider::ContextBreakdown`) ---
    /// Bytes del prompt de sistema. MEDIDOS EN BYTES, nunca tokens (longitud
    /// de re-serializar el fragmento con `serde_json::to_vec`, JSON canónico,
    /// no bytes de wire). `None` si `Provider::decompose` no pudo calcular
    /// nada (body no parseó como JSON o no era un objeto).
    pub context_system_bytes: Option<usize>,
    /// Bytes de los esquemas de herramientas. Mismo contrato de medición que
    /// `context_system_bytes`.
    pub context_tools_bytes: Option<usize>,
    /// Bytes del historial (todos los mensajes menos el último). Mismo
    /// contrato de medición que `context_system_bytes`.
    pub context_history_bytes: Option<usize>,
    /// Bytes del último mensaje (el turno nuevo). Mismo contrato de medición
    /// que `context_system_bytes`.
    pub context_last_turn_bytes: Option<usize>,
    /// Bytes del resto de campos de control a nivel raíz (`model`,
    /// `temperature`, `max_tokens`…). Mismo contrato de medición que
    /// `context_system_bytes`.
    pub context_other_bytes: Option<usize>,
    /// Suma de los cinco campos de contexto anteriores. DIFIERE levemente de
    /// `prompt_bytes` (que sí es el tamaño exacto sobre el cable): este es el
    /// tamaño del JSON canónico re-serializado, no el de los bytes que
    /// realmente mandó el cliente. Nunca combinar `context_measured_bytes`
    /// con `prompt_bytes` en un mismo cociente.
    pub context_measured_bytes: Option<usize>,
    /// Número de mensajes del historial completo (incluyendo el último).
    pub context_messages_count: Option<usize>,
    /// `(system + tools + history) / measured`: fracción del body que es
    /// prefijo estable (ver `ContextBreakdown::context_tax_ratio`).
    ///
    /// ASIMETRÍA A PROPÓSITO: cuando `context_measured_bytes` es `Some(0)`,
    /// esta ratio es `None` (no hay nada de qué sacar fracción, evitamos una
    /// división por cero), mientras que los siete campos en bytes de arriba
    /// SÍ quedan en `Some(0)` (sabemos con certeza que no midieron nada). No
    /// es una inconsistencia: son dos preguntas distintas ("¿cuánto medimos?"
    /// vs. "¿qué fracción es prefijo estable?").
    pub context_tax_ratio: Option<f64>,

    // --- Desglose de herramientas por servidor MCP (ver `provider::ToolServerBytes`) ---
    /// Desglose de `tools` por servidor MCP: cuántas herramientas y cuántos
    /// bytes aporta cada servidor (`(native)`, cada `mcp__<server>__*`
    /// identificado individualmente, y `(others)` si se agotó el cupo de
    /// servidores trackeados —ver `provider::MAX_TOOL_SERVERS`—).
    ///
    /// **ESTE ES EL ÚNICO CAMPO NO-PLANO DE TODA LA FILA.** El resto de
    /// `RequestMetric` son escalares (número, string, booleano) porque el
    /// esquema de columnas de un JSONL de telemetría se fija de antemano.
    /// Acá no puede serlo: la cardinalidad es DEPENDIENTE DEL DATO (una fila
    /// por cada servidor MCP distinto que el cliente declare en ESTE request
    /// puntual, de cero a `provider::MAX_TOOL_SERVERS + 1`), así que no
    /// existe un conjunto fijo de columnas (`tool_server_1_bytes`,
    /// `tool_server_2_bytes`…) que lo cubra sin desperdiciar espacio en la
    /// mayoría de las filas o sin truncar arbitrariamente en las que
    /// declaran más servidores. Un array JSON anidado es la única
    /// representación honesta de este dato.
    ///
    /// `None` y `Some(vec![])` son estados DISTINTOS, mismo criterio que ya
    /// aplica `Provider::tool_entries` entre "ausente" y "vacío": `None`
    /// cuando `Provider::decompose` no produjo nada (el body no parseó como
    /// JSON, o parseó pero no era un objeto — ni siquiera pudimos mirar
    /// adentro); `Some(vec![])` cuando el body SÍ parseó como objeto pero no
    /// declaró ninguna herramienta atribuible a ningún servidor (`tools`
    /// ausente, no-array, o `[]`). Confundir ambos perdería la diferencia
    /// entre "no sabemos" y "sabemos que no hay".
    ///
    /// BYTES, nunca tokens — mismo contrato de medición que los campos
    /// `context_*` de arriba: cada `bytes` de un `ToolServerBytes` es la
    /// longitud de re-serializar con `serde_json::to_vec` el fragmento de esa
    /// herramienta (JSON canónico, no bytes de wire ni tokens del modelo).
    pub tools_by_server: Option<Vec<ToolServerBytes>>,
    /// Bytes de `tools` no atribuidos a ningún servidor (ver
    /// `provider::tools_overhead_bytes`): estructura del array `tools`
    /// (corchetes y comas), wrappers sin atribución propia (el
    /// `functionDeclarations` de Gemini), y herramientas huérfanas sin
    /// `name`. Mismo contrato `None`/`Some` que `tools_by_server` — nacen del
    /// mismo `context.is_some()` calculado en `provider::*::prepare`, nunca
    /// se puede tener uno `Some` y el otro `None`.
    pub tools_overhead_bytes: Option<usize>,

    /// Microsegundos que `middleware::proxy::run` pasó DENTRO de
    /// `Provider::prepare` (parseo del body + `decompose` + mutación
    /// opcional del body). `u64` en MICROsegundos, no `f64` en milisegundos:
    /// a esta magnitud (típicamente decenas a cientos de µs) redondear a ms
    /// como flotante borraría la señal.
    ///
    /// NO incluye: leer el body del socket (eso pasa ANTES de `prepare`, en
    /// `run`), ni el round-trip hacia el proveedor upstream (eso pasa
    /// DESPUÉS, en `send_and_meter`). Es, a propósito, el costo propio del
    /// proxy — la primera vez que OxideGate se mide a sí mismo.
    ///
    /// A partir de este slice, `prepare` también calcula `tools_by_server`
    /// (que re-serializa CADA herramienta individualmente, además del array
    /// completo que ya medía `decompose` para `context_tools_bytes`): sobre
    /// el componente más pesado del body (esquemas de herramientas, decenas
    /// de KB en agentes reales) esto duplica aproximadamente el trabajo de
    /// serialización en el camino crítico. Se espera que `prepare_us` suba
    /// en la misma proporción en requests con muchas herramientas; no se
    /// optimiza acá a propósito (ver informe del cambio).
    pub prepare_us: u64,
}

/// Tupla de los 8 campos `context_*` en el mismo orden en que aparecen en
/// [`RequestMetric`]: `(system_bytes, tools_bytes, history_bytes,
/// last_turn_bytes, other_bytes, measured_bytes, messages_count,
/// tax_ratio)`. Existe solo para que [`flatten_context_breakdown`] tenga un
/// tipo de retorno nombrado (en vez de una tupla de 8 elementos inline, que
/// `clippy::type_complexity` rechaza).
pub(crate) type ContextFieldsTuple = (
    Option<usize>,
    Option<usize>,
    Option<usize>,
    Option<usize>,
    Option<usize>,
    Option<usize>,
    Option<usize>,
    Option<f64>,
);

/// Aplana un [`ContextBreakdown`] opcional en la tupla de 8 campos que
/// exige [`RequestMetric`] (ver el contrato de medición completo en
/// [`ContextBreakdown`]). `None` en la entrada ⇒ los 8 campos en `None`: no
/// hay nada que aplanar porque el body no parseó como JSON o no era un
/// objeto. Es el único lugar que sabe mapear `ContextBreakdown` a la forma
/// plana de la métrica; `middleware::proxy` (camino de error de upstream) y
/// `telemetry::metered` (camino de streaming) llaman a esta función en vez
/// de repetir la lógica de aplanado cada uno por su cuenta.
pub(crate) fn flatten_context_breakdown(context: Option<&ContextBreakdown>) -> ContextFieldsTuple {
    match context {
        Some(c) => (
            Some(c.system_bytes),
            Some(c.tools_bytes),
            Some(c.history_bytes),
            Some(c.last_turn_bytes),
            Some(c.other_bytes),
            Some(c.measured_bytes),
            Some(c.messages_count),
            c.context_tax_ratio(),
        ),
        None => (None, None, None, None, None, None, None, None),
    }
}

/// Deriva los dos campos `tools_by_server`/`tools_overhead_bytes` de
/// [`RequestMetric`] a partir de lo que calculó `Provider::prepare`.
///
/// `Outgoing::tools_by_server` es un `Vec` liso (nunca `Option`): queda
/// vacío tanto si el body no parseó / no era un objeto, como si SÍ era un
/// objeto pero no declaró herramientas — `Outgoing` no distingue esos dos
/// casos por sí solo. La señal que SÍ los distingue es `context.is_some()`
/// (mismo criterio que decide el resto del desglose de contexto, ver
/// `flatten_context_breakdown`): si `context` es `None`, el body no era
/// indexable y no pudimos ni mirar, así que acá se devuelve `None` en vez de
/// `Some(vec![])` (mentir con un vacío "sabido" sería peor que un hueco
/// honesto). Si `context` es `Some`, el dialecto SÍ se evaluó de verdad, así
/// que se devuelve `Some(...)`, aunque el vector venga vacío.
///
/// Único lugar que sabe hacer este mapeo; usado tanto desde
/// `middleware::proxy` (camino de error de upstream) como desde
/// `telemetry::metered` (camino de streaming), igual que
/// `flatten_context_breakdown`.
pub(crate) fn tools_fields(
    context: Option<&ContextBreakdown>,
    tools_by_server: Vec<ToolServerBytes>,
    tools_overhead_bytes: usize,
) -> (Option<Vec<ToolServerBytes>>, Option<usize>) {
    if context.is_some() {
        (Some(tools_by_server), Some(tools_overhead_bytes))
    } else {
        (None, None)
    }
}

/// Handle clonable que los handlers usan para emitir métricas sin bloquear.
#[derive(Clone)]
pub struct TelemetrySink {
    tx: mpsc::UnboundedSender<RequestMetric>,
    /// Agregación en vivo por `(upstream, model)`, alimentada por la misma
    /// task de drenaje que escribe el JSONL. Se comparte con el handler de
    /// `/stats` vía `stats()`.
    stats: Arc<RwLock<StatsRegistry>>,
    /// Buffer en vivo de los últimos N requests individuales, alimentado por
    /// la misma task de drenaje. Se comparte con el handler de `/requests`
    /// vía `recent()`.
    recent: Arc<RwLock<RecentRequests>>,
}

impl TelemetrySink {
    /// Arranca la task escritora y devuelve el handle para emitir métricas.
    pub fn spawn(storage_dir: PathBuf) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<RequestMetric>();
        let stats = Arc::new(RwLock::new(StatsRegistry::default()));
        let stats_writer = Arc::clone(&stats);
        let recent = Arc::new(RwLock::new(RecentRequests::default()));
        let recent_writer = Arc::clone(&recent);

        let mut path = storage_dir;
        path.push("telemetry.jsonl");

        tokio::spawn(async move {
            let mut file = match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("⚠️  telemetría: no se pudo abrir {path:?}: {e}");
                    return;
                }
            };

            while let Some(metric) = rx.recv().await {
                // Lock breve y SIN `.await` dentro: tomamos, actualizamos y
                // soltamos antes de tocar el archivo (I/O async). Nunca debe
                // sostenerse un lock a través de un punto de suspensión.
                //
                // Ante un lock envenenado (un panic previo mientras estaba
                // tomado) recuperamos el guard con `into_inner` en vez de
                // ignorarlo: así el escritor sigue alimentando `/stats`, igual
                // que el lector, y no dejamos las estadísticas congeladas para
                // siempre por un único panic.
                {
                    let mut registry = stats_writer
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    registry.ingest(&metric);
                }

                {
                    let mut recent = recent_writer
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    recent.ingest(&metric);
                }

                if let Ok(mut line) = serde_json::to_string(&metric) {
                    line.push('\n');
                    if let Err(e) = file.write_all(line.as_bytes()).await {
                        eprintln!("⚠️  telemetría: fallo al escribir: {e}");
                    }
                }
            }
        });

        Self { tx, stats, recent }
    }

    /// No bloquea: si el canal se cerró, descartamos la métrica en silencio.
    pub fn record(&self, metric: RequestMetric) {
        let _ = self.tx.send(metric);
    }

    /// Handle compartido a la agregación en vivo, para que el handler de
    /// `/stats` lea un snapshot sin pasar por el canal ni por disco.
    pub fn stats(&self) -> Arc<RwLock<StatsRegistry>> {
        Arc::clone(&self.stats)
    }

    /// Handle compartido al buffer de requests recientes, para que el
    /// handler de `/requests` lea un snapshot sin pasar por el canal ni por
    /// disco.
    pub fn recent(&self) -> Arc<RwLock<RecentRequests>> {
        Arc::clone(&self.recent)
    }
}
