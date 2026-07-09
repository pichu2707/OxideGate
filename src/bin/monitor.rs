//! Monitor TUI: cliente de terminal en vivo para `GET /stats`.
//!
//! Es un binario INDEPENDIENTE (como `bench.rs`): no comparte módulos con
//! `main.rs` porque el crate no tiene `lib.rs`, así que define sus propios
//! structs de deserialización para la fila de `/stats`. No lee el
//! `telemetry.jsonl` ni conoce nada del proxy más allá del contrato HTTP de
//! `GET /stats` — un cliente desacoplado, reemplazable sin tocar el proxy.
//!
//! El objetivo es ver el efecto de una optimización (p. ej. forzar
//! `cache_control`) EN VIVO: marcás un baseline con `b` antes de prender la
//! palanca, y el panel ANTES/DESPUÉS muestra el delta de la ventana desde
//! ese momento — tokens/seg, TTFT y cache-hit "limpios", sin arrastrar el
//! promedio histórico completo.
//!
//! Uso:
//!   cargo run --bin monitor              # TUI interactiva
//!   cargo run --bin monitor -- --once    # snapshot de texto plano y sale
//!   cargo run --bin monitor -- --url http://127.0.0.1:8080/stats
//!
//! URL del endpoint de agregados (en orden de prioridad):
//!   1. flag `--url <url>`
//!   2. env `OXIDEGATE_STATS_URL`
//!   3. `http://127.0.0.1:{OXIDEGATE_PORT}/stats` (puerto default 8080, el
//!      mismo que usa el proxy en `config.rs`: así, corriendo ambos con la
//!      misma `OXIDEGATE_PORT` —o ninguna—, el monitor apunta al proxy sin
//!      configuración extra).
//!
//! URL de `/requests` (detalle por petición individual, ver
//! [`resolve_requests_url`] para la precedencia completa): se deriva de la
//! URL de `/stats` ya resuelta, salvo que `OXIDEGATE_REQUESTS_URL` la
//! sobreescriba explícitamente.
//!
//! Teclas en la TUI interactiva:
//!   q / Esc   salir
//!   b         marcar baseline (ventana ANTES/DESPUÉS)
//!   r         resetear baseline
//!   ↑ / ↓     elegir modelo en la tabla de agregados
//!   p         mostrar/ocultar el panel de requests recientes (outliers)
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table};
use ratatui::{Frame, Terminal};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

/// Intervalo entre polls a `/stats`.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Cuántas muestras se recuerdan por modelo para los sparklines (~2 min a 1
/// muestra/seg). Acotado para no crecer sin límite en una sesión larga.
const HISTORY_CAP: usize = 120;

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let once = args.iter().any(|a| a == "--once");
    let url = resolve_url(&args);
    let requests_url = resolve_requests_url(&url);

    if once {
        run_once(&url, &requests_url);
        return Ok(());
    }

    install_panic_hook();
    let mut terminal = setup_terminal()?;
    let result = run_app(&mut terminal, &url, &requests_url);
    // SIEMPRE restauramos la terminal, sin importar cómo terminó el loop
    // (éxito, error de draw, error de evento): dejar la terminal en raw mode
    // o en pantalla alternada rompe la shell del usuario al salir.
    let restore_result = restore_terminal(&mut terminal);

    if let Err(e) = result {
        eprintln!("monitor: error en el loop: {e}");
    }
    restore_result
}

/// Resuelve la URL de `/stats` según la prioridad documentada en el header
/// del módulo: flag `--url`, luego `OXIDEGATE_STATS_URL`, luego
/// `OXIDEGATE_PORT` (default 8080, el mismo default que el proxy).
fn resolve_url(args: &[String]) -> String {
    if let Some(pos) = args.iter().position(|a| a == "--url")
        && let Some(url) = args.get(pos + 1)
    {
        return url.clone();
    }

    if let Ok(url) = std::env::var("OXIDEGATE_STATS_URL") {
        return url;
    }

    let port = std::env::var("OXIDEGATE_PORT").unwrap_or_else(|_| "8080".to_string());
    format!("http://127.0.0.1:{port}/stats")
}

/// Resuelve la URL de `/requests` a partir de la URL de `/stats` YA resuelta
/// (`stats_url`), con esta prioridad:
///   1. env `OXIDEGATE_REQUESTS_URL` (override explícito, ignora todo lo demás)
///   2. `stats_url` con el sufijo `/stats` reemplazado por `/requests` — así
///      ambos endpoints quedan apuntando al MISMO host/puerto sin que el
///      usuario tenga que repetir `--url` para cada uno.
///   3. si `stats_url` no termina en `/stats` (p. ej. vino de un `--url`
///      atípico), no hay forma segura de derivarla por sustitución: cae al
///      default `http://127.0.0.1:{OXIDEGATE_PORT|8080}/requests`, igual que
///      hace [`resolve_url`] para `/stats`.
///
/// Es un wrapper fino sobre [`resolve_requests_url_inner`] que solo se ocupa
/// de leer las dos variables de entorno; la lógica de precedencia en sí es
/// pura y testeable sin tocar el entorno del proceso (ver tests).
fn resolve_requests_url(stats_url: &str) -> String {
    let requests_url_env = std::env::var("OXIDEGATE_REQUESTS_URL").ok();
    let port_env = std::env::var("OXIDEGATE_PORT").ok();
    resolve_requests_url_inner(stats_url, requests_url_env, port_env)
}

/// Núcleo puro de [`resolve_requests_url`]: misma precedencia, pero recibe
/// los valores de entorno ya leídos como parámetros en vez de leerlos ella
/// misma. Separarla así permite testear las tres ramas de precedencia sin
/// mutar `std::env` (que es estado global del proceso y correría en carrera
/// con otros tests ejecutados en paralelo).
fn resolve_requests_url_inner(stats_url: &str, requests_url_env: Option<String>, port_env: Option<String>) -> String {
    if let Some(url) = requests_url_env {
        return url;
    }

    if let Some(prefix) = stats_url.strip_suffix("/stats") {
        return format!("{prefix}/requests");
    }

    let port = port_env.unwrap_or_else(|| "8080".to_string());
    format!("http://127.0.0.1:{port}/requests")
}

// ---------------------------------------------------------------------------
// Modo headless: --once
// ---------------------------------------------------------------------------

/// Hace UN fetch de `/stats` y de `/requests` y los imprime como tablas de
/// texto plano, sin raw mode. Sirve para verificación headless (CI, scripts)
/// y como fallback CLI cuando no hay terminal interactiva disponible. Nunca
/// panickea: si el proxy está caído o `/requests` no existe (build vieja del
/// proxy), imprime un aviso para esa parte y sigue con el resto, saliendo
/// limpio con código 0.
fn run_once(url: &str, requests_url: &str) {
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("monitor: no se pudo crear el cliente HTTP: {e}");
            return;
        }
    };

    match fetch_stats(&client, url) {
        Ok(rows) if rows.is_empty() => {
            println!("(sin filas todavía en {url} — el proxy está arriba pero sin tráfico)");
        }
        Ok(rows) => {
            println!("{:<10} {:<20} {:>6} {:>8} {:>9} {:>10} {:>8}", "PROVEEDOR", "MODELO", "REQ", "tok/s", "TTFT ms", "coste $", "err%");
            for r in &rows {
                println!(
                    "{:<10} {:<20} {:>6} {:>8.1} {:>9.1} {:>10.4} {:>7.1}%",
                    r.upstream,
                    r.model,
                    r.requests,
                    r.avg_tokens_per_sec,
                    r.avg_ttft_ms,
                    r.cost_usd,
                    r.error_rate * 100.0
                );
            }
        }
        Err(e) => {
            println!("proxy no disponible en {url} ({e})");
        }
    }

    println!();

    // `/requests` es un endpoint MÁS NUEVO que `/stats`: un proxy de build
    // anterior puede no tenerlo todavía. Si falla, avisamos y seguimos —
    // nunca es motivo para que `--once` termine con error.
    match fetch_requests(&client, requests_url) {
        Ok(rows) if rows.is_empty() => {
            println!("(sin requests individuales todavía en {requests_url})");
        }
        Ok(rows) => {
            print_requests_table(&rows);
        }
        Err(e) => {
            println!("/requests no disponible en {requests_url} ({e}) — puede ser una build del proxy anterior a este endpoint");
        }
    }
}

