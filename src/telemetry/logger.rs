//! Escritor de telemetría fuera del camino crítico.
//!
//! El handler solo hace `sink.record(...)` (un `send` a un canal, no bloquea).
//! Una task en background serializa a JSONL y escribe a disco, y de paso
//! alimenta el [`StatsRegistry`](crate::telemetry::stats::StatsRegistry) y el
//! [`RecentRequests`](crate::telemetry::recent::RecentRequests) compartidos
//! para que `/stats` y `/requests` puedan leer, respectivamente, la
//! agregación y el detalle reciente en vivo sin tocar el JSONL. Así el I/O de
//! log NUNCA se suma a la latencia que le devolvemos a gentle-ai.
use crate::provider::{
    ContextBreakdown, InstructionsBlock, SkillsBlock, ToolCalls, ToolSearchSignal, ToolServerBytes,
};
use crate::telemetry::{
    CacheBySection, SectionShare, CodexQuota, RecentRequests, SessionAttribution, SessionRegistry, StatsRegistry,
};
use serde::{Deserialize, Serialize};
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
/// # Lectura desde `telemetry.jsonl` (rehidratación)
///
/// `Deserialize` existe para poder releer el histórico que ya está en disco
/// (issue #42). La tolerancia a filas viejas se declara **campo a campo** y no
/// con un `#[serde(default)]` a nivel de struct, a propósito: eso exigiría
/// `Default` en todo el tipo y convertiría un `upstream` ausente en `""`, que
/// es exactamente la clase de cero fabricado que este proyecto no admite. Una
/// fila sin `upstream` está corrupta y debe fallar al parsear.
///
/// Los campos que SÍ llevan `default` son los que una build anterior no
/// escribía: ahí la ausencia es legítima y el `None`/vacío es honesto.
#[derive(Debug, Serialize, Deserialize)]
pub struct RequestMetric {
    // --- Identidad ---
    /// Instante en que se emite la métrica (RFC 3339, UTC).
    pub timestamp: String,
    /// Ruta local del proxy que atendió el request (`/v1/messages`, …).
    pub route: String,
    /// Proveedor destino (`anthropic`, `openai`).
    pub upstream: String,
    /// Modelo solicitado, leído del body del request. `None` si no venía.
    #[serde(default)]
    pub model: Option<String>,
    /// Huella (hash no criptográfico) del body del request. Igual huella ⇒
    /// mismo prompt: base para detectar peticiones duplicadas o redundantes.
    pub prompt_hash: String,
    /// `true` si el cliente pidió respuesta en streaming (SSE).
    pub stream: bool,
    /// `User-Agent` del cliente que originó el request, crudo (sin
    /// normalizar) salvo un tope de longitud (ver
    /// `middleware::proxy::client_of`). Claude Code se identifica con algo
    /// como `claude-cli/1.2.3 (external, cli)`; otros harnesses mandan su
    /// propia cadena. `None` si el header no vino o no era UTF-8 válido.
    ///
    /// Es la pieza que faltaba para distinguir un harness que YA difiere
    /// tools MCP por su cuenta (Claude Code, cuando no cae al fallback de
    /// carga upfront) de uno genuinamente eager (ver
    /// `docs/optimizer-tool-search.md` §3): antes de este campo, "este
    /// tráfico era Claude Code" solo se podía INFERIR por los nombres de
    /// servidor MCP declarados, nunca confirmarse.
    #[serde(default)]
    pub client: Option<String>,

