//! Escritor de telemetría fuera del camino crítico.
//!
//! El handler solo hace `sink.record(...)` (un `send` a un canal, no bloquea).
//! Una task en background serializa a JSONL y escribe a disco, y de paso
//! alimenta el [`StatsRegistry`](crate::telemetry::stats::StatsRegistry)
//! compartido para que `/stats` pueda leer la agregación en vivo sin tocar el
//! JSONL. Así el I/O de log NUNCA se suma a la latencia que le devolvemos a
//! gentle-ai.
use crate::telemetry::StatsRegistry;
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
}

/// Handle clonable que los handlers usan para emitir métricas sin bloquear.
#[derive(Clone)]
pub struct TelemetrySink {
    tx: mpsc::UnboundedSender<RequestMetric>,
    /// Agregación en vivo por `(upstream, model)`, alimentada por la misma
    /// task de drenaje que escribe el JSONL. Se comparte con el handler de
    /// `/stats` vía `stats()`.
    stats: Arc<RwLock<StatsRegistry>>,
}

impl TelemetrySink {
    /// Arranca la task escritora y devuelve el handle para emitir métricas.
    pub fn spawn(storage_dir: PathBuf) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<RequestMetric>();
        let stats = Arc::new(RwLock::new(StatsRegistry::default()));
        let stats_writer = Arc::clone(&stats);

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

                if let Ok(mut line) = serde_json::to_string(&metric) {
                    line.push('\n');
                    if let Err(e) = file.write_all(line.as_bytes()).await {
                        eprintln!("⚠️  telemetría: fallo al escribir: {e}");
                    }
                }
            }
        });

        Self { tx, stats }
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
}