// ---------------------------------------------------------------------------
// Setup / teardown de terminal
// ---------------------------------------------------------------------------

/// Instala un hook de panic que restaura la terminal ANTES de propagar el
/// panic. Sin esto, un panic durante el loop dejaría la shell del usuario en
/// raw mode / pantalla alternada, ilegible hasta hacer `reset` a mano.
fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}

// ---------------------------------------------------------------------------
// Deserialización de /stats
// ---------------------------------------------------------------------------

/// Fila de `/stats`, deserializada solo con los campos que el monitor usa.
/// `serde` ignora el resto del JSON sin fallar (no hace falta espejar todo
/// `ModelStatsRow` de `src/telemetry/stats.rs`).
#[derive(Debug, Clone, Deserialize)]
struct StatsRow {
    upstream: String,
    model: String,
    requests: u64,

    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,

    cost_usd: f64,

    avg_ttft_ms: f64,
    avg_tokens_per_sec: f64,

    #[allow(dead_code)]
    cache_hit_rate: f64,
    redundancy_rate: f64,
    error_rate: f64,

    ttft_ms_sum: f64,
    ttft_ms_count: u64,
    #[allow(dead_code)]
    total_ms_sum: f64,
    errors: u64,
}

/// Clave lógica de una fila: `(upstream, model)`.
type ModelKey = (String, String);

fn key_of(r: &StatsRow) -> ModelKey {
    (r.upstream.clone(), r.model.clone())
}

/// Hace el GET a `/stats` y parsea el array de filas.
fn fetch_stats(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<StatsRow>, String> {
    let resp = client.get(url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("status {}", resp.status()));
    }
    resp.json::<Vec<StatsRow>>().map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Deserialización de /requests
// ---------------------------------------------------------------------------

/// Fila de `/requests`: espejo local y exacto de
/// [`RecentRequest`](../../src/telemetry/recent.rs), mismos nombres y tipos
/// de campo, igual que `StatsRow` espeja `ModelStatsRow`. Se define acá
/// (y no se reusa el struct del crate) porque `monitor` es un binario
/// independiente que solo conoce el contrato HTTP, no los tipos internos del
/// proxy.
#[derive(Debug, Clone, Deserialize)]
struct RequestRow {
    timestamp: String,
    #[allow(dead_code)]
    route: String,
    upstream: String,
    model: Option<String>,
    stream: bool,
    status: u16,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    cost_estimate_usd: Option<f64>,
    #[allow(dead_code)]
    cache_control_forced: bool,
    ttft_ms: Option<f64>,
    total_ms: f64,
}

/// Hace el GET a `/requests` y parsea el array de filas (orden cronológico,
/// más viejo primero — igual que lo entrega el buffer del proxy).
fn fetch_requests(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<RequestRow>, String> {
    let resp = client.get(url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("status {}", resp.status()));
    }
    resp.json::<Vec<RequestRow>>().map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Detección de outliers — función PURA, sin I/O ni tipos de ratatui
// ---------------------------------------------------------------------------

/// Cantidad mínima de filas en un grupo `(upstream, model)` para que tenga
/// sentido calcular media/desvío estándar. Con menos muestras, cualquier
/// desvío parece "atípico" y el desvío estándar en sí es poco significativo
/// (una sola fila distinta domina el cálculo). Por debajo de este umbral solo
/// se flaggea [`OutlierKind::Error`], que no necesita estadística alguna.
const MIN_GROUP_SAMPLE: usize = 5;

/// Cuántos desvíos estándar por encima/debajo de la media del grupo hacen
/// falta para considerar una fila atípica en TTFT o throughput de generación.
const OUTLIER_SIGMA: f64 = 2.0;

/// Clasificación de una petición respecto a la distribución de SU MISMO
/// modelo (agrupado por `(upstream, model)`). Una fila puede llevar más de
/// una etiqueta a la vez (p. ej. error Y TTFT lento), por eso
/// [`classify_outliers`] devuelve un `Vec<OutlierKind>` por fila en vez de
/// una única variante — colapsar a una sola escondería información real.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlierKind {
    /// `status >= 400`. Siempre se flaggea, sin importar el tamaño de la
    /// muestra: un error no necesita estadística para ser relevante.
    Error,
    /// Esta fila NO tuvo cache-hit (`cache_read_tokens` es `None` o `0`)
    /// mientras al menos la MITAD de las OTRAS filas del mismo grupo sí lo
    /// tuvieron. En una conversación larga el prefijo debería venir de
    /// caché; un miss aislado es una anomalía cara.
    CacheMiss,
    /// `ttft_ms` de esta fila está a >= [`OUTLIER_SIGMA`] desvíos estándar
    /// POR ENCIMA de la media de TTFT del grupo.
    SlowTtft,
    /// El throughput de generación de esta fila
    /// (`output_tokens / ((total_ms - ttft_ms) / 1000)`) está a >=
    /// [`OUTLIER_SIGMA`] desvíos estándar POR DEBAJO de la media del grupo.
    SlowGeneration,
}

impl OutlierKind {
    /// Marcador corto para la columna de la tabla. El texto en sí (no solo
    /// el color) tiene que llevar el significado, para que la señal no se
    /// pierda en terminales sin color o para usuarios daltónicos.
    fn marker(self) -> &'static str {
        match self {
            OutlierKind::Error => "ERR",
            OutlierKind::CacheMiss => "MISS",
            OutlierKind::SlowTtft => "TTFT",
            OutlierKind::SlowGeneration => "SLOW",
        }
    }
}

