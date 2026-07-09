//! Agregación EN VIVO de telemetría por `(proveedor, modelo)`.
//!
//! `RequestMetric` ya trae el modelo por fila (la captura no cambia acá); este
//! módulo solo ACUMULA lo que ya se mide para responder en `O(1)` por request
//! "¿qué modelo conviene optimizar ahora mismo?" sin recorrer el JSONL entero.
//! Es PURO: no conoce axum ni ningún framework HTTP, solo `RequestMetric`. El
//! handler que lo expone por HTTP vive en `middleware::stats`.
use crate::telemetry::logger::RequestMetric;
use serde::Serialize;
use std::collections::HashMap;

/// Tope de huellas de prompt distintas que se recuerdan POR MODELO.
///
/// La redundancia se mide contando ocurrencias de `prompt_hash`, pero ese
/// mapa podría crecer sin límite en un servidor de larga vida con tráfico
/// muy variado. Con el cap, una vez lleno dejamos de admitir huellas nuevas
/// (solo seguimos incrementando las ya vistas) y marcamos
/// `redundancy_saturated`. La métrica resultante es una COTA INFERIOR
/// honesta de la redundancia real, no un número inflado ni un OOM.
const MAX_DISTINCT_PROMPTS_PER_MODEL: usize = 50_000;

/// Acumulador incremental de un `(upstream, model)`.
///
/// Cada campo se actualiza en `O(1)` por request salvo el mapa de huellas
/// (`prompt_counts`), que es la única estructura que crece con el tráfico
/// (acotada por [`MAX_DISTINCT_PROMPTS_PER_MODEL`]). Todo lo demás son sumas,
/// contadores y min/max: no se recalcula nada desde cero en cada `ingest`.
#[derive(Debug, Default)]
struct ModelAccumulator {
    requests: u64,

    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    cost_usd: f64,

    ttft_ms_sum: f64,
    ttft_ms_count: u64,
    ttft_ms_min: Option<f64>,
    ttft_ms_max: Option<f64>,

    total_ms_sum: f64,

    tokens_per_sec_sum: f64,
    tokens_per_sec_count: u64,

    cache_forced: u64,
    errors: u64,

    /// Huella (`prompt_hash` como `u64`) → nº de ocurrencias vistas.
    prompt_counts: HashMap<u64, u32>,
    /// `true` en cuanto el cap de huellas distintas se alcanzó una vez.
    redundancy_saturated: bool,
}

impl ModelAccumulator {
    /// Incorpora una métrica ya perteneciente a este `(upstream, model)`.
    fn ingest(&mut self, m: &RequestMetric) {
        self.requests += 1;

        if let Some(v) = m.input_tokens {
            self.input_tokens += v;
        }
        if let Some(v) = m.output_tokens {
            self.output_tokens += v;
        }
        if let Some(v) = m.cache_read_tokens {
            self.cache_read_tokens += v;
        }
        if let Some(v) = m.cache_write_tokens {
            self.cache_write_tokens += v;
        }
        if let Some(v) = m.cost_estimate_usd {
            self.cost_usd += v;
        }

        if let Some(v) = m.ttft_ms {
            self.ttft_ms_sum += v;
            self.ttft_ms_count += 1;
            self.ttft_ms_min = Some(self.ttft_ms_min.map_or(v, |min| min.min(v)));
            self.ttft_ms_max = Some(self.ttft_ms_max.map_or(v, |max| max.max(v)));
        }

        self.total_ms_sum += m.total_ms;

        if let Some(v) = m.tokens_per_sec {
            self.tokens_per_sec_sum += v;
            self.tokens_per_sec_count += 1;
        }

        if m.cache_control_forced {
            self.cache_forced += 1;
        }
        if m.status >= 400 {
            self.errors += 1;
        }

        self.ingest_prompt_hash(&m.prompt_hash);
    }

    /// Registra la huella para el cálculo de redundancia, respetando el cap.
    ///
    /// La huella viaja como hex de 64 bits en `RequestMetric::prompt_hash`;
    /// si no parsea (formato inesperado) la ignoramos para redundancia sin
    /// afectar el resto de la métrica.
    fn ingest_prompt_hash(&mut self, prompt_hash: &str) {
        let Ok(hash) = u64::from_str_radix(prompt_hash, 16) else {
            return;
        };

        if let Some(count) = self.prompt_counts.get_mut(&hash) {
            *count += 1;
            return;
        }

        if self.prompt_counts.len() >= MAX_DISTINCT_PROMPTS_PER_MODEL {
            // Cap alcanzado: no admitimos huellas nuevas, pero seguimos
            // contando las ya presentes (rama de arriba). La redundancia
            // reportada queda como cota inferior honesta.
            self.redundancy_saturated = true;
            return;
        }

        self.prompt_counts.insert(hash, 1);
    }