    // --- Coste ---
    /// Tamaño en bytes del body que MANDÓ EL CLIENTE, en su forma lógica.
    ///
    /// **No es el tamaño de wire**, aunque sea lo más cercano que se mide:
    /// excluye el framing HTTP, y en `/v1/codex/responses` y `/v1beta/*` se
    /// calcula sobre el body ya DESCOMPRIMIDO (ver `provider::maybe_decompress`)
    /// — si el cliente comprimió, por el cable subieron menos bytes.
    ///
    /// **Tampoco es lo que subió al proveedor**: se toma sobre el body
    /// ORIGINAL, antes de cualquier mutación, así que con
    /// `cache_control_forced` el body reenviado es MAYOR que este número.
    ///
    /// Contrato completo en `docs/telemetry-per-request.md` §4.10.
    #[serde(default)]
    pub prompt_bytes: usize,
    /// Tokens de entrada exactos, tal como los reporta el proveedor en `usage`.
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// Tokens de salida exactos, tal como los reporta el proveedor en `usage`.
    #[serde(default)]
    pub output_tokens: Option<u64>,
    /// Tokens servidos desde caché (lectura, tarifa reducida), crudos tal
    /// como los reporta el proveedor. `None` si no se midió o el proveedor
    /// no reportó caché en este request.
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    /// Tokens escritos a caché (creación, sobreprecio), crudos. Solo lo
    /// reportan algunos proveedores (p. ej. Anthropic); `None` en el resto.
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
    /// Coste estimado en USD según la tabla de precios. `None` si no calculable.
    #[serde(default)]
    pub cost_estimate_usd: Option<f64>,
    /// `true` si OxideGate inyectó el breakpoint de `cache_control` en este
    /// request (palanca A del optimizador, solo Anthropic). Permite
    /// correlacionar la inyección con los `cache_read_tokens` resultantes de
    /// las llamadas repetidas. `false` si la palanca estaba apagada, el
    /// cliente ya gestionaba su propio caching, o el proveedor no aplica.
    #[serde(default)]
    pub cache_control_forced: bool,
    /// Nivel de esfuerzo de razonamiento PEDIDO por el cliente
    /// (`output_config.effort`, ver `provider::Outgoing::requested_effort`).
    /// Dialecto exclusivo de Anthropic: `None` en OpenAI/Gemini, o si el
    /// campo estaba ausente o no era un string en el body de Anthropic.
    #[serde(default)]
    pub requested_effort: Option<String>,
    /// Modo de velocidad PEDIDO por el cliente (`speed` a nivel raíz del
    /// body, ver `provider::Outgoing::requested_speed`). Dialecto exclusivo
    /// de Anthropic.
    ///
    /// **SEPARADO A PROPÓSITO de `served_speed`** (no se colapsan en un solo
    /// campo): el modo `fast` de Anthropic tiene su propio rate limit, así
    /// que un request puede PEDIR `"fast"` y ser SERVIDO a `"standard"`.
    /// Fusionar ambos en un único campo escondería exactamente el fallo que
    /// este par de campos existe para exponer — un `requested_speed ==
    /// Some("fast")` con `served_speed != Some("fast")` es la señal de que el
    /// rate limit del modo rápido se activó para esta petición.
    #[serde(default)]
    pub requested_speed: Option<String>,
    /// Velocidad con la que el proveedor SIRVIÓ REALMENTE la respuesta
    /// (`usage.speed`, ver `provider::Usage::speed`). Dialecto exclusivo de
    /// Anthropic.
    ///
    /// ESTADO: campo DOCUMENTADO por Anthropic pero NO OBSERVADO todavía en
    /// tráfico real de este proyecto (el modo `fast` no se ejercitó aún).
    /// `None` significa "el proveedor no lo reportó", nunca "sirvió a
    /// velocidad estándar" — no colapsar ambos casos.
    #[serde(default)]
    pub served_speed: Option<String>,

    // --- Latencia ---
    /// Código de estado HTTP devuelto al cliente.
    pub status: u16,
    /// Time To First Token: ms desde que recibimos el request hasta el PRIMER
    /// chunk de la respuesta. La métrica que siente el usuario en streaming.
    #[serde(default)]
    pub ttft_ms: Option<f64>,
    /// Latencia total: ms desde el request hasta que el stream se cierra.
    pub total_ms: f64,
    /// Velocidad de generación (tokens de salida por segundo). `None` si no
    /// tenemos tokens o el tramo de generación fue nulo.
    #[serde(default)]
    pub tokens_per_sec: Option<f64>,