/// Media y desvío estándar POBLACIONAL de `values`. Devuelve `None` si
/// `values` está vacío o si el resultado no es finito (defensivo: no
/// debería pasar con valores ya filtrados por `is_finite`, pero una media de
/// una lista con `inf` mezclado igual podría colarse sin este guard).
fn mean_and_stddev(values: &[f64]) -> Option<(f64, f64)> {
    if values.is_empty() {
        return None;
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    let stddev = variance.sqrt();
    if !mean.is_finite() || !stddev.is_finite() {
        return None;
    }
    Some((mean, stddev))
}

/// Throughput de generación de una fila, en tokens/seg, o `None` si no es
/// calculable: sin `output_tokens`, sin `ttft_ms`, o con
/// `total_ms - ttft_ms <= 0` (no-streaming, donde TTFT ≈ total: la resta da
/// cero o negativo). Estas filas se EXCLUYEN del todo de la métrica, nunca
/// se tratan como "lentas".
fn generation_throughput(output_tokens: u64, total_ms: f64, ttft_ms: f64) -> Option<f64> {
    let gen_ms = total_ms - ttft_ms;
    if gen_ms <= 0.0 {
        return None;
    }
    let value = output_tokens as f64 / (gen_ms / 1000.0);
    if value.is_finite() {
        Some(value)
    } else {
        None
    }
}

/// `gen_ms` (tiempo de generación, `total_ms - ttft_ms`) de una fila, o
/// `None` si no hay `ttft_ms` o si el resultado no es positivo — mismo
/// criterio que [`generation_throughput`], para que la columna `gen_ms` de
/// la tabla y el cálculo de outliers sean consistentes entre sí.
fn gen_ms_of(r: &RequestRow) -> Option<f64> {
    let ttft = r.ttft_ms?;
    let gen_ms = r.total_ms - ttft;
    if gen_ms > 0.0 {
        Some(gen_ms)
    } else {
        None
    }
}

/// Clasifica cada fila de `rows` respecto a la distribución de su mismo
/// grupo `(upstream, model)`. Devuelve un `Vec<Vec<OutlierKind>>` en el
/// MISMO orden e índice que `rows` (no reordena ni filtra nada): el llamador
/// decide cómo presentar el resultado (p. ej. invertido, truncado).
///
/// Es una función PURA: no hace I/O, no conoce ratatui, no muta nada fuera
/// de su propio resultado. Eso es lo que la hace testeable sin terminal ni
/// HTTP de por medio.
fn classify_outliers(rows: &[RequestRow]) -> Vec<Vec<OutlierKind>> {
    let mut result: Vec<Vec<OutlierKind>> = vec![Vec::new(); rows.len()];
    if rows.is_empty() {
        return result;
    }

    // Agrupamos por (upstream, model): cada petición se compara solo contra
    // sus pares del mismo proveedor+modelo, nunca contra el resto.
    let mut groups: HashMap<(String, Option<String>), Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        groups.entry((r.upstream.clone(), r.model.clone())).or_default().push(i);
    }

    for indices in groups.values() {
        // Error no necesita estadística: se flaggea siempre, incluso en
        // grupos de una sola fila.
        for &i in indices {
            if rows[i].status >= 400 {
                result[i].push(OutlierKind::Error);
            }
        }

        // Con menos de MIN_GROUP_SAMPLE filas en el grupo, cualquier media o
        // desvío sería ruido estadístico: no flaggeamos nada más.
        if indices.len() < MIN_GROUP_SAMPLE {
            continue;
        }

        classify_slow_ttft(rows, indices, &mut result);
        classify_slow_generation(rows, indices, &mut result);
        classify_cache_miss(rows, indices, &mut result);
    }

    result
}

/// Sub-paso de [`classify_outliers`]: marca `SlowTtft` en las filas del
/// grupo cuyo `ttft_ms` esté a >= [`OUTLIER_SIGMA`] desvíos por encima de la
/// media. Filas sin `ttft_ms` se excluyen de la media Y no pueden flaggearse
/// (no hay dato con qué compararlas).
fn classify_slow_ttft(rows: &[RequestRow], indices: &[usize], result: &mut [Vec<OutlierKind>]) {
    let values: Vec<f64> = indices.iter().filter_map(|&i| rows[i].ttft_ms).filter(|v| v.is_finite()).collect();

    if values.len() < MIN_GROUP_SAMPLE {
        return;
    }
    let Some((mean, stddev)) = mean_and_stddev(&values) else {
        return;
    };
    // Desvío 0 (o no finito, ya descartado arriba): no hay variación real en
    // el grupo, flaggear cualquier cosa sería ruido, no señal.
    if stddev <= 0.0 {
        return;
    }

    let threshold = mean + OUTLIER_SIGMA * stddev;
    for &i in indices {
        if let Some(ttft) = rows[i].ttft_ms
            && ttft.is_finite()
            && ttft >= threshold
        {
            result[i].push(OutlierKind::SlowTtft);
        }
    }
}

/// Sub-paso de [`classify_outliers`]: marca `SlowGeneration` en las filas
/// del grupo cuyo throughput esté a >= [`OUTLIER_SIGMA`] desvíos por debajo
/// de la media. Filas sin throughput calculable (ver
/// [`generation_throughput`]) se excluyen de la media Y no pueden
/// flaggearse.
fn classify_slow_generation(rows: &[RequestRow], indices: &[usize], result: &mut [Vec<OutlierKind>]) {
    let samples: Vec<(usize, f64)> = indices
        .iter()
        .filter_map(|&i| {
            let r = &rows[i];
            let throughput = generation_throughput(r.output_tokens?, r.total_ms, r.ttft_ms?)?;
            Some((i, throughput))
        })
        .collect();

    if samples.len() < MIN_GROUP_SAMPLE {
        return;
    }
    let values: Vec<f64> = samples.iter().map(|(_, v)| *v).collect();
    let Some((mean, stddev)) = mean_and_stddev(&values) else {
        return;
    };
    if stddev <= 0.0 {
        return;
    }

    let threshold = mean - OUTLIER_SIGMA * stddev;
    for &(i, throughput) in &samples {
        if throughput <= threshold {
            result[i].push(OutlierKind::SlowGeneration);
        }
    }
}

/// Sub-paso de [`classify_outliers`]: marca `CacheMiss` en las filas sin
/// cache-hit cuando al menos la mitad de las OTRAS filas del grupo sí lo
/// tuvieron. El umbral se calcula por fila (excluyéndose a sí misma del
/// denominador), no una vez para todo el grupo, porque "las otras filas"
/// depende de cuál es la fila evaluada.
fn classify_cache_miss(rows: &[RequestRow], indices: &[usize], result: &mut [Vec<OutlierKind>]) {
    for &i in indices {
        let others: Vec<usize> = indices.iter().copied().filter(|&j| j != i).collect();
        if others.is_empty() {
            continue;
        }

        let hits = others.iter().filter(|&&j| rows[j].cache_read_tokens.is_some_and(|v| v > 0)).count();
        let hit_ratio = hits as f64 / others.len() as f64;
        let this_is_miss = rows[i].cache_read_tokens.is_none_or(|v| v == 0);

        if this_is_miss && hit_ratio >= 0.5 {
            result[i].push(OutlierKind::CacheMiss);
        }
    }
}

// ---------------------------------------------------------------------------
// Cálculo ANTES/DESPUÉS — funciones puras, testeables sin terminal ni HTTP
// ---------------------------------------------------------------------------

/// Throughput instantáneo de una ventana: tokens de salida generados dividido
/// el tiempo transcurrido. `0.0` si la ventana no tiene duración positiva
/// (defensivo: dos polls no deberían chocar en el mismo instante, pero un
/// reloj de sistema ajustado hacia atrás podría producirlo).
fn window_throughput(d_output_tokens: u64, elapsed_secs: f64) -> f64 {
    if elapsed_secs > 0.0 {
        d_output_tokens as f64 / elapsed_secs
    } else {
        0.0
    }
}

