//! Envoltorio de medición sobre el stream de respuesta del proveedor.
//!
//! El passthrough original hacía `Body::from_stream(resp.bytes_stream())` y se
//! desentendía. Aquí interponemos [`MeteredBody`]: reenvía cada chunk SIN
//! modificarlo (no bufferiza, no rompe el SSE) pero de paso:
//!   1. marca el TTFT en el primer chunk,
//!   2. va escaneando los eventos SSE en busca del `usage` del proveedor
//!      (delegando la forma exacta en `Provider::extract_usage`),
//!   3. al cerrarse el stream calcula coste/velocidad y emite la métrica.
//!
//! La métrica se emite UNA sola vez, tanto si el stream termina limpio como si
//! el cliente se desconecta a media respuesta (vía `Drop`). Este módulo es
//! mecánica PURA de medición: no conoce el dialecto de ningún proveedor
//! concreto, solo el trait [`Provider`].
//!
//! Desde este slice también transporta el par PEDIDO/SERVIDO de velocidad:
//! `MetricBase::requested_effort`/`requested_speed` nacen en `Outgoing` (se
//! conocen ANTES de la respuesta); `served_speed` sale de `Usage::speed`, que
//! el escáner de `usage` recién conoce al leer la respuesta — por eso viaja
//! en `self.scanner.usage.speed`, no en `MetricBase`.
use crate::provider::{
    ContextBreakdown, InstructionsBlock, Provider, SkillsBlock, ToolCalls, ToolSearchSignal,
    ToolServerBytes, Usage,
};
use crate::telemetry::cache_attribution;
use crate::telemetry::section_share;
use crate::telemetry::logger::{flatten_context_breakdown, tools_fields};
use crate::telemetry::pricing;
use crate::telemetry::{CodexQuota, RequestMetric, SessionAttribution, TelemetrySink};
use bytes::Bytes;
use futures_util::Stream;
use serde_json::Value;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