    /// Deriva la fila serializable para el snapshot de este `(upstream, model)`.
    fn to_row(&self, upstream: String, model: String) -> ModelStatsRow {
        let requests_f = self.requests as f64;

        let cache_denominator =
            (self.input_tokens + self.cache_read_tokens + self.cache_write_tokens) as f64;
        let cache_hit_rate = if cache_denominator > 0.0 {
            self.cache_read_tokens as f64 / cache_denominator
        } else {
            0.0
        };

        let distinct_prompts = self.prompt_counts.len() as u64;
        let redundant_requests: u64 = self
            .prompt_counts
            .values()
            .map(|&count| (count as u64).saturating_sub(1))
            .sum();

        ModelStatsRow {
            upstream,
            model,
            requests: self.requests,

            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            avg_input_tokens: safe_div(self.input_tokens as f64, requests_f),
            avg_output_tokens: safe_div(self.output_tokens as f64, requests_f),

            cache_hit_rate,
            cache_forced_rate: safe_div(self.cache_forced as f64, requests_f),

            cost_usd: self.cost_usd,
            avg_cost_usd: safe_div(self.cost_usd, requests_f),

            avg_ttft_ms: safe_div(self.ttft_ms_sum, self.ttft_ms_count as f64),
            min_ttft_ms: self.ttft_ms_min,
            max_ttft_ms: self.ttft_ms_max,
            ttft_ms_sum: self.ttft_ms_sum,
            ttft_ms_count: self.ttft_ms_count,
            avg_total_ms: safe_div(self.total_ms_sum, requests_f),
            total_ms_sum: self.total_ms_sum,
            avg_tokens_per_sec: safe_div(self.tokens_per_sec_sum, self.tokens_per_sec_count as f64),
            tokens_per_sec_sum: self.tokens_per_sec_sum,
            tokens_per_sec_count: self.tokens_per_sec_count,

            error_rate: safe_div(self.errors as f64, requests_f),
            errors: self.errors,

            distinct_prompts,
            redundant_requests,
            redundancy_rate: safe_div(redundant_requests as f64, requests_f),
            redundancy_saturated: self.redundancy_saturated,
        }
    }
}

/// División protegida contra denominador cero: evita `NaN`/`inf` en las tasas
/// y promedios cuando el contador correspondiente (requests, ttft_count, …)
/// todavía es cero.
fn safe_div(numerator: f64, denominator: f64) -> f64 {
    if denominator > 0.0 {
        numerator / denominator
    } else {
        0.0
    }
}

/// Fila derivada de un `(upstream, model)`, lista para serializar a JSON.
///
/// Solo expone agregados y conteos: ninguna huella individual ni contenido
/// de prompt sale de acá, así que el endpoint que la sirve no filtra datos
/// sensibles aunque no tenga autenticación.
#[derive(Debug, Clone, Serialize)]
pub struct ModelStatsRow {
    pub upstream: String,
    pub model: String,
    pub requests: u64,

    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub avg_input_tokens: f64,
    pub avg_output_tokens: f64,

    /// Fracción de tokens de contexto servidos desde caché. Bajo ⇒ candidato
    /// a forzar `cache_control` (palanca A).
    pub cache_hit_rate: f64,
    /// Fracción de requests en que OxideGate ya forzó `cache_control`.
    pub cache_forced_rate: f64,

    pub cost_usd: f64,
    pub avg_cost_usd: f64,