    // --- Desglose de contexto (ver `provider::ContextBreakdown`) ---
    /// Bytes del prompt de sistema. MEDIDOS EN BYTES, nunca tokens (longitud
    /// de re-serializar el fragmento con `serde_json::to_vec`, JSON canónico,
    /// no bytes de wire). `None` si `Provider::decompose` no pudo calcular
    /// nada (body no parseó como JSON o no era un objeto).
    #[serde(default)]
    pub context_system_bytes: Option<usize>,
    /// Bytes de los esquemas de herramientas. Mismo contrato de medición que
    /// `context_system_bytes`.
    #[serde(default)]
    pub context_tools_bytes: Option<usize>,
    /// Bytes del historial (todos los mensajes menos el último). Mismo
    /// contrato de medición que `context_system_bytes`.
    #[serde(default)]
    pub context_history_bytes: Option<usize>,
    /// Bytes del último mensaje (el turno nuevo). Mismo contrato de medición
    /// que `context_system_bytes`.
    #[serde(default)]
    pub context_last_turn_bytes: Option<usize>,
    /// Bytes del resto de campos de control a nivel raíz (`model`,
    /// `temperature`, `max_tokens`…). Mismo contrato de medición que
    /// `context_system_bytes`.
    #[serde(default)]
    pub context_other_bytes: Option<usize>,
    /// Suma de los cinco campos de contexto anteriores. DIFIERE levemente de
    /// `prompt_bytes` (que mide el body tal como lo mandó el cliente): este es
    /// el tamaño del JSON canónico RE-SERIALIZADO, no el de los bytes que
    /// realmente llegaron. Nunca combinar `context_measured_bytes` con
    /// `prompt_bytes` en un mismo cociente: son dos mediciones tomadas en
    /// puntos distintos del pipeline.
    #[serde(default)]
    pub context_measured_bytes: Option<usize>,
    /// Número de mensajes del historial completo (incluyendo el último).
    #[serde(default)]
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
    #[serde(default)]
    pub context_tax_ratio: Option<f64>,

    // --- Atribución de caché por sección (ver `telemetry::cache_attribution`) ---
    /// Qué cubo del contexto cayó dentro del prefijo cacheado, ESTIMADO.
    ///
    /// **Va en un objeto anidado y no suelto entre los campos de arriba a
    /// propósito.** Todo lo demás en este bloque es medición directa de bytes;
    /// esto es una estimación: el proveedor reporta los tokens cacheados en
    /// total, nunca por sección, así que la frontera se deduce convirtiendo
    /// tokens a bytes. Mezclarlos al mismo nivel invitaría a pintarlos en la
    /// misma columna, que es justo el error que el issue #50 marca como el
    /// único irreversible.
    ///
    /// Lleva dentro su propio `method` versionado: un consumidor puede decidir
    /// si entiende el algoritmo antes de dibujar nada con él.
    ///
    /// `None` significa **no atribuible** (sin desglose de contexto, sin
    /// `cache_read_tokens` reportados, o `upstream` desconocido). Todo a cero
    /// significa **medido y nada cacheado**. No colapsar ambos casos.
    ///
    /// **No se deserializa nunca** (`skip_deserializing`). Es un campo de
    /// `GET /requests`, y la rehidratación del histórico alimenta `/stats` y
    /// `/sessions` — que no lo usan. Saltarlo no es un atajo: es la frontera
    /// que el propio issue #42 pide («tampoco rehidratar /requests»). De paso
    /// evita tener que hacer deserializable un `&'static str`, que no lo es.
    #[serde(skip_deserializing)]
    pub cache_by_section: Option<CacheBySection>,
    /// Fracción del input PAGADO que corresponde a cada sección, ESTIMADA.
    ///
    /// Se apoya en `cache_by_section`, así que es **una estimación sobre otra**
    /// y hereda su misma disciplina: objeto anidado, `method` versionado
    /// dentro, `None` honesto en cuanto falta una pieza.
    ///
    /// **Son fracciones de 0 a 1, nunca dinero**, y ninguna clave lleva la
    /// palabra `cost` — hay test que lo guarda. Convertirlas en euros exige
    /// multiplicar por `cost_estimate_usd`, y quien lo haga pasa por leer qué
    /// es ese campo. Ver `telemetry::section_share` y §4.12.
    ///
    /// Mismo motivo que `cache_by_section` para no deserializarse: es un campo
    /// de `/requests`, y la rehidratación alimenta `/stats` y `/sessions`.
    #[serde(skip_deserializing)]
    pub input_share_by_section: Option<SectionShare>,

