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
use crate::provider::{ContextBreakdown, Provider, Usage};
use crate::telemetry::logger::flatten_context_breakdown;
use crate::telemetry::pricing;
use crate::telemetry::{RequestMetric, TelemetrySink};
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
    /// Microsegundos que `middleware::proxy::run` pasó dentro de
    /// `provider.prepare(...)`. Viaja intacto hasta `RequestMetric::prepare_us`.
    pub prepare_us: u64,
    /// Proveedor dueño del dialecto de esta respuesta: la extracción del
    /// `usage` se delega íntegramente en él, así este módulo no necesita
    /// saber nada de ningún proveedor concreto.
    pub provider: &'static dyn Provider,
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
}

impl UsageScanner {
    fn new(is_stream: bool, provider: &'static dyn Provider) -> Self {
        Self {
            is_stream,
            line_buf: Vec::new(),
            full_body: Vec::new(),
            provider,
            usage: Usage::default(),
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
        }
    }

    /// Cierre del stream: en no-streaming el `usage` vive en el JSON completo.
    fn finish(&mut self) {
        if self.is_stream {
            return;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&self.full_body) {
            self.provider.extract_usage(&value, &mut self.usage);
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

        self.sink.record(RequestMetric {
            timestamp: self.base.timestamp.clone(),
            route: self.base.route.clone(),
            upstream: self.base.upstream.clone(),
            model: self.base.model.clone(),
            prompt_hash: self.base.prompt_hash.clone(),
            stream: self.base.stream,
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
            prepare_us: self.base.prepare_us,
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
    use super::UsageScanner;
    use crate::provider::ANTHROPIC;

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
}