    pub avg_ttft_ms: f64,
    pub min_ttft_ms: Option<f64>,
    pub max_ttft_ms: Option<f64>,
    /// Suma cruda de `ttft_ms` acumulada (no promedio). Expuesta para que un
    /// cliente externo (p. ej. el monitor TUI) pueda derivar el `avg_ttft_ms`
    /// de una VENTANA de tiempo (Δsuma / Δcount) sin tener que reconstruirla
    /// a partir de dos promedios, lo cual sería matemáticamente incorrecto.
    pub ttft_ms_sum: f64,
    /// Cantidad de requests que aportaron a `ttft_ms_sum` (puede ser menor a
    /// `requests`: no todas las respuestas exponen TTFT, p. ej. no-streaming).
    pub ttft_ms_count: u64,
    pub avg_total_ms: f64,
    /// Suma cruda de `total_ms` acumulada. El count coincide con `requests`
    /// (todo request mide `total_ms`), así que no hace falta un campo aparte.
    pub total_ms_sum: f64,
    pub avg_tokens_per_sec: f64,
    /// Suma cruda de `tokens_per_sec` acumulada (no promedio).
    pub tokens_per_sec_sum: f64,
    /// Cantidad de requests que aportaron a `tokens_per_sec_sum`.
    pub tokens_per_sec_count: u64,

    pub error_rate: f64,
    /// Cantidad cruda de requests con `status >= 400` (numerador de
    /// `error_rate`, expuesto para poder derivar la tasa de una ventana).
    pub errors: u64,

    /// Huellas de prompt distintas vistas (acotado por el cap de memoria).
    pub distinct_prompts: u64,
    /// Requests cuya huella ya se había visto antes: candidatos a
    /// deduplicación o a la palanca B del optimizador.
    pub redundant_requests: u64,
    pub redundancy_rate: f64,
    /// `true` si se alcanzó el cap de huellas distintas para este modelo: la
    /// redundancia reportada es una cota inferior, no el valor exacto.
    pub redundancy_saturated: bool,
}

/// Snapshot completo: una fila por `(upstream, model)`, ordenado por
/// `requests` descendente (los modelos con más tráfico primero).
///
/// `serde(transparent)` hace que serialice directamente como array JSON, sin
/// envoltorio, para que el endpoint devuelva un JSON limpio.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct StatsSnapshot(pub Vec<ModelStatsRow>);

/// Registro en memoria de la agregación por `(upstream, model)`.
///
/// Vive detrás de un `Arc<RwLock<_>>` compartido entre la task de drenaje
/// (que llama `ingest`) y el handler de `/stats` (que llama `snapshot`). Este
/// tipo en sí mismo no sabe nada de locks ni de axum: esa coordinación es
/// responsabilidad de quien lo posea.
#[derive(Debug, Default)]
pub struct StatsRegistry {
    accumulators: HashMap<(String, String), ModelAccumulator>,
}

impl StatsRegistry {
    /// Incorpora una métrica al acumulador de su `(upstream, model)`.
    ///
    /// `model: None` se agrupa bajo la clave `"unknown"` para no perder la
    /// fila (algunos requests fallan antes de conocer el modelo, pero siguen
    /// aportando señal de error/latencia al proveedor).
    pub fn ingest(&mut self, m: &RequestMetric) {
        let model = m.model.clone().unwrap_or_else(|| "unknown".to_string());
        let key = (m.upstream.clone(), model);
        self.accumulators.entry(key).or_default().ingest(m);
    }