/// Cache-hit rate de una ventana: misma fórmula que el acumulador global
/// (`cache_read / (input + cache_read + cache_write)`), pero sobre los
/// deltas de la ventana en vez de los totales acumulados.
fn window_cache_hit_rate(d_input: u64, d_cache_read: u64, d_cache_write: u64) -> f64 {
    let denom = (d_input + d_cache_read + d_cache_write) as f64;
    if denom > 0.0 {
        d_cache_read as f64 / denom
    } else {
        0.0
    }
}

/// TTFT promedio de una ventana: `Δsuma / Δcount`. Promediar dos promedios ya
/// calculados (`avg_ttft` viejo y nuevo) sería matemáticamente incorrecto si
/// el count de requests con TTFT cambió entre polls; por eso el snapshot
/// expone las sumas/counts crudas y esta función opera sobre esos deltas.
fn window_avg_ttft(d_ttft_sum: f64, d_ttft_count: u64) -> f64 {
    if d_ttft_count > 0 {
        d_ttft_sum / d_ttft_count as f64
    } else {
        0.0
    }
}

/// Error rate de una ventana: `Δerrors / Δrequests`.
fn window_error_rate(d_errors: u64, d_requests: u64) -> f64 {
    if d_requests > 0 {
        d_errors as f64 / d_requests as f64
    } else {
        0.0
    }
}

/// Contadores crudos acumulados de un `(upstream, model)` en un instante
/// dado. Es el subconjunto de `StatsRow` necesario para calcular deltas de
/// ventana; no se guarda la fila completa para no arrastrar campos ya
/// derivados (promedios, tasas) que quedarían obsoletos entre polls.
#[derive(Debug, Clone, Copy, Default)]
struct RawCounters {
    requests: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    cost_usd: f64,
    ttft_ms_sum: f64,
    ttft_ms_count: u64,
    errors: u64,
}

impl RawCounters {
    fn from_row(r: &StatsRow) -> Self {
        Self {
            requests: r.requests,
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            cache_read_tokens: r.cache_read_tokens,
            cache_write_tokens: r.cache_write_tokens,
            cost_usd: r.cost_usd,
            ttft_ms_sum: r.ttft_ms_sum,
            ttft_ms_count: r.ttft_ms_count,
            errors: r.errors,
        }
    }
}

/// Delta derivado entre un baseline y el estado actual, ya con las tasas de
/// ventana calculadas. Es lo que pinta el panel ANTES/DESPUÉS.
#[derive(Debug, Clone, Copy, Default)]
struct WindowDelta {
    d_requests: u64,
    d_output_tokens: u64,
    d_cost_usd: f64,
    throughput: f64,
    avg_ttft: f64,
    cache_hit_rate: f64,
    error_rate: f64,
}

/// Resta `current - baseline` con `saturating_sub` en todos los enteros
/// (defensivo: los acumuladores del proxy solo crecen, pero un restart del
/// proxy entre el baseline y el poll actual podría hacerlos retroceder; en
/// ese caso el delta cae a 0 en vez de underflow-ear) y deriva las tasas de
/// la ventana con las funciones puras de arriba.
fn compute_window_delta(baseline: &RawCounters, current: &RawCounters, elapsed_secs: f64) -> WindowDelta {
    let d_requests = current.requests.saturating_sub(baseline.requests);
    let d_output_tokens = current.output_tokens.saturating_sub(baseline.output_tokens);
    let d_input_tokens = current.input_tokens.saturating_sub(baseline.input_tokens);
    let d_cache_read = current.cache_read_tokens.saturating_sub(baseline.cache_read_tokens);
    let d_cache_write = current.cache_write_tokens.saturating_sub(baseline.cache_write_tokens);
    let d_cost_usd = (current.cost_usd - baseline.cost_usd).max(0.0);
    let d_ttft_sum = (current.ttft_ms_sum - baseline.ttft_ms_sum).max(0.0);
    let d_ttft_count = current.ttft_ms_count.saturating_sub(baseline.ttft_ms_count);
    let d_errors = current.errors.saturating_sub(baseline.errors);

    WindowDelta {
        d_requests,
        d_output_tokens,
        d_cost_usd,
        throughput: window_throughput(d_output_tokens, elapsed_secs),
        avg_ttft: window_avg_ttft(d_ttft_sum, d_ttft_count),
        cache_hit_rate: window_cache_hit_rate(d_input_tokens, d_cache_read, d_cache_write),
        error_rate: window_error_rate(d_errors, d_requests),
    }
}

// ---------------------------------------------------------------------------
// Estado de la aplicación
// ---------------------------------------------------------------------------

/// Baseline marcado por el usuario (tecla `b`): contadores crudos por modelo
/// en el instante en que se marcó, para calcular el delta de ventana.
struct Baseline {
    at: Instant,
    by_key: HashMap<ModelKey, RawCounters>,
}

/// Historial acotado de un modelo para los sparklines.
#[derive(Default)]
struct History {
    throughput: VecDeque<u64>,
    ttft: VecDeque<u64>,
}

impl History {
    fn push(&mut self, throughput: u64, ttft: u64) {
        self.throughput.push_back(throughput);
        if self.throughput.len() > HISTORY_CAP {
            self.throughput.pop_front();
        }
        self.ttft.push_back(ttft);
        if self.ttft.len() > HISTORY_CAP {
            self.ttft.pop_front();
        }
    }
}

/// Estado completo de la TUI entre redraws.
struct App {
    url: String,
    latest: Vec<StatsRow>,
    baseline: Option<Baseline>,
    history: HashMap<ModelKey, History>,
    prev_poll: Option<(Instant, HashMap<ModelKey, RawCounters>)>,
    selected: usize,
    status: String,
    /// Último snapshot bueno de `/requests`, en orden cronológico (más viejo
    /// primero, tal como lo entrega el buffer). Si el último poll a
    /// `/requests` falló, esto conserva el snapshot anterior en vez de
    /// vaciarse — degradación con gracia, ver `poll_requests`.
    recent_requests: Vec<RequestRow>,
    /// Estado textual del último poll a `/requests`, separado de `status`
    /// (que es el de `/stats`) porque ambos endpoints pueden fallar de forma
    /// independiente.
    requests_status: String,
    /// Visibilidad del panel de requests recientes, toggleable con `p`.
    show_requests_panel: bool,
}

impl App {
    fn new(url: String) -> Self {
        Self {
            url,
            latest: Vec::new(),
            baseline: None,
            history: HashMap::new(),
            prev_poll: None,
            selected: 0,
            status: "esperando el primer poll...".to_string(),
            recent_requests: Vec::new(),
            requests_status: "esperando el primer poll...".to_string(),
            show_requests_panel: true,
        }
    }

    /// Hace un fetch de `/stats` y de `/requests` cada tick y actualiza todo
    /// el estado derivado. Ambos fetches son independientes entre sí: si uno
    /// falla, el otro sigue actualizándose con normalidad.
    fn poll(&mut self, client: &reqwest::blocking::Client, url: &str, requests_url: &str) {
        self.poll_stats(client, url);
        self.poll_requests(client, requests_url);
    }