    // --- Desglose de herramientas por servidor MCP (ver `provider::ToolServerBytes`) ---
    /// Desglose de `tools` por servidor MCP: cuántas herramientas y cuántos
    /// bytes aporta cada servidor (`(native)`, cada `mcp__<server>__*`
    /// identificado individualmente, y `(others)` si se agotó el cupo de
    /// servidores trackeados —ver `provider::MAX_TOOL_SERVERS`—).
    ///
    /// Uno de los TRES campos no-planos de la fila (los otros son
    /// `codex_quota` y `session`, más abajo). El resto de `RequestMetric` son
    /// escalares (número, string,
    /// booleano) porque el esquema de columnas de un JSONL de telemetría se
    /// fija de antemano. Acá no puede serlo: la cardinalidad es DEPENDIENTE
    /// DEL DATO (una fila por cada servidor MCP distinto que el cliente
    /// declare en ESTE request puntual, de cero a
    /// `provider::MAX_TOOL_SERVERS + 1`), así que no existe un conjunto fijo
    /// de columnas (`tool_server_1_bytes`, `tool_server_2_bytes`…) que lo
    /// cubra sin desperdiciar espacio en la mayoría de las filas o sin
    /// truncar arbitrariamente en las que declaran más servidores. Un array
    /// JSON anidado es la única representación honesta de este dato.
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
    #[serde(default)]
    pub tools_by_server: Option<Vec<ToolServerBytes>>,
    /// Bytes de `tools` no atribuidos a ningún servidor (ver
    /// `provider::tools_overhead_bytes`): estructura del array `tools`
    /// (corchetes y comas), wrappers sin atribución propia (el
    /// `functionDeclarations` de Gemini), y herramientas huérfanas sin
    /// `name`. Mismo contrato `None`/`Some` que `tools_by_server` — nacen del
    /// mismo `context.is_some()` calculado en `provider::*::prepare`, nunca
    /// se puede tener uno `Some` y el otro `None`.
    #[serde(default)]
    pub tools_overhead_bytes: Option<usize>,

    /// Señal de carga diferida de herramientas (`tool_search`) del dialecto
    /// OpenAI/Codex Responses (ver `provider::ToolSearchSignal`). El SEGUNDO
    /// campo no-plano de la fila (`tools_by_server` es el primero; `codex_quota`
    /// y `session`, más abajo, son el tercero y cuarto).
    ///
    /// Es el diferenciador eager-vs-lazy por cliente que `tools_by_server` no
    /// puede dar: en este dialecto las tools diferidas NO viajan en `tools[]`
    /// (siempre eager) sino como items `tool_search_output` dentro de
    /// `input[]`. `Some { used: false }` = request Responses/Codex medido, sin
    /// carga diferida este turno (EAGER confirmado); `Some { used: true }` =
    /// LAZY; `None` = dialecto donde no aplica (Anthropic, Gemini, OpenAI Chat)
    /// o body que no parseó.
    ///
    /// **No dobla bytes.** Los bytes de esos items ya los miden los campos
    /// `context_*` (viven en `input`): esta señal solo cuenta y clasifica, no
    /// vuelve a sumar bytes ni toca `tools_by_server`.
    #[serde(default)]
    pub tool_search: Option<ToolSearchSignal>,