/// Datos conocidos ANTES de que la respuesta empiece a fluir.
///
/// Se rellena en el handler (ruta, upstream, modelo, huella del prompt…) y se
/// combina con lo medido durante el stream para construir la métrica final.
pub struct MetricBase {
    pub timestamp: String,
    pub route: String,
    pub upstream: String,
    pub model: Option<String>,
    pub prompt_hash: String,
    pub stream: bool,
    pub prompt_bytes: usize,
    pub status: u16,
    /// `User-Agent` del cliente, leído de los headers del request entrante
    /// ANTES de que exista ningún `Outgoing` (`middleware::proxy::client_of`).
    /// Viaja intacto hasta `RequestMetric::client`.
    pub client: Option<String>,
    /// Atribución de sesión resuelta por precedencia de cabeceras
    /// (`middleware::proxy::session_of`), ANTES de que exista ningún
    /// `Outgoing`, igual que `client`. Nunca `Option`: la peor rama es
    /// `SessionSource::Unattributed`, un bucket honesto, no una ausencia
    /// (ver `telemetry::session` para el contrato completo). Viaja intacto
    /// hasta `RequestMetric::session`.
    pub session: SessionAttribution,
    /// `true` si `provider.prepare` inyectó un breakpoint de `cache_control`
    /// en el body saliente (palanca A del optimizador). Nace en `Outgoing` y
    /// viaja intacto hasta la métrica final.
    pub cache_control_forced: bool,
    /// Desglose del body por componente, calculado por `provider.prepare`
    /// (`Outgoing::context`). `None` si el body no parseó como JSON o no era
    /// un objeto. Se aplana a los ocho campos `context_*` de `RequestMetric`
    /// recién en [`MeteredBody::emit`], vía
    /// [`flatten_context_breakdown`](crate::telemetry::logger::flatten_context_breakdown).
    pub context: Option<ContextBreakdown>,
    /// Desglose de `tools` por servidor MCP, calculado por `provider.prepare`
    /// (`Outgoing::tools_by_server`). Vacío por construcción tanto si el
    /// body no parseó como si parseó pero no declaró herramientas — ver
    /// `telemetry::logger::tools_fields`, que es quien recupera esa
    /// distinción usando `context.is_some()` al emitir la métrica final.
    pub tools_by_server: Vec<ToolServerBytes>,
    /// Bytes de `tools` no atribuidos a ningún servidor
    /// (`Outgoing::tools_overhead_bytes`). Mismo criterio que
    /// `tools_by_server` sobre cuándo vale `0` "de verdad" vs. "no se pudo
    /// calcular".
    pub tools_overhead_bytes: usize,
    /// Microsegundos que `middleware::proxy::run` pasó dentro de
    /// `provider.prepare(...)`. Viaja intacto hasta `RequestMetric::prepare_us`.
    pub prepare_us: u64,
    /// Nivel de esfuerzo de razonamiento PEDIDO por el cliente
    /// (`Outgoing::requested_effort`). Viaja intacto hasta
    /// `RequestMetric::requested_effort`.
    pub requested_effort: Option<String>,
    /// Modo de velocidad PEDIDO por el cliente (`Outgoing::requested_speed`).
    /// Viaja intacto hasta `RequestMetric::requested_speed`. Ver esa doc para
    /// por qué es un campo SEPARADO de la velocidad servida (`served_speed`,
    /// que sale de `self.scanner.usage.speed` recién en [`MeteredBody::emit`],
    /// no acá: solo se conoce después de leer la respuesta).
    pub requested_speed: Option<String>,
    /// Señal de carga diferida de herramientas medida en `prepare`
    /// (`Outgoing::tool_search`, solo dialecto Responses/Codex). Viaja intacta
    /// hasta `RequestMetric::tool_search`. `None` en el resto de dialectos.
    pub tool_search: Option<ToolSearchSignal>,
    /// Señal de honestidad sobre la atribución de `tools_by_server`
    /// (`Outgoing::tools_flattened`, solo dialecto Responses/Codex). Viaja
    /// intacta hasta `RequestMetric::tools_flattened`. `None` en el resto.
    pub tools_flattened: Option<bool>,
    /// Listado de skills declarado en el body (`Outgoing::skills`). Viaja
    /// intacto hasta `RequestMetric::skills`. `None` = no se pudo ver.
    pub skills: Option<SkillsBlock>,
    /// Bloque de instrucciones del usuario declarado en el body
    /// (`Outgoing::instructions`). Viaja intacto hasta
    /// `RequestMetric::instructions`. `None` = no se pudo ver.
    pub instructions: Option<InstructionsBlock>,
    /// Nivel de esfuerzo impuesto por la palanca B (`Outgoing::effort_forced`).
    /// Viaja intacto hasta `RequestMetric::effort_forced`. `None` = el proxy no
    /// intervino.
    pub effort_forced: Option<String>,
    /// Proveedor dueño del dialecto de esta respuesta: la extracción del
    /// `usage` se delega íntegramente en él, así este módulo no necesita
    /// saber nada de ningún proveedor concreto.
    pub provider: &'static dyn Provider,
    /// Cuota de suscripción de Codex, parseada de las cabeceras `x-codex-*`
    /// de la respuesta del upstream (`CodexQuota::from_headers`, ver
    /// `middleware::proxy::send_and_meter`). `None` si la petición no fue a
    /// Codex vía OAuth (Anthropic, Gemini, OpenAI vía API key) o si el
    /// upstream falló antes de que hubiera respuesta que inspeccionar. Viaja
    /// intacto hasta `RequestMetric::codex_quota`.
    pub codex_quota: Option<CodexQuota>,
}

/// Acumulador incremental que extrae `input/output_tokens` del cuerpo de la
/// respuesta, sea SSE (streaming) o un único JSON (no-streaming). La forma
/// exacta del `usage` la conoce el `provider`, no este escáner.
struct UsageScanner {
    /// `true` si la respuesta es SSE; decide la estrategia de parseo.
    is_stream: bool,
    /// Buffer de línea parcial: un chunk puede partir un evento SSE por la mitad.
    line_buf: Vec<u8>,
    /// Cuerpo completo acumulado, solo en modo no-streaming (un JSON suelto).
    full_body: Vec<u8>,
    /// Proveedor al que se delega la extracción del `usage` de cada valor JSON.
    provider: &'static dyn Provider,
    usage: Usage,
    /// Invocaciones de herramienta vistas en la respuesta. Se acumulan sobre
    /// el MISMO `Value` que ya se parseó para el `usage`: ni un recorrido
    /// extra del stream ni un byte bufferizado de más.
    calls: ToolCalls,
}

