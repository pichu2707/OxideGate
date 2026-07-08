//! Escritor de telemetría fuera del camino crítico.
//!
//! El handler solo hace `sink.record(...)` (un `send` a un canal, no bloquea).
//! Una task en background serializa a JSONL y escribe a disco. Así el I/O de
//! log NUNCA se suma a la latencia que le devolvemos a gentle-ai.
use serde::Serialize;
use std::path::PathBuf;
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
}

/// Handle clonable que los handlers usan para emitir métricas sin bloquear.
#[derive(Clone)]
pub struct TelemetrySink {
    tx: mpsc::UnboundedSender<RequestMetric>,
}

impl TelemetrySink {
    /// Arranca la task escritora y devuelve el handle para emitir métricas.
    pub fn spawn(storage_dir: PathBuf) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<RequestMetric>();

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
                if let Ok(mut line) = serde_json::to_string(&metric) {
                    line.push('\n');
                    if let Err(e) = file.write_all(line.as_bytes()).await {
                        eprintln!("⚠️  telemetría: fallo al escribir: {e}");
                    }
                }
            }
        });

        Self { tx }
    }

    /// No bloquea: si el canal se cerró, descartamos la métrica en silencio.
    pub fn record(&self, metric: RequestMetric) {
        let _ = self.tx.send(metric);
    }
}