    /// Señal de honestidad sobre la ATRIBUCIÓN de `tools_by_server` en el
    /// dialecto Responses/Codex (ver `provider::Provider::tools_flattened`).
    /// Complementa a `tools_by_server`: avisa de cuándo su cubo `(native)` NO
    /// es verificable porque el cliente (`pi`/`opencode`) no usa el
    /// namespacing `mcp__`.
    ///
    /// - `None`: dialecto donde no aplica (Anthropic/Gemini/OpenAI-Chat, cuyo
    ///   `mcp__` es fiable) o body sin herramientas.
    /// - `Some(false)`: hay tools Y al menos una usa `mcp__` — el `(native)`
    ///   de esta fila es de fiar.
    /// - `Some(true)`: hay tools pero NINGUNA usa `mcp__` — el `(native)`
    ///   puede ocultar MCP aplanado (medido en `pi`/`opencode`). Observación
    ///   estructural, NUNCA una atribución inventada: no nombra servidores.
    #[serde(default)]
    pub tools_flattened: Option<bool>,
    /// Listado de skills declarado en el body: `{declared, listing_bytes,
    /// format}`, o `null`. El coste se paga en CADA petición, se invoque una
    /// skill o no. `null` significa "no se reconoció ningún listado" — nunca
    /// "cero skills": ver `provider::skills` y `docs/skills-across-tools.md`.
    #[serde(default)]
    pub skills: Option<SkillsBlock>,
    /// Bloque de instrucciones del usuario declarado en el body:
    /// `{bytes, format}`, o `null`. En Claude Code es el **48% del peaje fijo**
    /// de la sesión (`docs/fixed-toll-claude-code.md` §1), y hasta este campo
    /// sus bytes solo existían diluidos dentro de `context_history_bytes`.
    ///
    /// `null` significa "no se reconoció ningún bloque" — nunca "el usuario no
    /// tiene instrucciones". El caso que lo hace inevitable está medido: Claude
    /// Code IGNORA `AGENTS.md`, así que `null` con ese fichero en el proyecto
    /// es la respuesta correcta. Ver `provider::instructions`.
    #[serde(default)]
    pub instructions: Option<InstructionsBlock>,
    /// Nivel de esfuerzo que IMPUSO la palanca B del optimizador, o `null` si
    /// el proxy no intervino — el caso por defecto, porque arranca apagada.
    ///
    /// **Se lee JUNTO a `requested_effort`, nunca en su lugar.** Ese campo
    /// dice lo que pidió el cliente (leído antes de mutar); este, lo que subió
    /// de verdad. Una fila con `requested_effort: "high"` y
    /// `effort_forced: "low"` avisa de que sus `output_tokens` son del segundo:
    /// sin este campo, un ahorro provocado por el propio medidor sería
    /// indistinguible de uno del cliente, que es el peor fallo que este
    /// proyecto puede cometer.
    ///
    /// Ver `docs/optimizer-effort.md` y `docs/telemetry-per-request.md` §4.14.
    #[serde(default)]
    pub effort_forced: Option<String>,
    /// Invocaciones de herramienta observadas en la RESPUESTA
    /// (`provider::ToolCalls`). **Contrapartida de `tool_names`**, que dice
    /// lo que el cliente DECLARA: esto dice lo que el modelo USA. Cruzar los
    /// dos sobre el histórico es lo único que permite afirmar "pagas 12.400 B
    /// por este servidor MCP y no has invocado ninguna de sus herramientas",
    /// la palanca más grande del catálogo (−55.098 B).
    ///
    /// **`None` significa "este proveedor no tiene extractor", y por eso NO
    /// es un `Vec` vacío.** Hoy solo Anthropic lo tiene, y las filas
    /// anteriores a que existiera el campo también rehidratan como `None`.
    /// Un `Some` con listas vacías es una afirmación distinta y mucho más
    /// fuerte: se escaneó la respuesta y el modelo no invocó nada. Fundir
    /// ambas en un vector vacío haría que el recomendador contase como
    /// "servidor sin usar" cada fila escrita antes del extractor.
    ///
    /// Antes de concluir que un servidor no se usa hay que mirar además dos
    /// cosas dentro del bloque: `complete` (si vale `false`, las listas son
    /// un prefijo — el turno se abortó) y `invoked_total` frente a
    /// `invoked.len()` (si difieren, la lista está recortada por el cupo).
    /// Ver `docs/telemetry-per-request.md` §4.15.
    #[serde(default)]
    pub tool_calls: Option<ToolCalls>,
    /// Bytes del CUERPO DE LA RESPUESTA que cruzaron el proxy. `None` si no
    /// llegó a haber respuesta del upstream.
    ///
    /// **Sin comprimir, y no es lo mismo que ancho de banda.** El proxy
    /// descarta `Accept-Encoding` para poder leer el SSE en texto plano y
    /// sacar el `usage`; sin el medidor delante, el cliente habría recibido
    /// esta respuesta comprimida. Mide el TAMAÑO DEL CONTENIDO que bajó, no
    /// los bytes que se habrían pagado en la red sin proxy.
    ///
    /// No se combina con `prompt_bytes` en un mismo ratio sin decir en voz
    /// alta que uno es wire de subida y el otro contenido de bajada.
    #[serde(default)]
    pub response_bytes: Option<usize>,

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
    #[serde(default)]
    pub prepare_us: u64,