impl UsageScanner {
    fn new(is_stream: bool, provider: &'static dyn Provider) -> Self {
        Self {
            is_stream,
            line_buf: Vec::new(),
            full_body: Vec::new(),
            provider,
            usage: Usage::default(),
            calls: ToolCalls::default(),
        }
    }

    /// Ingiere un chunk de la respuesta. En streaming corta por líneas y parsea
    /// cada evento `data:`; en no-streaming acumula para parsear el JSON al final.
    fn feed(&mut self, chunk: &[u8]) {
        if !self.is_stream {
            self.full_body.extend_from_slice(chunk);
            return;
        }

        self.line_buf.extend_from_slice(chunk);
        // Procesamos todas las líneas completas; dejamos el resto para el próximo
        // chunk (la respuesta puede cortarse en cualquier byte).
        while let Some(pos) = self.line_buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.line_buf.drain(..=pos).collect();
            self.scan_sse_line(&line);
        }
    }

    /// Parsea una línea SSE. Solo nos interesan las líneas `data: {json}`.
    fn scan_sse_line(&mut self, line: &[u8]) {
        let Ok(text) = std::str::from_utf8(line) else {
            return;
        };
        let text = text.trim();
        let Some(payload) = text.strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            return;
        }
        if let Ok(value) = serde_json::from_str::<Value>(payload) {
            self.provider.extract_usage(&value, &mut self.usage);
            self.provider.extract_tool_use(&value, &mut self.calls);
        }
    }

    /// Cierre del stream: en no-streaming el `usage` vive en el JSON completo.
    fn finish(&mut self) {
        if self.is_stream {
            // Un upstream que corta la conexion justo tras el ultimo
            // `data: {...}` deja ese evento en `line_buf` sin su `\n`
            // final, y hasta aqui se descartaba entero. Afectaba ya al
            // `usage`; ahora ademas se perderia una invocacion, asi que se
            // vacia el resto antes de cerrar. `scan_sse_line` ignora lo que
            // no sea un `data:` valido, de modo que un remanente a medio
            // JSON no hace nada — no puede corromper el acumulado.
            if !self.line_buf.is_empty() {
                let resto = std::mem::take(&mut self.line_buf);
                self.scan_sse_line(&resto);
            }
            return;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&self.full_body) {
            self.provider.extract_usage(&value, &mut self.usage);
            self.provider.extract_tool_use(&value, &mut self.calls);
        }
    }
}

/// Stream que envuelve la respuesta del proveedor para medirla al vuelo.
///
/// Reenvía los chunks intactos (transparencia total hacia el cliente) mientras
/// acumula telemetría. Es `Unpin` porque el stream interno va en `Pin<Box<..>>`.
pub struct MeteredBody {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    sink: TelemetrySink,
    base: MetricBase,
    /// Instante en que el proxy recibió el request (origen de TTFT y total).
    start: Instant,
    ttft_ms: Option<f64>,
    scanner: UsageScanner,
    /// Bytes del cuerpo de la respuesta que han cruzado el proxy.
    ///
    /// Se acumula en el MISMO recorrido que ya hace `poll_next`: ni un segundo
    /// pase ni bufferizar la respuesta entera.
    ///
    /// **Son bytes SIN COMPRIMIR, y eso importa.** El proxy descarta
    /// `Accept-Encoding` a propósito (ver `middleware::proxy`) para poder leer
    /// el SSE en texto plano y extraer el `usage`. Sin el proxy en medio, el
    /// cliente habría recibido esta misma respuesta comprimida. Es una medida
    /// honesta del TAMAÑO DEL CONTENIDO, no del ancho de banda que se habría
    /// consumido sin el medidor delante.
    response_bytes: usize,
    /// Guarda para no emitir la métrica dos veces (fin de stream + Drop).
    emitted: bool,
}

impl MeteredBody {
    /// Envuelve `inner` con la telemetría descrita en `base`.
    ///
    /// `start` debe ser el instante en que se recibió el request, para que el
    /// TTFT y la latencia total reflejen la experiencia real del cliente.
    pub fn new(
        inner: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
        sink: TelemetrySink,
        base: MetricBase,
        start: Instant,
    ) -> Self {
        let is_stream = base.stream;
        let provider = base.provider;
        Self {
            inner: Box::pin(inner),
            sink,
            base,
            start,
            ttft_ms: None,
            scanner: UsageScanner::new(is_stream, provider),
            response_bytes: 0,
            emitted: false,
        }
    }

