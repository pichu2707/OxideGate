//! Envoltorio de medición sobre el stream de respuesta del proveedor.
//!
//! El passthrough original hacía `Body::from_stream(resp.bytes_stream())` y se
//! desentendía. Aquí interponemos [`MeteredBody`]: reenvía cada chunk SIN
//! modificarlo (no bufferiza, no rompe el SSE) pero de paso:
//!   1. marca el TTFT en el primer chunk,
//!   2. va escaneando los eventos SSE en busca del `usage` del proveedor,
//!   3. al cerrarse el stream calcula coste/velocidad y emite la métrica.
//!
//! La métrica se emite UNA sola vez, tanto si el stream termina limpio como si
//! el cliente se desconecta a media respuesta (vía `Drop`).
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
}

/// Acumulador incremental que extrae `input/output_tokens` del cuerpo de la
/// respuesta, sea SSE (streaming) o un único JSON (no-streaming).
struct UsageScanner {
    /// `true` si la respuesta es SSE; decide la estrategia de parseo.
    is_stream: bool,
    /// Buffer de línea parcial: un chunk puede partir un evento SSE por la mitad.
    line_buf: Vec<u8>,
    /// Cuerpo completo acumulado, solo en modo no-streaming (un JSON suelto).
    full_body: Vec<u8>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl UsageScanner {
    fn new(is_stream: bool) -> Self {
        Self {
            is_stream,
            line_buf: Vec::new(),
            full_body: Vec::new(),
            input_tokens: None,
            output_tokens: None,
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
            self.extract_usage(&value);
        }
    }

    /// Cierre del stream: en no-streaming el `usage` vive en el JSON completo.
    fn finish(&mut self) {
        if self.is_stream {
            return;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&self.full_body) {
            self.extract_usage(&value);
        }
    }

    /// Busca un objeto de `usage` en un valor JSON y actualiza los contadores.
    ///
    /// Cubre las tres formas de los proveedores:
    ///   - OpenAI / Anthropic `message_delta`: `usage` en la raíz.
    ///   - Anthropic `message_start`: `usage` anidado bajo `message`.
    ///   - Gemini: `usageMetadata` en la raíz, con otros nombres de campo.
    ///
    /// El output es acumulativo/final (Anthropic y Gemini), así que "último gana".
    fn extract_usage(&mut self, value: &Value) {
        let usage = value
            .get("usage")
            .or_else(|| value.get("message").and_then(|m| m.get("usage")))
            .or_else(|| value.get("usageMetadata"));
        let Some(usage) = usage else {
            return;
        };

        // Entrada: `input_tokens` (Anthropic), `prompt_tokens` (OpenAI) o
        // `promptTokenCount` (Gemini).
        if let Some(v) = usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))
            .or_else(|| usage.get("promptTokenCount"))
            .and_then(Value::as_u64)
        {
            self.input_tokens = Some(v);
        }

        // Salida: `output_tokens` (Anthropic), `completion_tokens` (OpenAI) o
        // `candidatesTokenCount` (Gemini). Nota: en Gemini los tokens de
        // "thinking" (`thoughtsTokenCount`) van aparte y aún no se suman aquí.
        if let Some(v) = usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens"))
            .or_else(|| usage.get("candidatesTokenCount"))
            .and_then(Value::as_u64)
        {
            self.output_tokens = Some(v);
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
        Self {
            inner: Box::pin(inner),
            sink,
            base,
            start,
            ttft_ms: None,
            scanner: UsageScanner::new(is_stream),
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
            self.scanner.input_tokens,
            self.scanner.output_tokens,
        );

        // Velocidad de generación = tokens de salida / tramo de generación
        // (total − TTFT). Solo tiene sentido en STREAMING: en una respuesta
        // no-streaming todo llega de golpe (ttft ≈ total) y el tramo tiende a
        // cero, disparando un número absurdo. Fuera de streaming la anulamos.
        let tokens_per_sec = match (self.base.stream, self.scanner.output_tokens, self.ttft_ms) {
            (true, Some(out), Some(ttft)) if total_ms > ttft => {
                Some(out as f64 / ((total_ms - ttft) / 1000.0))
            }
            _ => None,
        };

        self.sink.record(RequestMetric {
            timestamp: self.base.timestamp.clone(),
            route: self.base.route.clone(),
            upstream: self.base.upstream.clone(),
            model: self.base.model.clone(),
            prompt_hash: self.base.prompt_hash.clone(),
            stream: self.base.stream,
            prompt_bytes: self.base.prompt_bytes,
            input_tokens: self.scanner.input_tokens,
            output_tokens: self.scanner.output_tokens,
            cost_estimate_usd,
            status: self.base.status,
            ttft_ms: self.ttft_ms,
            total_ms,
            tokens_per_sec,
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

    /// Anthropic manda el input en `message_start` y el output acumulado en
    /// `message_delta`. El scanner debe quedarse con ambos.
    #[test]
    fn extracts_anthropic_usage_from_sse() {
        let mut scanner = UsageScanner::new(true);
        scanner.feed(b"event: message_start\n");
        scanner.feed(b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":1}}}\n\n");
        scanner.feed(b"event: message_delta\n");
        scanner.feed(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":99}}\n\n");
        scanner.finish();

        assert_eq!(scanner.input_tokens, Some(42));
        assert_eq!(scanner.output_tokens, Some(99));
    }

    /// OpenAI (con include_usage) manda el `usage` en el chunk final, con
    /// `prompt_tokens`/`completion_tokens` y `choices` vacío.
    #[test]
    fn extracts_openai_usage_from_sse() {
        let mut scanner = UsageScanner::new(true);
        scanner.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hola\"}}]}\n\n");
        scanner.feed(b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":20,\"total_tokens\":30}}\n\n");
        scanner.feed(b"data: [DONE]\n\n");
        scanner.finish();

        assert_eq!(scanner.input_tokens, Some(10));
        assert_eq!(scanner.output_tokens, Some(20));
    }

    /// El caso feo: un evento SSE partido entre dos chunks. El buffer de línea
    /// debe recomponerlo antes de parsear.
    #[test]
    fn reassembles_event_split_across_chunks() {
        let mut scanner = UsageScanner::new(true);
        scanner.feed(b"data: {\"type\":\"message_delta\",\"usa");
        scanner.feed(b"ge\":{\"output_tokens\":7}}\n\n");
        scanner.finish();

        assert_eq!(scanner.output_tokens, Some(7));
    }

    /// Gemini (`alt=sse`) manda `usageMetadata` con otros nombres de campo, y el
    /// conteo es acumulativo en el chunk final. El scanner debe mapearlo.
    #[test]
    fn extracts_gemini_usage_from_sse() {
        let mut scanner = UsageScanner::new(true);
        scanner.feed(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hola\"}]}}],\"usageMetadata\":{\"promptTokenCount\":11,\"candidatesTokenCount\":3,\"totalTokenCount\":14}}\n\n");
        scanner.finish();

        assert_eq!(scanner.input_tokens, Some(11));
        assert_eq!(scanner.output_tokens, Some(3));
    }

    /// Respuesta no-streaming: el `usage` vive en el JSON completo, que se parsea
    /// al cerrar el stream, no por líneas SSE.
    #[test]
    fn extracts_usage_from_non_stream_body() {
        let mut scanner = UsageScanner::new(false);
        scanner.feed(b"{\"model\":\"claude\",\"usage\":{\"input_tokens\":5,");
        scanner.feed(b"\"output_tokens\":8}}");
        scanner.finish();

        assert_eq!(scanner.input_tokens, Some(5));
        assert_eq!(scanner.output_tokens, Some(8));
    }
}