    /// Hace un fetch de `/stats` y actualiza todo el estado derivado
    /// (historial de sparklines, contadores para el próximo poll). Nunca
    /// panickea si el proxy no responde: solo actualiza `status` y sigue.
    fn poll_stats(&mut self, client: &reqwest::blocking::Client, url: &str) {
        let rows = match fetch_stats(client, url) {
            Ok(rows) => rows,
            Err(e) => {
                self.status = format!("proxy no disponible en {url} ({e})");
                return;
            }
        };

        self.status = format!("ok · {} modelos", rows.len());
        let now = Instant::now();

        let mut current: HashMap<ModelKey, RawCounters> = HashMap::new();
        for r in &rows {
            current.insert(key_of(r), RawCounters::from_row(r));
        }

        if let Some((prev_at, prev_map)) = &self.prev_poll {
            let elapsed = now.duration_since(*prev_at).as_secs_f64();
            for r in &rows {
                let key = key_of(r);
                if let Some(prev) = prev_map.get(&key) {
                    let d_out = r.output_tokens.saturating_sub(prev.output_tokens);
                    let throughput = window_throughput(d_out, elapsed);
                    self.history
                        .entry(key)
                        .or_default()
                        .push(throughput as u64, r.avg_ttft_ms as u64);
                }
            }
        }

        self.prev_poll = Some((now, current));
        self.latest = rows;
        self.clamp_selected();
    }

    /// Hace un fetch de `/requests` y actualiza el buffer de requests
    /// recientes. Endpoint MÁS NUEVO que `/stats`: un proxy de build
    /// anterior puede no tenerlo. Si falla, el monitor DEGRADA con gracia —
    /// conserva el último `recent_requests` bueno y sigue funcionando con
    /// normalidad para el resto de los paneles. Nunca panickea.
    ///
    /// OJO: el fetch en sí SÍ es bloqueante (`reqwest::blocking::Client`,
    /// timeout de 3s) y corre en el mismo hilo que dibuja la TUI y lee el
    /// teclado. Un endpoint lento (no caído, lento) congela ese hilo hasta el
    /// timeout en cada ciclo de poll — no hay forma de cancelarlo desde acá.
    fn poll_requests(&mut self, client: &reqwest::blocking::Client, requests_url: &str) {
        match fetch_requests(client, requests_url) {
            Ok(rows) => {
                self.requests_status = format!("ok · {} requests", rows.len());
                self.recent_requests = rows;
            }
            Err(e) => {
                self.requests_status = format!("/requests no disponible ({e})");
            }
        }
    }

    /// Alterna la visibilidad del panel de requests recientes (tecla `p`).
    fn toggle_requests_panel(&mut self) {
        self.show_requests_panel = !self.show_requests_panel;
    }

    /// Marca el baseline en el instante actual con los contadores crudos de
    /// cada modelo visible ahora mismo.
    fn mark_baseline(&mut self) {
        let mut by_key = HashMap::new();
        for r in &self.latest {
            by_key.insert(key_of(r), RawCounters::from_row(r));
        }
        self.baseline = Some(Baseline { at: Instant::now(), by_key });
    }

    fn reset_baseline(&mut self) {
        self.baseline = None;
    }

    fn clamp_selected(&mut self) {
        if self.latest.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.latest.len() {
            self.selected = self.latest.len() - 1;
        }
    }

    fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn select_next(&mut self) {
        if !self.latest.is_empty() && self.selected + 1 < self.latest.len() {
            self.selected += 1;
        }
    }

    fn selected_row(&self) -> Option<&StatsRow> {
        self.latest.get(self.selected)
    }

    /// Delta de ventana del modelo seleccionado contra el baseline, si hay
    /// baseline marcado y el modelo ya existía en ese momento.
    fn selected_delta(&self) -> Option<WindowDelta> {
        let baseline = self.baseline.as_ref()?;
        let row = self.selected_row()?;
        let key = key_of(row);
        let base_counters = baseline.by_key.get(&key)?;
        let current = RawCounters::from_row(row);
        let elapsed = baseline.at.elapsed().as_secs_f64();
        Some(compute_window_delta(base_counters, &current, elapsed))
    }
}

// ---------------------------------------------------------------------------
// Loop principal
// ---------------------------------------------------------------------------

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, url: &str, requests_url: &str) -> io::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut app = App::new(url.to_string());
    let mut last_poll = Instant::now() - POLL_INTERVAL; // fuerza un poll inmediato

    loop {
        terminal.draw(|f| ui(f, &app))?;

        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
        {
            // Filtramos por `Press`: en backends que emiten eventos de
            // `Release` (algunos terminales Windows) un solo toque de
            // tecla dispararía la acción dos veces.
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('b') => app.mark_baseline(),
                    KeyCode::Char('r') => app.reset_baseline(),
                    KeyCode::Up => app.select_prev(),
                    KeyCode::Down => app.select_next(),
                    KeyCode::Char('p') => app.toggle_requests_panel(),
                    _ => {}
                }
            }
        }

        if last_poll.elapsed() >= POLL_INTERVAL {
            app.poll(&client, url, requests_url);
            last_poll = Instant::now();
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

fn ui(f: &mut Frame, app: &App) {
    let area = f.area();
    // El panel de requests recientes es toggleable (tecla `p`): cuando está
    // oculto, no reservamos su espacio para que los paneles fijos (header,
    // antes/después, sparklines) no se vean apretados sin necesidad.
    let mut constraints = vec![
        Constraint::Length(3), // header
        Constraint::Min(5),    // tabla principal
        Constraint::Length(6), // panel antes/después
        Constraint::Length(7), // sparklines
    ];
    if app.show_requests_panel {
        constraints.push(Constraint::Length(12)); // requests recientes + leyenda
    }
    constraints.push(Constraint::Length(1)); // footer

    let chunks = Layout::default().direction(Direction::Vertical).constraints(constraints).split(area);

    draw_header(f, chunks[0], app);
    draw_table(f, chunks[1], app);
    draw_before_after(f, chunks[2], app);
    draw_sparklines(f, chunks[3], app);

    let footer_idx = if app.show_requests_panel {
        draw_requests_panel(f, chunks[4], app);
        5
    } else {
        4
    };
    draw_footer(f, chunks[footer_idx]);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let baseline_age = match &app.baseline {
        Some(b) => format!("baseline hace {}s", b.at.elapsed().as_secs()),
        None => "sin baseline — apretá 'b'".to_string(),
    };

    let text = vec![
        Line::from(vec![
            Span::styled("OxideGate · monitor en vivo", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::raw(&app.url),
        ]),
        Line::from(vec![
            Span::raw(format!("estado: {}", app.status)),
            Span::raw("  |  "),
            Span::raw(baseline_age),
        ]),
    ];

    f.render_widget(Paragraph::new(text).block(Block::default().borders(Borders::ALL)), area);
}

fn draw_table(f: &mut Frame, area: Rect, app: &App) {
    let header = Row::new(vec!["MODELO", "REQ", "tok/s", "TTFT ms", "cache-hit", "coste $", "redun%"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .latest
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let cells = vec![
                Cell::from(format!("{}/{}", r.upstream, r.model)),
                Cell::from(r.requests.to_string()),
                Cell::from(format!("{:.1}", r.avg_tokens_per_sec)),
                Cell::from(format!("{:.1}", r.avg_ttft_ms)),
                Cell::from(format!("{:.1}%", r.cache_hit_rate() * 100.0)),
                Cell::from(format!("{:.4}", r.cost_usd)),
                Cell::from(format!("{:.1}%", r.redundancy_rate * 100.0)),
            ];
            let row = Row::new(cells);
            if i == app.selected {
                row.style(Style::default().bg(Color::Blue).fg(Color::White))
            } else {
                row
            }
        })
        .collect();

    let widths = [
        Constraint::Percentage(30),
        Constraint::Percentage(10),
        Constraint::Percentage(12),
        Constraint::Percentage(12),
        Constraint::Percentage(12),
        Constraint::Percentage(12),
        Constraint::Percentage(12),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" modelos (total acumulado) "));

    f.render_widget(table, area);
}

fn draw_before_after(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" ANTES/DESPUÉS (ventana desde baseline) ");

    let text = match (app.selected_row(), app.selected_delta()) {
        (Some(row), Some(d)) => vec![
            Line::from(format!("modelo: {}/{}", row.upstream, row.model)),
            Line::from(format!(
                "Δreq: {}   tok/s ventana: {:.1}   TTFT ventana: {:.1} ms",
                d.d_requests, d.throughput, d.avg_ttft
            )),
            Line::from(format!(
                "cache-hit ventana: {:.1}%   Δcoste: ${:.4}   Δoutput_tokens: {}   error% ventana: {:.1}%",
                d.cache_hit_rate * 100.0,
                d.d_cost_usd,
                d.d_output_tokens,
                d.error_rate * 100.0
            )),
        ],
        (Some(_), None) => vec![Line::from("sin baseline (o el modelo no existía al marcarlo) — apretá 'b'")],
        (None, _) => vec![Line::from("sin modelo seleccionado todavía")],
    };

    f.render_widget(Paragraph::new(text).block(block), area);
}