    /// Construye y emite la métrica final. Idempotente gracias a `emitted`.
    fn emit(&mut self) {
        if self.emitted {
            return;
        }
        self.emitted = true;
        self.scanner.finish();

        let total_ms = self.start.elapsed().as_secs_f64() * 1000.0;
        let cost_estimate_usd = pricing::estimate_cost_usd(
            self.base.model.as_deref(),
            self.scanner.usage.input_tokens,
            self.scanner.usage.output_tokens,
            self.scanner.usage.cache_read_tokens,
            self.scanner.usage.cache_write_tokens,
        );

        // Velocidad de generación = tokens de salida / tramo de generación
        // (total − TTFT). Solo tiene sentido en STREAMING: en una respuesta
        // no-streaming todo llega de golpe (ttft ≈ total) y el tramo tiende a
        // cero, disparando un número absurdo. Fuera de streaming la anulamos.
        let tokens_per_sec = match (
            self.base.stream,
            self.scanner.usage.output_tokens,
            self.ttft_ms,
        ) {
            (true, Some(out), Some(ttft)) if total_ms > ttft => {
                Some(out as f64 / ((total_ms - ttft) / 1000.0))
            }
            _ => None,
        };

        let (
            context_system_bytes,
            context_tools_bytes,
            context_history_bytes,
            context_last_turn_bytes,
            context_other_bytes,
            context_measured_bytes,
            context_messages_count,
            context_tax_ratio,
        ) = flatten_context_breakdown(self.base.context.as_ref());

        // Atribución de la caché por sección. Se calcula AQUÍ y no en
        // `Provider::prepare` porque necesita las dos mitades a la vez: los
        // cubos de bytes (que solo existen en `prepare`) y los tokens de caché
        // (que no existen hasta que el proveedor responde). Este es el único
        // punto del recorrido donde ambas coinciden, y ya está fuera del
        // camino crítico: la respuesta se cerró antes de entrar en `emit`.
        let cache_by_section = cache_attribution::attribute_cache(
            &self.base.upstream,
            self.base.context.as_ref(),
            self.scanner.usage.input_tokens,
            self.scanner.usage.cache_read_tokens,
            self.scanner.usage.cache_write_tokens,
        );

        // El reparto se apoya en la atribución de caché recién calculada: sin
        // ella daría el reparto por bytes, que es justo el que la medición
        // desmiente. Por eso va después y toma su resultado, no el body.
        let input_share_by_section = section_share::attribute_share(
            self.base.model.as_deref(),
            self.base.context.as_ref(),
            cache_by_section.as_ref(),
        );

        let (tools_by_server, tools_overhead_bytes) = tools_fields(
            self.base.context.as_ref(),
            self.base.tools_by_server.clone(),
            self.base.tools_overhead_bytes,
        );

        self.sink.record(RequestMetric {
            timestamp: self.base.timestamp.clone(),
            route: self.base.route.clone(),
            upstream: self.base.upstream.clone(),
            model: self.base.model.clone(),
            prompt_hash: self.base.prompt_hash.clone(),
            stream: self.base.stream,
            client: self.base.client.clone(),
            session: self.base.session.clone(),
            prompt_bytes: self.base.prompt_bytes,
            input_tokens: self.scanner.usage.input_tokens,
            output_tokens: self.scanner.usage.output_tokens,
            cache_read_tokens: self.scanner.usage.cache_read_tokens,
            cache_write_tokens: self.scanner.usage.cache_write_tokens,
            cost_estimate_usd,
            cache_control_forced: self.base.cache_control_forced,
            status: self.base.status,
            ttft_ms: self.ttft_ms,
            total_ms,
            tokens_per_sec,
            context_system_bytes,
            context_tools_bytes,
            context_history_bytes,
            context_last_turn_bytes,
            context_other_bytes,
            context_measured_bytes,
            context_messages_count,
            context_tax_ratio,
            cache_by_section,
            input_share_by_section,
            tools_by_server,
            tools_overhead_bytes,
            prepare_us: self.base.prepare_us,
            requested_effort: self.base.requested_effort.clone(),
            requested_speed: self.base.requested_speed.clone(),
            served_speed: self.scanner.usage.speed.clone(),
            tool_search: self.base.tool_search.clone(),
            tools_flattened: self.base.tools_flattened,
            skills: self.base.skills,
            instructions: self.base.instructions,
            effort_forced: self.base.effort_forced.clone(),
            // `None` si este proveedor no tiene extractor: publicar listas
            // vacias ahi seria afirmar "no invoco nada", que es otra cosa.
            tool_calls: self
                .base
                .provider
                .captura_invocaciones()
                .then(|| std::mem::take(&mut self.scanner.calls)),
            response_bytes: Some(self.response_bytes),
            codex_quota: self.base.codex_quota.clone(),
        });
    }
}