    // --- Cuota de suscripción de Codex (ver `telemetry::codex_quota`) ---
    /// Estado de la cuota de suscripción de Codex, parseado de las doce
    /// cabeceras `x-codex-*` que manda el backend de Codex cuando el
    /// request se enrutó vía OAuth (plan de suscripción, no API key). El
    /// TERCER campo no-plano de la fila (ver `tools_by_server` y `tool_search`
    /// arriba); `session`, más abajo, es el CUARTO.
    ///
    /// `None` si el tráfico no llevaba ninguna cabecera `x-codex-*`
    /// (Anthropic, Gemini, o OpenAI vía API key en `api.openai.com`) — la
    /// PRESENCIA de cabeceras es la única señal discriminadora, nunca el
    /// `upstream` ni el slug del modelo (ver `telemetry::codex_quota` para
    /// el contrato completo). También `None` en la rama de error de
    /// upstream (`middleware::proxy`), donde no llegó a existir respuesta
    /// que inspeccionar.
    ///
    /// SEPARADO A PROPÓSITO de `cost_estimate_usd`: la cuota es un
    /// porcentaje de ventana en un plan de precio fijo, nunca un importe en
    /// dólares. Ninguna función de este proyecto mezcla ambas entradas —
    /// ver la garantía estructural documentada en `telemetry::codex_quota`.
    #[serde(default)]
    pub codex_quota: Option<CodexQuota>,

    // --- Atribución de sesión (ver `telemetry::session`) ---
    /// Sesión resuelta por precedencia de cabeceras del REQUEST entrante
    /// (`middleware::proxy::session_of`): `X-OxideGate-Session` explícito,
    /// `x-claude-code-session-id` nativo, o el bucket de fallback
    /// `Unattributed` con el `User-Agent` como valor. El CUARTO campo
    /// no-plano de la fila (ver `tools_by_server`, `tool_search` y
    /// `codex_quota` arriba).
    ///
    /// Nunca `Option`: la precedencia siempre resuelve a algo — la peor rama
    /// es un fallback honesto, no una ausencia (ver `telemetry::session`
    /// para el contrato completo de honestidad `source`+`key`). A
    /// diferencia de `codex_quota`, que depende de la RESPUESTA del
    /// upstream y por eso puede quedar en `None` si el upstream falla,
    /// `session` se resuelve de las cabeceras del REQUEST y por eso está
    /// disponible idéntico tanto en el camino de éxito como en el de error
    /// de `middleware::proxy::send_and_meter`.
    #[serde(default)]
    pub session: SessionAttribution,
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
    /// Agregación por sesión, alimentada en la misma task de drenaje.
    sessions: Arc<RwLock<SessionRegistry>>,
}

/// Línea de confirmación que se imprime UNA sola vez, la primera vez que el
/// proxy mide una petición.
///
/// # Por qué existe
///
/// Un cableado mal puesto no da error: el agente sigue funcionando —hablando
/// directamente con el proveedor— y OxideGate se queda callado con `/stats`
/// vacío. El usuario no tiene forma de distinguir "todavía no he lanzado nada"
/// de "lo he lanzado y no pasa por aquí" sin ir a consultar el endpoint a mano.
/// Este banner convierte ese silencio en una respuesta inmediata y no
/// solicitada.
///
/// # Qué NO hace
///
/// No fabrica el nombre del cliente. Sin `User-Agent` legible lo declara
/// ausente, igual que el resto de la telemetría distingue un dato que falta de
/// un cero real. Y no afirma "todo bien" ante un status que no es 2xx: el
/// cableado funcionó (la petición llegó y se midió), pero el código real se
/// muestra tal cual para que un 401 no se lea como éxito.
fn first_request_banner(client: Option<&str>, upstream: &str, route: &str, status: u16) -> String {
    let quien = client.unwrap_or("cliente sin identificar");
    let veredicto = if (200..300).contains(&status) {
        "el cableado funciona"
    } else {
        "el cableado funciona, pero esta petición no fue un 2xx"
    };

    format!(
        "✅ Primera petición medida — {veredicto}.\n   \
         {quien} → {upstream} {route} {status}\n   \
         Dashboard en vivo: oxidegate-monitor"
    )
}