fn draw_sparklines(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let empty = History::default();
    let history = app
        .selected_row()
        .and_then(|r| app.history.get(&key_of(r)))
        .unwrap_or(&empty);

    let throughput_data: Vec<u64> = history.throughput.iter().copied().collect();
    let ttft_data: Vec<u64> = history.ttft.iter().copied().collect();

    let throughput_sparkline = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(" tok/s (histórico) "))
        .data(&throughput_data)
        .style(Style::default().fg(Color::Green));

    let ttft_sparkline = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(" TTFT ms (histórico) "))
        .data(&ttft_data)
        .style(Style::default().fg(Color::Yellow));

    f.render_widget(throughput_sparkline, chunks[0]);
    f.render_widget(ttft_sparkline, chunks[1]);
}

/// Panel de requests recientes, más nuevo arriba (ver comentario de
/// inversión más abajo), con marcadores de outlier por fila. Nunca indexa el
/// área sin antes chequear que tiene alto/ancho positivo: en una terminal
/// muy chica el `Constraint::Length(12)` de arriba puede terminar recortado
/// a un área de 0 filas, y `Layout::split` sobre un área vacía no debe
/// panickear el render.
fn draw_requests_panel(f: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" requests recientes · {} ", app.requests_status));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // La última línea del panel se reserva para la leyenda de marcadores;
    // el resto es la tabla (que a su vez usa su primera fila para el header).
    let legend_height = 1u16.min(inner.height);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(legend_height)])
        .split(inner);
    let table_area = layout[0];
    let legend_area = layout[1];

    if table_area.height > 1 {
        let outliers = classify_outliers(&app.recent_requests);

        // El buffer llega en orden cronológico (más viejo primero); acá lo
        // invertimos para mostrar MÁS NUEVO ARRIBA, que es como se lee un
        // panel de "últimos eventos". `classify_outliers` se calculó sobre
        // el orden original para que las estadísticas del grupo no cambien.
        let mut indexed: Vec<(usize, &RequestRow)> = app.recent_requests.iter().enumerate().collect();
        indexed.reverse();

        // La tabla reserva su propia primera fila para el header.
        let capacity = (table_area.height - 1) as usize;
        indexed.truncate(capacity);

        let header = Row::new(vec![
            "hora", "modelo", "st", "status", "in", "out", "c_rd", "c_wr", "ttft_ms", "gen_ms", "tok/s", "usd", "outlier",
        ])
        .style(Style::default().add_modifier(Modifier::BOLD));

        let rows: Vec<Row> = indexed
            .iter()
            .map(|(i, r)| {
                let kinds = &outliers[*i];
                let row = Row::new(vec![
                    Cell::from(format_time(&r.timestamp)),
                    Cell::from(truncate_model(r.model.as_deref())),
                    Cell::from(if r.stream { "y" } else { "n" }),
                    Cell::from(r.status.to_string()),
                    Cell::from(opt_u64(r.input_tokens)),
                    Cell::from(opt_u64(r.output_tokens)),
                    Cell::from(opt_u64(r.cache_read_tokens)),
                    Cell::from(opt_u64(r.cache_write_tokens)),
                    Cell::from(opt_fixed(r.ttft_ms, 1)),
                    Cell::from(opt_fixed(gen_ms_of(r), 1)),
                    Cell::from(tokens_per_sec_cell(r)),
                    Cell::from(opt_fixed(r.cost_estimate_usd, 4)),
                    Cell::from(marker_text(kinds)),
                ]);
                if kinds.is_empty() {
                    row
                } else {
                    row.style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
                }
            })
            .collect();

        let widths = [
            Constraint::Length(9),
            Constraint::Length(16),
            Constraint::Length(3),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(14),
        ];

        f.render_widget(Table::new(rows, widths).header(header), table_area);
    }

    if legend_area.height > 0 {
        let legend = Paragraph::new(Line::from(
            "leyenda: ERR=error(status>=400) · MISS=cache-miss atípico · TTFT=TTFT lento(>=2σ) · SLOW=generación lenta(>=2σ)",
        ));
        f.render_widget(legend, legend_area);
    }
}

/// Extrae `HH:MM:SS` (UTC) de un timestamp RFC3339. Si el timestamp no
/// parsea (dato corrupto o formato inesperado), devuelve el string crudo tal
/// cual llegó: mejor mostrar el dato raro que ocultarlo con un placeholder
/// engañoso.
fn format_time(timestamp: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(timestamp) {
        Ok(dt) => dt.with_timezone(&chrono::Utc).format("%H:%M:%S").to_string(),
        Err(_) => timestamp.to_string(),
    }
}

/// Máximo de caracteres para el nombre de modelo en la columna de la tabla,
/// para no romper el ancho fijo de columna con nombres largos.
const MODEL_DISPLAY_MAX: usize = 16;

/// Trunca el nombre del modelo a [`MODEL_DISPLAY_MAX`] caracteres. `None` se
/// muestra como `-`, nunca como string vacío (que se confundiría con una
/// celda sin renderizar).
fn truncate_model(model: Option<&str>) -> String {
    match model {
        None => "-".to_string(),
        Some(m) if m.chars().count() <= MODEL_DISPLAY_MAX => m.to_string(),
        Some(m) => {
            let head: String = m.chars().take(MODEL_DISPLAY_MAX.saturating_sub(1)).collect();
            format!("{head}…")
        }
    }
}

/// `None` se renderiza como `-`, NUNCA como `0`: un `0` real (p. ej. 0 tokens
/// de caché) y un dato ausente son cosas distintas para el usuario.
fn opt_u64(v: Option<u64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "-".to_string())
}

/// Igual que [`opt_u64`] pero para `f64`, con precisión fija de `decimals`.
/// Filtra valores no finitos (NaN/inf) como si fueran `None`: no deberían
/// llegar hasta acá, pero un `-` es preferible a imprimir `NaN` en la UI.
fn opt_fixed(v: Option<f64>, decimals: usize) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{x:.decimals$}"),
        _ => "-".to_string(),
    }
}