impl Stream for MeteredBody {
    type Item = reqwest::Result<Bytes>;

    /// Reenvía el próximo chunk intacto y va midiendo; emite la métrica al
    /// llegar el fin del stream o un error del proveedor.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                if this.ttft_ms.is_none() {
                    this.ttft_ms = Some(this.start.elapsed().as_secs_f64() * 1000.0);
                }
                this.response_bytes += bytes.len();
                this.scanner.feed(&bytes);
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.emit();
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.emit();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for MeteredBody {
    /// Red de seguridad: si el cliente se desconecta antes del fin del stream,
    /// emitimos igual con lo medido hasta ese punto (no perdemos el request).
    fn drop(&mut self) {
        self.emit();
    }
}

#[cfg(test)]
mod tests {
    use super::{MeteredBody, MetricBase, UsageScanner};
    use crate::provider::{ANTHROPIC, ToolCalls};
    use crate::telemetry::{SessionAttribution, SessionSource, TelemetrySink};
    use bytes::Bytes;
    use futures_util::StreamExt;
    use std::time::Instant;

    /// Solo los nombres, para no acoplar estos tests (que prueban el ESCÁNER)
    /// a la forma de la atribución a servidor, que se prueba aparte.
    fn nombres_de(calls: &ToolCalls) -> Vec<String> {
        calls.invoked.iter().map(|c| c.name.clone()).collect()
    }

    /// `MetricBase` mínima para ejercitar el recorrido del stream.
    fn base_de_prueba(stream: bool) -> MetricBase {
        MetricBase {
            timestamp: "2026-07-25T00:00:00Z".to_string(),
            route: "/v1/messages".to_string(),
            upstream: "anthropic".to_string(),
            model: None,
            prompt_hash: "h".to_string(),
            stream,
            client: None,
            session: SessionAttribution {
                source: SessionSource::Unattributed,
                key: "k".to_string(),
            },
            prompt_bytes: 0,
            status: 200,
            cache_control_forced: false,
            context: None,
            tools_by_server: Vec::new(),
            tools_overhead_bytes: 0,
            prepare_us: 0,
            requested_effort: None,
            requested_speed: None,
            tool_search: None,
            tools_flattened: None,
            skills: None,
            instructions: None,
            effort_forced: None,
            provider: &ANTHROPIC,
            codex_quota: None,
        }
    }