impl TelemetrySink {
    /// Arranca la task escritora y devuelve el handle para emitir métricas.
    pub fn spawn(storage_dir: PathBuf) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<RequestMetric>();
        let stats = Arc::new(RwLock::new(StatsRegistry::default()));
        let stats_writer = Arc::clone(&stats);
        let recent = Arc::new(RwLock::new(RecentRequests::default()));
        let recent_writer = Arc::clone(&recent);
        let sessions = Arc::new(RwLock::new(SessionRegistry::default()));
        let sessions_writer = Arc::clone(&sessions);

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

            // La confirmación es de la PRIMERA petición, así que basta un bool
            // local: esta task es la única dueña del flag y consume las
            // métricas en serie, sin necesidad de atómicos ni locks. Vive en el
            // drenaje —fuera del camino crítico— para que anunciar no le cueste
            // latencia a la petición que se está midiendo.
            let mut ya_anunciada = false;

            while let Some(metric) = rx.recv().await {
                if !ya_anunciada {
                    ya_anunciada = true;
                    println!(
                        "{}",
                        first_request_banner(
                            metric.client.as_deref(),
                            &metric.upstream,
                            &metric.route,
                            metric.status,
                        )
                    );
                }

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

                {
                    let mut sessions = sessions_writer
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    sessions.ingest(&metric);
                }

                if let Ok(mut line) = serde_json::to_string(&metric) {
                    line.push('\n');
                    if let Err(e) = file.write_all(line.as_bytes()).await {
                        eprintln!("⚠️  telemetría: fallo al escribir: {e}");
                    }
                }
            }
        });

        Self {
            tx,
            stats,
            recent,
            sessions,
        }
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

    /// Handle compartido de la agregación por sesión, para `GET /sessions`.
    pub fn sessions(&self) -> Arc<RwLock<SessionRegistry>> {
        Arc::clone(&self.sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::SessionSource;

    /// Construye un `RequestMetric` mínimo (resto de campos en su valor más
    /// neutro) con la `session` dada, para ejercitar el round-trip serde de
    /// ese único campo sin depender de una fixture compartida con
    /// `recent.rs` (fuera de alcance de este PR).
    fn minimal_metric(session: SessionAttribution) -> RequestMetric {
        RequestMetric {
            timestamp: "2026-07-15T00:00:00Z".to_string(),
            route: "/v1/messages".to_string(),
            upstream: "anthropic".to_string(),
            model: None,
            prompt_hash: "hash".to_string(),
            stream: false,
            client: None,
            prompt_bytes: 0,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cost_estimate_usd: None,
            cache_control_forced: false,
            requested_effort: None,
            requested_speed: None,
            served_speed: None,
            tool_search: None,
            tools_flattened: None,
            skills: None,
            instructions: None,
            effort_forced: None,
            tool_calls: None,
            response_bytes: None,
            status: 200,
            ttft_ms: None,
            total_ms: 0.0,
            tokens_per_sec: None,
            context_system_bytes: None,
            context_tools_bytes: None,
            context_history_bytes: None,
            context_last_turn_bytes: None,
            context_other_bytes: None,
            context_measured_bytes: None,
            context_messages_count: None,
            context_tax_ratio: None,
            cache_by_section: None,
            input_share_by_section: None,
            tools_by_server: None,
            tools_overhead_bytes: None,
            prepare_us: 0,
            codex_quota: None,
            session,
        }
    }

    /// El banner nombra al cliente que mandó la petición: es el dato que
    /// responde "¿es MI agente el que está pasando por aquí?", y sin él la
    /// confirmación no distingue el tráfico propio de una sonda cualquiera.
    #[test]
    fn banner_nombra_al_cliente_cuando_viene() {
        let banner = first_request_banner(Some("claude-cli/2.0.1"), "anthropic", "/v1/messages", 200);

        assert!(
            banner.contains("claude-cli/2.0.1"),
            "no nombra al cliente: {banner}"
        );
    }

    /// Sin `User-Agent` legible, el banner NO fabrica un nombre: lo dice.
    /// Mismo contrato de honestidad que el resto de la telemetría — un dato
    /// ausente se declara ausente, nunca se rellena con un valor plausible.
    #[test]
    fn banner_sin_cliente_lo_declara_en_vez_de_inventarlo() {
        let banner = first_request_banner(None, "anthropic", "/v1/messages", 200);

        assert!(
            banner.contains("sin identificar"),
            "no declara la ausencia: {banner}"
        );
        assert!(
            !banner.contains("unknown") && !banner.contains("null"),
            "rellena el hueco con un valor fabricado: {banner}"
        );
    }

    /// El banner sitúa la petición: proveedor, ruta y código. Sin los tres, el
    /// usuario sabe que "algo" pasó pero no si es lo que esperaba.
    #[test]
    fn banner_situa_la_peticion_con_upstream_ruta_y_status() {
        let banner = first_request_banner(Some("opencode/1.0"), "openai", "/v1/chat/completions", 200);

        assert!(banner.contains("openai"), "falta el upstream: {banner}");
        assert!(
            banner.contains("/v1/chat/completions"),
            "falta la ruta: {banner}"
        );
        assert!(banner.contains("200"), "falta el status: {banner}");
    }

    /// El banner deja el siguiente paso a la vista. Confirmar que el cableado
    /// funciona sin decir dónde mirar deja al usuario en el mismo sitio.
    #[test]
    fn banner_apunta_al_monitor() {
        let banner = first_request_banner(Some("claude-cli/2.0.1"), "anthropic", "/v1/messages", 200);

        assert!(
            banner.contains("oxidegate-monitor"),
            "no dice dónde mirar: {banner}"
        );
    }

    /// Una primera petición que falló NO se anuncia como éxito de cableado:
    /// el cableado sí funcionó (la petición llegó), pero el banner debe
    /// mostrar el código real para que un 401 no se lea como "todo bien".
    #[test]
    fn banner_con_status_de_error_muestra_el_codigo_real() {
        let banner = first_request_banner(Some("claude-cli/2.0.1"), "anthropic", "/v1/messages", 401);

        assert!(banner.contains("401"), "oculta el status real: {banner}");
    }

    /// Round-trip serde con `session.source = Explicit`: el JSON serializa
    /// `"source": "explicit"` y la `key` correspondiente (mismo patrón que
    /// `round_trip_serde_con_codex_quota_presente` de `recent.rs`).
    #[test]
    fn round_trip_serde_con_session_explicit() {
        let metric = minimal_metric(SessionAttribution {
            source: SessionSource::Explicit,
            key: "claude-1".to_string(),
        });

        let json = serde_json::to_value(&metric).expect("RequestMetric serializa a JSON");
        assert_eq!(json["session"]["source"], "explicit");
        assert_eq!(json["session"]["key"], "claude-1");
    }

    /// Round-trip serde con `session.source = Native`.
    #[test]
    fn round_trip_serde_con_session_native() {
        let metric = minimal_metric(SessionAttribution {
            source: SessionSource::Native,
            key: "native-session-9".to_string(),
        });

        let json = serde_json::to_value(&metric).expect("RequestMetric serializa a JSON");
        assert_eq!(json["session"]["source"], "native");
        assert_eq!(json["session"]["key"], "native-session-9");
    }

    /// Round-trip serde con `session.source = Unattributed`: afirma la
    /// forma con el `User-Agent` como valor y, por separado, la constante
    /// de fallback cuando no hay `User-Agent`.
    #[test]
    fn round_trip_serde_con_session_unattributed() {
        let con_user_agent = minimal_metric(SessionAttribution {
            source: SessionSource::Unattributed,
            key: "claude-cli/1.2.3 (external, cli)".to_string(),
        });
        let json = serde_json::to_value(&con_user_agent).expect("RequestMetric serializa a JSON");
        assert_eq!(json["session"]["source"], "unattributed");
        assert_eq!(json["session"]["key"], "claude-cli/1.2.3 (external, cli)");

        let sin_user_agent = minimal_metric(SessionAttribution {
            source: SessionSource::Unattributed,
            key: "unattributed".to_string(),
        });
        let json = serde_json::to_value(&sin_user_agent).expect("RequestMetric serializa a JSON");
        assert_eq!(json["session"]["source"], "unattributed");
        assert_eq!(json["session"]["key"], "unattributed");
    }
}