/// Celda de `tok/s` para la tabla: reusa [`generation_throughput`] para que
/// la columna visible y el cálculo de `SlowGeneration` sean SIEMPRE
/// consistentes entre sí (mismo criterio de qué filas son calculables).
fn tokens_per_sec_cell(r: &RequestRow) -> String {
    let (Some(out), Some(ttft)) = (r.output_tokens, r.ttft_ms) else {
        return "-".to_string();
    };
    match generation_throughput(out, r.total_ms, ttft) {
        Some(v) => format!("{v:.1}"),
        None => "-".to_string(),
    }
}

/// Texto de marcadores de una fila, p. ej. `"ERR+TTFT"`. `-` si la fila no
/// tiene ningún outlier. El color de fila es solo refuerzo visual: este
/// texto es la señal que también funciona sin color.
fn marker_text(kinds: &[OutlierKind]) -> String {
    if kinds.is_empty() {
        "-".to_string()
    } else {
        kinds.iter().map(|k| k.marker()).collect::<Vec<_>>().join("+")
    }
}

/// Imprime la tabla de requests recientes en texto plano (modo `--once`),
/// más nuevo arriba, con los mismos marcadores de outlier que la TUI.
fn print_requests_table(rows: &[RequestRow]) {
    let outliers = classify_outliers(rows);

    println!(
        "{:<10} {:<16} {:>2} {:>6} {:>6} {:>6} {:>6} {:>6} {:>8} {:>8} {:>7} {:>8} {:<14}",
        "HORA", "MODELO", "st", "status", "in", "out", "c_rd", "c_wr", "ttft_ms", "gen_ms", "tok/s", "usd", "outlier"
    );
    for (i, r) in rows.iter().enumerate().rev() {
        println!(
            "{:<10} {:<16} {:>2} {:>6} {:>6} {:>6} {:>6} {:>6} {:>8} {:>8} {:>7} {:>8} {:<14}",
            format_time(&r.timestamp),
            truncate_model(r.model.as_deref()),
            if r.stream { "y" } else { "n" },
            r.status,
            opt_u64(r.input_tokens),
            opt_u64(r.output_tokens),
            opt_u64(r.cache_read_tokens),
            opt_u64(r.cache_write_tokens),
            opt_fixed(r.ttft_ms, 1),
            opt_fixed(gen_ms_of(r), 1),
            tokens_per_sec_cell(r),
            opt_fixed(r.cost_estimate_usd, 4),
            marker_text(&outliers[i]),
        );
    }
    println!("leyenda: ERR=error(status>=400) · MISS=cache-miss atípico · TTFT=TTFT lento(>=2σ) · SLOW=generación lenta(>=2σ)");
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let text = Line::from("q salir · b marcar baseline · r reset · ↑/↓ elegir modelo · p mostrar/ocultar requests");
    f.render_widget(Paragraph::new(text), area);
}

impl StatsRow {
    /// `cache_hit_rate` ya viaja calculado en la fila; este helper solo le
    /// da un nombre explícito en el sitio de uso de la tabla.
    fn cache_hit_rate(&self) -> f64 {
        self.cache_hit_rate
    }
}