    /// Construye la vista serializable del estado actual, ordenada por
    /// tráfico (`requests` desc) para que lo más relevante quede primero.
    pub fn snapshot(&self) -> StatsSnapshot {
        let mut rows: Vec<ModelStatsRow> = self
            .accumulators
            .iter()
            .map(|((upstream, model), acc)| acc.to_row(upstream.clone(), model.clone()))
            .collect();

        rows.sort_by_key(|row| std::cmp::Reverse(row.requests));

        StatsSnapshot(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construye una métrica mínima con los campos por defecto, para no
    /// repetir el struct literal completo en cada test.
    fn base_metric(upstream: &str, model: &str) -> RequestMetric {
        RequestMetric {
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            route: "/v1/messages".to_string(),
            upstream: upstream.to_string(),
            model: Some(model.to_string()),
            prompt_hash: "0000000000000001".to_string(),
            stream: false,
            prompt_bytes: 100,
            input_tokens: Some(10),
            output_tokens: Some(5),
            cache_read_tokens: None,
            cache_write_tokens: None,
            cost_estimate_usd: Some(0.01),
            cache_control_forced: false,
            status: 200,
            ttft_ms: Some(50.0),
            total_ms: 100.0,
            tokens_per_sec: Some(20.0),
            context_system_bytes: None,
            context_tools_bytes: None,
            context_history_bytes: None,
            context_last_turn_bytes: None,
            context_other_bytes: None,
            context_measured_bytes: None,
            context_messages_count: None,
            context_tax_ratio: None,
            prepare_us: 0,
        }
    }

    #[test]
    fn ingest_acumula_varias_metricas_del_mismo_modelo() {
        let mut registry = StatsRegistry::default();
        for _ in 0..3 {
            let mut m = base_metric("anthropic", "claude-opus-4");
            m.prompt_hash = format!("{:016x}", registry_next_hash());
            registry.ingest(&m);
        }

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.0.len(), 1);
        let row = &snapshot.0[0];
        assert_eq!(row.requests, 3);
        assert_eq!(row.input_tokens, 30);
        assert_eq!(row.output_tokens, 15);
        assert_eq!(row.avg_input_tokens, 10.0);
        assert_eq!(row.avg_output_tokens, 5.0);
        assert!((row.avg_cost_usd - 0.01).abs() < 1e-9);
        assert!((row.avg_ttft_ms - 50.0).abs() < 1e-9);
        // Sumas/counts crudas: deben coincidir con lo que promedian arriba.
        assert!((row.ttft_ms_sum - 150.0).abs() < 1e-9);
        assert_eq!(row.ttft_ms_count, 3);
        assert!((row.total_ms_sum - 300.0).abs() < 1e-9);
        assert!((row.tokens_per_sec_sum - 60.0).abs() < 1e-9);
        assert_eq!(row.tokens_per_sec_count, 3);
        assert_eq!(row.errors, 0);
    }

    /// Contador auxiliar para generar huellas distintas entre llamadas dentro
    /// de un mismo test, sin depender de estado global real.
    fn registry_next_hash() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn dos_modelos_distintos_generan_dos_filas() {
        let mut registry = StatsRegistry::default();
        registry.ingest(&base_metric("anthropic", "claude-opus-4"));
        registry.ingest(&base_metric("anthropic", "claude-haiku-4"));

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.0.len(), 2);
        let models: Vec<&str> = snapshot.0.iter().map(|r| r.model.as_str()).collect();
        assert!(models.contains(&"claude-opus-4"));
        assert!(models.contains(&"claude-haiku-4"));
    }

    #[test]
    fn cache_hit_rate_y_cache_forced_rate_se_calculan_bien() {
        let mut registry = StatsRegistry::default();

        let mut m1 = base_metric("anthropic", "claude-opus-4");
        m1.input_tokens = Some(10);
        m1.cache_read_tokens = Some(30);
        m1.cache_write_tokens = Some(0);
        m1.cache_control_forced = true;
        registry.ingest(&m1);

        let mut m2 = base_metric("anthropic", "claude-opus-4");
        m2.input_tokens = Some(10);
        m2.cache_read_tokens = Some(0);
        m2.cache_write_tokens = Some(0);
        m2.cache_control_forced = false;
        registry.ingest(&m2);

        let snapshot = registry.snapshot();
        let row = &snapshot.0[0];
        // cache_read total = 30, denominador = (10+10) + 30 + 0 = 50
        assert!((row.cache_hit_rate - 0.6).abs() < 1e-9);
        // 1 de 2 requests forzó cache_control
        assert!((row.cache_forced_rate - 0.5).abs() < 1e-9);
    }

    #[test]
    fn redundancia_cuenta_ocurrencias_de_la_misma_huella() {
        let mut registry = StatsRegistry::default();
        for _ in 0..3 {
            registry.ingest(&base_metric("anthropic", "claude-opus-4"));
        }

        let snapshot = registry.snapshot();
        let row = &snapshot.0[0];
        assert_eq!(row.distinct_prompts, 1);
        assert_eq!(row.redundant_requests, 2);
        assert!((row.redundancy_rate - (2.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn ttft_siempre_none_no_panica_y_promedia_cero() {
        let mut registry = StatsRegistry::default();
        let mut m = base_metric("anthropic", "claude-opus-4");
        m.ttft_ms = None;
        m.tokens_per_sec = None;
        registry.ingest(&m);

        let snapshot = registry.snapshot();
        let row = &snapshot.0[0];
        assert_eq!(row.avg_ttft_ms, 0.0);
        assert_eq!(row.min_ttft_ms, None);
        assert_eq!(row.max_ttft_ms, None);
        assert_eq!(row.avg_tokens_per_sec, 0.0);
    }

    #[test]
    fn modelo_none_se_agrupa_bajo_unknown() {
        let mut registry = StatsRegistry::default();
        let mut m = base_metric("anthropic", "claude-opus-4");
        m.model = None;
        registry.ingest(&m);

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.0[0].model, "unknown");
    }
}