    /// Los bytes de bajada se acumulan a lo largo de TODOS los chunks, no solo
    /// del primero ni del último. Un contador que se reasigne en vez de sumar
    /// daría el tamaño del último chunk y parecería plausible.
    #[tokio::test]
    async fn response_bytes_suma_todos_los_chunks() {
        let dir = std::env::temp_dir().join(format!("oxi-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir de prueba");
        let sink = TelemetrySink::spawn(dir.clone());
        let recent = sink.recent();

        let chunks = vec![
            Ok(Bytes::from_static(b"12345")),
            Ok(Bytes::from_static(b"678")),
            Ok(Bytes::from_static(b"90")),
        ];
        let mut body = MeteredBody::new(
            futures_util::stream::iter(chunks),
            sink,
            base_de_prueba(true),
            Instant::now(),
        );
        while body.next().await.is_some() {}
        drop(body);

        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let filas = recent
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .snapshot();
        let fila = filas.last().expect("debe haber una fila");

        assert_eq!(
            fila.response_bytes,
            Some(10),
            "5 + 3 + 2 = 10 bytes bajados"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// El caso feo: un evento SSE partido entre dos chunks. El buffer de
    /// línea debe recomponerlo antes de parsear, delegando en el proveedor
    /// la extracción del `usage` ya reconstituido. Esto ejercita la mecánica
    /// pura del escáner (split de líneas), no la forma de ningún proveedor.
    #[test]
    fn reassembles_event_split_across_chunks() {
        let mut scanner = UsageScanner::new(true, &ANTHROPIC);
        scanner.feed(b"data: {\"type\":\"message_delta\",\"usa");
        scanner.feed(b"ge\":{\"output_tokens\":7}}\n\n");
        scanner.finish();

        assert_eq!(scanner.usage.output_tokens, Some(7));
    }

    /// El escáner recoge la invocación del MISMO evento SSE del que ya sacó
    /// el `usage`, sin recorrer el stream dos veces — y lo hace sobre un
    /// evento partido entre chunks, para probar que hereda el buffer de
    /// línea existente en vez de traerse el suyo.
    #[test]
    fn el_escaner_recoge_invocaciones_del_mismo_stream() {
        let mut scanner = UsageScanner::new(true, &ANTHROPIC);

        scanner.feed(b"data: {\"type\":\"content_block_start\",\"index\":1,\"content_bl");
        scanner.feed(b"ock\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"mcp__context7__get-docs\"}}\n\n");
        scanner.feed(b"data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":");
        scanner.feed(
            b"{\"type\":\"server_tool_use\",\"id\":\"srvtoolu_1\",\"name\":\"web_search\"}}\n\n",
        );
        scanner.feed(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\n");
        scanner.finish();

        assert_eq!(
            nombres_de(&scanner.calls),
            vec!["mcp__context7__get-docs".to_string()],
            "la de cliente, reensamblada desde dos chunks"
        );
        assert_eq!(
            scanner.calls.server_invoked,
            vec!["web_search".to_string()],
            "la de servidor, en su lista"
        );
        assert_eq!(
            scanner.usage.output_tokens,
            Some(42),
            "y el usage sigue saliendo del mismo recorrido"
        );
    }

    /// El modo no-streaming pasa por `finish()`, no por `scan_sse_line`: el
    /// cuerpo entero se parsea una vez al cerrar. Sin este test, una
    /// implementación que solo cubriera SSE dejaría el campo vacío en todas
    /// las respuestas no-streaming sin que ningún test lo notara.
    #[test]
    fn sin_streaming_las_invocaciones_salen_del_cuerpo_completo() {
        let mut scanner = UsageScanner::new(false, &ANTHROPIC);

        scanner.feed(br#"{"id":"msg_1","content":[{"type":"tool_use","id":"t1","name":"Read"}],"#);
        scanner.feed(br#""usage":{"output_tokens":7}}"#);
        scanner.finish();

        assert_eq!(nombres_de(&scanner.calls), vec!["Read".to_string()]);
        assert_eq!(scanner.usage.output_tokens, Some(7));
    }

    /// REGRESIÓN. Un upstream que corta la conexión justo tras el último
    /// `data: {...}`, sin el salto de línea final, dejaba ese evento entero
    /// en `line_buf` y se descartaba en silencio. Afectaba ya al `usage`;
    /// con las invocaciones además se perdía una llamada, así que `finish()`
    /// vacía el resto antes de cerrar.
    #[test]
    fn el_ultimo_evento_sin_salto_de_linea_no_se_pierde() {
        let mut scanner = UsageScanner::new(true, &ANTHROPIC);

        scanner.feed(b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Read\"}}\n\n");
        // Sin `\n` final: la conexión se corta aquí.
        scanner.feed(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}");
        scanner.finish();

        assert_eq!(
            scanner.usage.output_tokens,
            Some(9),
            "el usage del ultimo evento tambien se recupera"
        );
        assert_eq!(nombres_de(&scanner.calls), vec!["Read".to_string()]);
    }

    /// Un remanente que no es un `data:` válido no puede corromper nada:
    /// `scan_sse_line` lo ignora, así que drenar el buffer es seguro.
    #[test]
    fn un_remanente_a_medio_json_no_rompe_el_acumulado() {
        let mut scanner = UsageScanner::new(true, &ANTHROPIC);

        scanner.feed(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":4}}\n\n");
        scanner.feed(b"data: {\"type\":\"content_bl");
        scanner.finish();

        assert_eq!(scanner.usage.output_tokens, Some(4), "lo bueno sobrevive");
        assert!(scanner.calls.invoked.is_empty(), "lo roto se ignora");
    }
}