// ---------------------------------------------------------------------------
// Tests — matemática de delta, sin terminal ni HTTP de por medio
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_throughput_divide_tokens_por_tiempo() {
        assert!((window_throughput(100, 10.0) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn window_throughput_cero_si_elapsed_no_positivo() {
        assert_eq!(window_throughput(100, 0.0), 0.0);
        assert_eq!(window_throughput(100, -1.0), 0.0);
    }

    #[test]
    fn window_cache_hit_rate_calcula_fraccion() {
        // cache_read=30, denom=(10+30+0)=40
        assert!((window_cache_hit_rate(10, 30, 0) - 0.75).abs() < 1e-9);
    }

    #[test]
    fn window_cache_hit_rate_cero_si_denom_cero() {
        assert_eq!(window_cache_hit_rate(0, 0, 0), 0.0);
    }

    #[test]
    fn window_avg_ttft_divide_suma_por_count() {
        assert!((window_avg_ttft(300.0, 3) - 100.0).abs() < 1e-9);
    }

    #[test]
    fn window_avg_ttft_cero_si_count_cero() {
        assert_eq!(window_avg_ttft(300.0, 0), 0.0);
    }

    #[test]
    fn window_error_rate_calcula_fraccion() {
        assert!((window_error_rate(1, 4) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn window_error_rate_cero_si_sin_requests() {
        assert_eq!(window_error_rate(0, 0), 0.0);
    }

    fn raw(requests: u64, output_tokens: u64, ttft_sum: f64, ttft_count: u64, cost: f64) -> RawCounters {
        RawCounters {
            requests,
            input_tokens: 0,
            output_tokens,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: cost,
            ttft_ms_sum: ttft_sum,
            ttft_ms_count: ttft_count,
            errors: 0,
        }
    }

    #[test]
    fn compute_window_delta_resta_baseline_de_current() {
        let baseline = raw(10, 1000, 500.0, 10, 0.10);
        let current = raw(15, 1500, 800.0, 15, 0.25);

        let d = compute_window_delta(&baseline, &current, 10.0);

        assert_eq!(d.d_requests, 5);
        assert_eq!(d.d_output_tokens, 500);
        assert!((d.d_cost_usd - 0.15).abs() < 1e-9);
        // throughput = 500 tokens / 10s = 50 tok/s
        assert!((d.throughput - 50.0).abs() < 1e-9);
        // ttft ventana = (800-500)/(15-10) = 300/5 = 60
        assert!((d.avg_ttft - 60.0).abs() < 1e-9);
    }

    #[test]
    fn compute_window_delta_no_underflowea_si_current_retrocede() {
        // Si el proxy se reinicia entre el baseline y el poll actual, los
        // contadores pueden "retroceder". saturating_sub debe dar 0, no
        // panickear ni envolver a un u64 gigante.
        let baseline = raw(10, 1000, 500.0, 10, 0.50);
        let current = raw(2, 100, 50.0, 2, 0.05);

        let d = compute_window_delta(&baseline, &current, 5.0);

        assert_eq!(d.d_requests, 0);
        assert_eq!(d.d_output_tokens, 0);
        assert_eq!(d.d_cost_usd, 0.0);
        assert_eq!(d.throughput, 0.0);
    }

    #[test]
    fn resolve_url_usa_flag_si_esta_presente() {
        let args = vec!["monitor".to_string(), "--url".to_string(), "http://x:1/stats".to_string()];
        assert_eq!(resolve_url(&args), "http://x:1/stats");
    }

    // -----------------------------------------------------------------
    // resolve_requests_url_inner — precedencia, sin tocar std::env
    // -----------------------------------------------------------------

    #[test]
    fn resolve_requests_url_deriva_del_stats_url_del_flag_override() {
        // Caso `--url http://x:1/stats`: la URL de /requests se deriva
        // reemplazando el sufijo /stats por /requests.
        assert_eq!(resolve_requests_url_inner("http://x:1/stats", None, None), "http://x:1/requests");
    }

    #[test]
    fn resolve_requests_url_usa_env_override_si_esta_presente() {
        assert_eq!(
            resolve_requests_url_inner("http://x:1/stats", Some("http://y:2/requests".to_string()), None),
            "http://y:2/requests"
        );
    }

    #[test]
    fn resolve_requests_url_env_override_gana_aunque_stats_url_termine_en_stats() {
        // El override explícito tiene prioridad sobre la derivación por
        // sustitución, incluso si esta última sería válida.
        assert_eq!(
            resolve_requests_url_inner("http://x:1/stats", Some("http://z:3/requests".to_string()), Some("9999".to_string())),
            "http://z:3/requests"
        );
    }

    #[test]
    fn resolve_requests_url_fallback_si_stats_url_no_termina_en_stats() {
        assert_eq!(resolve_requests_url_inner("http://x:1/weird", None, None), "http://127.0.0.1:8080/requests");
    }

    #[test]
    fn resolve_requests_url_fallback_respeta_port_env() {
        assert_eq!(
            resolve_requests_url_inner("http://x:1/weird", None, Some("9090".to_string())),
            "http://127.0.0.1:9090/requests"
        );
    }

    // -----------------------------------------------------------------
    // classify_outliers — la parte central de este slice
    // -----------------------------------------------------------------

    /// Construye una `RequestRow` de prueba con los campos relevantes para
    /// la detección de outliers; el resto queda en valores neutros.
    fn req(
        upstream: &str,
        model: &str,
        status: u16,
        ttft_ms: Option<f64>,
        total_ms: f64,
        output_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
    ) -> RequestRow {
        RequestRow {
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            route: "/v1/messages".to_string(),
            upstream: upstream.to_string(),
            model: Some(model.to_string()),
            stream: true,
            status,
            input_tokens: Some(100),
            output_tokens,
            cache_read_tokens,
            cache_write_tokens: Some(0),
            cost_estimate_usd: Some(0.01),
            cache_control_forced: false,
            ttft_ms,
            total_ms,
        }
    }

    #[test]
    fn classify_outliers_input_vacio_devuelve_vacio() {
        let result = classify_outliers(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn classify_outliers_grupo_bajo_el_minimo_no_flaggea_estadistica() {
        // 3 filas (< MIN_GROUP_SAMPLE=5), con un TTFT que a simple vista
        // parece un outlier clarísimo (1000 vs 10, 10). Con una muestra tan
        // chica, el desvío estándar no es confiable: no debe flaggearse
        // SlowTtft (ni ningún otro estadístico), solo Error si lo hubiera.
        let rows = vec![
            req("anthropic", "claude-opus-4", 200, Some(10.0), 100.0, Some(50), Some(10)),
            req("anthropic", "claude-opus-4", 200, Some(10.0), 100.0, Some(50), Some(10)),
            req("anthropic", "claude-opus-4", 200, Some(1000.0), 1100.0, Some(50), Some(10)),
        ];

        let result = classify_outliers(&rows);

        assert!(result.iter().all(Vec::is_empty));
    }

    #[test]
    fn classify_outliers_grupo_con_stddev_cero_no_flaggea() {
        // 5 filas con TTFT idéntico: stddev=0, no hay variación real que
        // reportar como outlier.
        let rows: Vec<RequestRow> = (0..5)
            .map(|_| req("anthropic", "claude-opus-4", 200, Some(100.0), 200.0, Some(50), Some(10)))
            .collect();

        let result = classify_outliers(&rows);

        assert!(result.iter().all(Vec::is_empty));
    }

    #[test]
    fn classify_outliers_detecta_ttft_lento_a_2_sigma() {
        // ttft = [10,10,10,10,10,100]; mean=25, stddev≈33.54,
        // threshold=mean+2*stddev≈92.08. Solo la fila de 100 debe flaggearse.
        let mut rows: Vec<RequestRow> =
            (0..5).map(|_| req("anthropic", "claude-opus-4", 200, Some(10.0), 200.0, Some(50), Some(10))).collect();
        rows.push(req("anthropic", "claude-opus-4", 200, Some(100.0), 300.0, Some(50), Some(10)));

        let result = classify_outliers(&rows);

        assert!(result[0..5].iter().all(|k| !k.contains(&OutlierKind::SlowTtft)));
        assert!(result[5].contains(&OutlierKind::SlowTtft));
    }

    #[test]
    fn classify_outliers_detecta_cache_miss_entre_filas_cacheadas() {
        // 4 filas con cache-hit real (cache_read_tokens > 0) + 1 fila sin
        // cache-hit: la mitad+ de las OTRAS filas del grupo tienen hit, así
        // que la fila sin hit debe flaggearse CacheMiss. Las cacheadas no.
        let mut rows: Vec<RequestRow> =
            (0..4).map(|_| req("anthropic", "claude-opus-4", 200, Some(50.0), 200.0, Some(50), Some(500))).collect();
        rows.push(req("anthropic", "claude-opus-4", 200, Some(50.0), 200.0, Some(50), None));

        let result = classify_outliers(&rows);

        assert!(result[0..4].iter().all(|k| !k.contains(&OutlierKind::CacheMiss)));
        assert!(result[4].contains(&OutlierKind::CacheMiss));
    }

    #[test]
    fn classify_outliers_no_streaming_con_total_igual_a_ttft_no_es_slow_generation() {
        // total_ms == ttft_ms => gen_ms == 0: el throughput no es calculable
        // para esta fila y debe EXCLUIRSE de la métrica, no tratarse como
        // lenta, aunque el resto del grupo tenga throughput normal.
        let mut rows: Vec<RequestRow> =
            (0..4).map(|_| req("anthropic", "claude-opus-4", 200, Some(50.0), 550.0, Some(500), Some(10))).collect();
        rows.push(req("anthropic", "claude-opus-4", 200, Some(100.0), 100.0, Some(500), Some(10)));

        let result = classify_outliers(&rows);

        assert!(!result[4].contains(&OutlierKind::SlowGeneration));
    }

    #[test]
    fn classify_outliers_error_se_flaggea_incluso_con_una_sola_fila() {
        let rows = vec![req("anthropic", "claude-opus-4", 500, Some(10.0), 100.0, Some(50), Some(10))];

        let result = classify_outliers(&rows);

        assert_eq!(result[0], vec![OutlierKind::Error]);
    }

    #[test]
    fn classify_outliers_nan_en_ttft_no_panickea_y_se_excluye_de_la_media() {
        // Una fila con NaN no debería ni flaggearse a sí misma como
        // SlowTtft, ni contaminar la media/stddev usada para las demás.
        let mut rows: Vec<RequestRow> =
            (0..4).map(|_| req("anthropic", "claude-opus-4", 200, Some(10.0), 200.0, Some(50), Some(10))).collect();
        rows.push(req("anthropic", "claude-opus-4", 200, Some(f64::NAN), 200.0, Some(50), Some(10)));

        let result = classify_outliers(&rows);

        // No debe panickear (llegar acá ya lo prueba) y la fila NaN no debe
        // quedar flaggeada como SlowTtft.
        assert!(!result[4].contains(&OutlierKind::SlowTtft));
    }

    #[test]
    fn classify_outliers_none_en_ttft_se_excluye_sin_flaggear() {
        let mut rows: Vec<RequestRow> =
            (0..4).map(|_| req("anthropic", "claude-opus-4", 200, Some(10.0), 200.0, Some(50), Some(10))).collect();
        rows.push(req("anthropic", "claude-opus-4", 200, None, 200.0, Some(50), Some(10)));

        let result = classify_outliers(&rows);

        assert!(result[4].is_empty());
    }
}
