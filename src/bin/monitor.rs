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
//!   cargo run --bin oxidegate-monitor              # TUI interactiva
//!   cargo run --bin oxidegate-monitor -- --once    # snapshot de texto plano y sale
//!   cargo run --bin oxidegate-monitor -- --url http://127.0.0.1:8080/stats
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
//!   c         ciclar la vista de columnas del panel de requests
//!             (Latency ⇄ Context); no-op si el panel está oculto (`p`)
//!   s         mostrar/ocultar el panel de "tools por servidor" (desglose
//!             de bytes de herramientas MCP, con delta contra el baseline
//!             marcado con `b`); INDEPENDIENTE de `p`/`c`
//!   u         mostrar/ocultar el panel de cuota de suscripción Codex
//!             (uso de cuota); INDEPENDIENTE de `p`/`c`/`s`
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
use std::collections::{BTreeMap, HashMap, VecDeque};
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
            // `--once` es el modo para pegar resultados en texto plano en
            // una conversación: imprime AMBAS vistas (Latency y Context),
            // no una sola, cada una con su propio header — el usuario no
            // tiene forma de "apretar `c`" en un snapshot que ya salió.
            println!("--- vista: latency ---");
            print_requests_table(&rows);
            println!();
            println!("--- vista: context ---");
            print_context_table(&rows);
            println!();
            print_tools_table(&rows);
            println!();
            print_quota_table(&rows);
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
    /// `User-Agent` del cliente que originó el request. Espejo de
    /// `RecentRequest::client` (`src/telemetry/recent.rs`): crudo, topeado en
    /// longitud del lado del proxy, nunca clasificado ni interpretado acá.
    /// `None` si el header no vino.
    client: Option<String>,
    status: u16,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    cost_estimate_usd: Option<f64>,
    #[allow(dead_code)]
    cache_control_forced: bool,
    /// Nivel de esfuerzo de razonamiento PEDIDO por el cliente
    /// (`output_config.effort`). Dialecto exclusivo de Anthropic. `None`
    /// tanto si el proxy no lo reportó (build anterior a este campo, clave
    /// ausente en el JSON) como si el request no lo pedía explícitamente:
    /// espejo de `RecentRequest::requested_effort` del lado del proxy.
    requested_effort: Option<String>,
    /// Modo de velocidad PEDIDO por el cliente (`speed` a nivel raíz,
    /// `"fast"` en el beta de Anthropic). SEPARADO a propósito de
    /// `served_speed`: un request puede pedir `"fast"` y ser servido a
    /// `"standard"` si el rate limit del modo rápido se activó.
    requested_speed: Option<String>,
    /// Velocidad con la que el proveedor SIRVIÓ REALMENTE la respuesta
    /// (`usage.speed`). DOCUMENTADA por Anthropic, NO OBSERVADA todavía en
    /// tráfico real de este proyecto: `None` significa "no reportada", nunca
    /// "estándar".
    served_speed: Option<String>,
    ttft_ms: Option<f64>,
    total_ms: f64,

    // --- Desglose de contexto (espejo de `RecentRequest` en
    //     `src/telemetry/recent.rs`; ver esos docs para el significado
    //     completo de cada campo) ---
    context_system_bytes: Option<usize>,
    context_tools_bytes: Option<usize>,
    context_history_bytes: Option<usize>,
    context_last_turn_bytes: Option<usize>,
    context_other_bytes: Option<usize>,
    context_measured_bytes: Option<usize>,
    context_messages_count: Option<usize>,
    context_tax_ratio: Option<f64>,
    /// Microsegundos que el proxy pasó dentro de `Provider::prepare`.
    ///
    /// En `RecentRequest` (lado servidor) este campo NO es `Option`: el proxy
    /// siempre lo mide. Acá SÍ lo es, a propósito. El tipo del espejo no
    /// tiene por qué copiar al del servidor: modela lo que el monitor puede
    /// SABER. Un proxy de build anterior a este slice no manda la clave, y
    /// `serde` deja un `Option` ausente en `None` sin necesidad de atributos.
    ///
    /// `None` significa "el proxy no lo informó"; `Some(0)` significaría "lo
    /// midió y dio cero". Colapsar ambos casos en `0` sería inventar una
    /// medición que nadie hizo: este proyecto prefiere un hueco honesto a un
    /// cero falso.
    prepare_us: Option<u64>,

    /// Desglose de `context_tools_bytes` por servidor MCP declarante (ver
    /// [`ToolServerRow`] y `provider::ToolServerBytes` del lado del proxy).
    /// Mismo contrato `None`/`Some` que el resto de los campos opcionales de
    /// este struct, con una distinción CRÍTICA entre sus dos estados no-`None`:
    ///
    /// - `None`: el body no parseó como objeto JSON (no se pudo ni intentar
    ///   calcular el desglose), o el proxy es de una build anterior a este
    ///   campo y ni siquiera manda la clave.
    /// - `Some(vec![])`: el body SÍ parseó, pero no declaraba `tools`
    ///   (ausente, no-array, o array vacío) — es un dato real de "cero
    ///   servidores", no un hueco.
    ///
    /// Confundir ambos estados llevaría a elegir la fila equivocada como
    /// fuente del panel de tools por servidor (ver [`find_tools_source_row`]),
    /// por eso NUNCA se colapsan entre sí.
    tools_by_server: Option<Vec<ToolServerRow>>,
    /// Bytes de `tools` no atribuidos a ningún servidor (ver
    /// `provider::tools_overhead_bytes` del lado del proxy: brackets/comas
    /// del array, wrapper de Gemini, herramientas huérfanas sin `name`
    /// válido). Mismo contrato `None`/`Some` que `tools_by_server`.
    tools_overhead_bytes: Option<usize>,
    /// Estado de cuota de suscripción Codex de esta petición puntual (ver
    /// [`CodexQuotaRow`]). `Some` únicamente si la petición se enrutó al
    /// backend de Codex vía OAuth y el upstream mandó al menos una cabecera
    /// `x-codex-*`; `None` para el resto del tráfico (Anthropic, Gemini,
    /// OpenAI vía API key) y para un proxy anterior a esta captura. Fuente
    /// del panel de cuota (tecla `u`, ver [`find_quota_source_row`]).
    codex_quota: Option<CodexQuotaRow>,
}

/// Fila del desglose de `tools` por servidor: espejo local y liviano de
/// `provider::ToolServerBytes` (ver ese tipo en el proxy para el contrato
/// completo). A diferencia del original, `kind` viaja como `String` plana en
/// vez de espejar el enum `provider::ToolServerKind`: el monitor solo
/// MUESTRA este valor (llega ya serializado en minúsculas —
/// `"native"`/`"mcp"`/`"others"` — vía `#[serde(rename_all = "lowercase")]`
/// del lado del proxy), nunca decide nada en base a qué variante es.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct ToolServerRow {
    /// Etiqueta de display del servidor (`(native)`, `claude_ai_Gmail`, …).
    /// Ver la nota de `provider::ToolServerBytes::server` sobre por qué este
    /// nombre por sí solo no alcanza para distinguir cubos (para eso está
    /// `kind`).
    server: String,
    /// `"native"` / `"mcp"` / `"others"`, tal cual lo serializa el proxy.
    kind: String,
    /// Cantidad de herramientas atribuidas a este servidor.
    tools: usize,
    /// Suma de bytes de las herramientas de este servidor.
    bytes: usize,
    /// Cuántas de `tools` traían `defer_loading: true` en el body ENTRANTE.
    /// Espejo de `provider::ToolServerBytes::deferred_tools`: es la fuente de
    /// verdad POR SERVIDOR. `deferred_tools == tools` ⇒ servidor totalmente
    /// diferido; `== 0` ⇒ nada diferido (sus `bytes` son reales y
    /// desconectables); en el medio ⇒ diferido parcial.
    ///
    /// `Option<usize>`, NO `usize` con `#[serde(default)]`: un proxy de build
    /// anterior a este campo manda la fila de `tools_by_server` SIN esta
    /// clave. Con `#[serde(default)]` sobre un `usize` eso caería en `0` —
    /// indistinguible de un proxy que SÍ midió y confirmó "nada diferido". Es
    /// el mismo criterio que ya siguen el resto de los campos opcionales de
    /// este archivo (p. ej. `RequestRow::prepare_us`): `None` es "el proxy no
    /// lo informó", `Some(0)` es "lo midió y dio cero" — nunca se colapsan
    /// entre sí. Ver `deferred_cell` para cómo se renderiza el tercer estado.
    deferred_tools: Option<usize>,
}

/// Espejo local de [`CodexQuota`](../../src/telemetry/codex_quota.rs) (12
/// campos, mismo contrato de saneo: cabecera ausente/vacía/malformada →
/// `None`, nunca un `0`/`""` fabricado). El monitor es un binario
/// independiente sin `lib.rs` que importar, así que redefine el struct con
/// los mismos nombres y tipos — ver `RecentRequest::codex_quota`
/// (`src/telemetry/recent.rs`) y `telemetry::codex_quota::CodexQuota` para el
/// contrato completo campo a campo, incluida la separación estricta respecto
/// de `cost_estimate_usd` (cuota y dólares nunca se mezclan). Fuente del
/// panel de cuota, tecla `u` (ver [`find_quota_source_row`]).
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct CodexQuotaRow {
    plan_type: Option<String>,
    active_limit: Option<String>,
    credits_balance: Option<String>,
    primary_used_percent: Option<u64>,
    secondary_used_percent: Option<u64>,
    primary_window_minutes: Option<u64>,
    secondary_window_minutes: Option<u64>,
    primary_reset_after_seconds: Option<u64>,
    primary_reset_at: Option<i64>,
    secondary_reset_at: Option<i64>,
    credits_has_credits: Option<bool>,
    credits_unlimited: Option<bool>,
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

/// Diferencia relativa mínima entre `context_measured_bytes` de dos filas del
/// mismo grupo de "tope de tokens" (ver [`classify_truncation`]) para que la
/// diferencia se considere MATERIAL y no ruido de serialización. Se expresa
/// como fracción del body MÁS GRANDE del par:
/// `(max_bytes - min_bytes) / max_bytes >= TRUNCATION_BYTES_DELTA`.
///
/// Por qué una FRACCIÓN y no un piso absoluto de bytes: si un body crece en
/// `ΔB` bytes y el total de tokens reportado no se mueve, esos tokens que
/// "faltan" son aproximadamente `ΔB / (bytes por token)`. Como
/// `total_bytes ≈ (bytes por token) × tokens`, el DELTA RELATIVO de bytes
/// es, en consecuencia, aproximadamente la FRACCIÓN del prompt que
/// desapareció en silencio — `(max_bytes - min_bytes) / max_bytes >= X`
/// significa literalmente "al menos X del prompt se perdió sin contarse".
/// Es un enunciado de dominio, no un número mágico, y escala de forma
/// correcta con el tamaño del body: el ruido de serialización (un UUID, un
/// timestamp, un request id) es una fracción cada vez más chica cuanto más
/// grande es el prompt, exactamente como se espera de ruido — mientras que
/// un piso absoluto de bytes (o de tokens implícitos) no distingue "500 B de
/// ruido en un body de 1 kB" de "500 B de ruido en un body de 200 kB", que
/// son señales completamente distintas.
///
/// Calibración: el valor anterior (0.10) se fijó mirando un solo caso
/// observado que difería en ~34% (18.955 B vs. 28.806 B, ambos con
/// `input_tokens = 4095`) y produjo un FALSO NEGATIVO medido sobre tráfico
/// real: dos requests de OpenCode contra un Ollama local (`llama3.2:3b`,
/// `num_ctx = 4096`) reportaron EXACTAMENTE 4095 tokens de prompt con bodies
/// de 77.579 B y 84.161 B — una diferencia real de truncamiento del 7,8%,
/// por debajo del 10% exigido, que el detector dejó pasar. `0.05` cubre
/// ambos casos reales observados (7,8% y ~34%) con margen, y sigue muy por
/// encima de la banda de ruido de serialización (fracciones de punto
/// porcentual).
const TRUNCATION_BYTES_DELTA: f64 = 0.05;

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
    /// El total de tokens de prompt de esta fila ([`prompt_tokens_total`])
    /// coincide EXACTAMENTE con el de al menos otra fila del mismo grupo,
    /// mientras sus `context_measured_bytes` difieren entre sí en al menos
    /// [`TRUNCATION_BYTES_DELTA`]. Ver [`classify_truncation`] para el
    /// detector completo.
    ///
    /// Que dos bodies de tamaño MUY distinto reporten el MISMO total de
    /// tokens no es una coincidencia: es la firma de que el proveedor dejó
    /// de contar al llegar a un tope (`num_ctx` de Ollama, ventana de
    /// contexto, etc.) y truncó en silencio el resto del prompt, devolviendo
    /// `200 OK` igual. A diferencia de `SlowTtft`/`SlowGeneration`/
    /// `CacheMiss`, esto NO es un test estadístico (no usa media ni desvío)
    /// y por eso no está gateado por [`MIN_GROUP_SAMPLE`].
    Truncated,
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
            OutlierKind::Truncated => "TRUNC",
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

/// Total de tokens de "prompt" (contexto enviado al proveedor) de una fila,
/// con el denominador correcto según el dialecto de contabilidad de caché de
/// `upstream`. `None` si `input_tokens` no vino en la fila: sin ese dato base
/// no hay total que calcular, y tratarlo como `0` inventaría un denominador
/// falso en [`bytes_per_token`].
///
/// - `upstream == "anthropic"`: `input_tokens + cache_read_tokens +
///   cache_write_tokens` (caché APARTE del input medido). Un request
///   cacheado real puede reportar `input_tokens = 2` con
///   `cache_read_tokens` en las decenas de miles — sumarlas es obligatorio o
///   el denominador queda absurdamente chico y dispara falsos positivos de
///   truncamiento en el request MÁS SANO posible (el que mejor aprovechó la
///   caché).
/// - cualquier otro `upstream` (OpenAI, Gemini, y cualquier proveedor
///   compatible con su API — p. ej. Ollama vía el provider `openai`, ver
///   `src/provider/openai.rs`): `input_tokens` solo. `cache_read_tokens` ya
///   es SUBCONJUNTO de `input_tokens` en estos dialectos; sumarlo encima
///   sería doble conteo.
///
/// ESTA FUNCIÓN DUPLICA A PROPÓSITO conocimiento que
/// `src/telemetry/pricing.rs::CacheAccounting` ya posee del lado del proxy
/// (`Separate` para Anthropic, `Subset` para OpenAI/Gemini). La duplicación
/// existe porque `monitor` es un binario INDEPENDIENTE — el crate no expone
/// `lib.rs` (ver el comentario de cabecera del archivo), así que este binario
/// no puede hacer `use crate::telemetry::pricing::CacheAccounting`. Si la
/// semántica de contabilidad de caché de `pricing.rs` cambia (nuevo
/// proveedor, un dialecto que pasa de `Subset` a `Separate`, etc.), ESTA
/// FUNCIÓN DEBE ACTUALIZARSE A LA PAR: no hay ningún mecanismo del
/// compilador que fuerce esa sincronía desde acá, solo esta nota.
fn prompt_tokens_total(row: &RequestRow) -> Option<u64> {
    let input = row.input_tokens?;
    if row.upstream == "anthropic" {
        let cache_read = row.cache_read_tokens.unwrap_or(0);
        let cache_write = row.cache_write_tokens.unwrap_or(0);
        Some(input + cache_read + cache_write)
    } else {
        Some(input)
    }
}

/// Bytes medidos de contexto por token de prompt: `context_measured_bytes /
/// prompt_tokens_total(row)`. `None` si falta cualquiera de los dos datos, o
/// si el total de tokens es `0` (denominador indefinido) — NUNCA se devuelve
/// `0.0` para un valor que en realidad no se pudo calcular.
///
/// Este ratio es la escotilla de escape para el caso de UNA sola fila, donde
/// [`classify_truncation`] no puede probar nada (hacen falta >= 2 muestras
/// con el mismo total de tokens). No hay una constante universal de
/// bytes-por-token contra la que comparar: cada tokenizer da un ratio
/// distinto (datos reales medidos: Anthropic ~2.7, llama.cpp/Ollama ~4.1) —
/// ver `docs/monitor-tui.md` para cómo se lee este número en la práctica.
fn bytes_per_token(row: &RequestRow) -> Option<f64> {
    let bytes = row.context_measured_bytes?;
    let tokens = prompt_tokens_total(row)?;
    if tokens == 0 {
        return None;
    }
    Some(bytes as f64 / tokens as f64)
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

        // Truncated NO es un test estadístico (no usa media ni desvío
        // estándar): la prueba es una igualdad exacta de tokens más una
        // diferencia de tamaño material entre bodies, y esa prueba es igual
        // de válida con 2 muestras que con 50. Por eso corre ANTES del gate
        // de MIN_GROUP_SAMPLE, no después.
        classify_truncation(rows, indices, &mut result);

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

/// Sub-paso de [`classify_outliers`]: marca `Truncated` en TODAS las filas
/// del grupo que comparten un mismo total de tokens de prompt
/// ([`prompt_tokens_total`]) cuando ese total lo reportan >= 2 filas VÁLIDAS
/// cuyos `context_measured_bytes` difieren entre sí en al menos
/// [`TRUNCATION_BYTES_DELTA`].
///
/// Filas sin [`prompt_tokens_total`] (falta `input_tokens`) o sin
/// `context_measured_bytes` se EXCLUYEN del análisis por completo — no se
/// tratan como cero ni participan del agrupamiento por token.
///
/// Deliberadamente NO gateado por [`MIN_GROUP_SAMPLE`]: no es una
/// comparación contra una distribución (no hay media ni desvío de por
/// medio), es una igualdad exacta de tokens combinada con una diferencia de
/// tamaño de body que ya de por sí es la prueba. Exigir 5 muestras acá
/// escondería el caso real que motivó este detector, donde 2 probes ya
/// prueban el tope.
///
/// Un grupo donde TODOS los bodies miden lo mismo (p. ej. probes idénticos
/// repetidos) NO flaggea nada: coincidir en tokens Y en bytes es lo
/// ESPERADO, no una señal de truncamiento — para eso existe justamente el
/// guard de [`TRUNCATION_BYTES_DELTA`].
fn classify_truncation(rows: &[RequestRow], indices: &[usize], result: &mut [Vec<OutlierKind>]) {
    // token_total -> [(índice de fila, bytes medidos), ...]
    let mut by_token: HashMap<u64, Vec<(usize, usize)>> = HashMap::new();
    for &i in indices {
        let Some(tokens) = prompt_tokens_total(&rows[i]) else { continue };
        let Some(bytes) = rows[i].context_measured_bytes else { continue };
        by_token.entry(tokens).or_default().push((i, bytes));
    }

    for samples in by_token.values() {
        // Hacen falta >= 2 muestras para EXCLUIR la coincidencia: un solo
        // sample con ese total de tokens no prueba nada (podría ser
        // genuinamente el tamaño real del prompt).
        if samples.len() < 2 {
            continue;
        }

        let min_bytes = samples.iter().map(|(_, b)| *b).min().expect("samples no está vacío (len >= 2)");
        let max_bytes = samples.iter().map(|(_, b)| *b).max().expect("samples no está vacío (len >= 2)");
        if max_bytes == 0 {
            continue;
        }

        let relative_diff = (max_bytes - min_bytes) as f64 / max_bytes as f64;
        if relative_diff >= TRUNCATION_BYTES_DELTA {
            for &(i, _) in samples {
                result[i].push(OutlierKind::Truncated);
            }
        }
    }
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
// Panel "tools por servidor" (tecla `s`) — funciones puras, testeables sin
// terminal ni HTTP de por medio
// ---------------------------------------------------------------------------

/// Encuentra la fila MÁS RECIENTE de `rows` cuyo `tools_by_server` sea
/// `Some` y no vacío. `rows` llega en orden cronológico (más viejo primero,
/// igual que el buffer del proxy — ver `RecentRequests::snapshot`), así que
/// se recorre desde el final hacia el principio.
///
/// Una fila con `tools_by_server: Some(vec![])` NO califica: declara
/// explícitamente que esa petición puntual no tenía herramientas, y usarla
/// como "la fuente" del panel confundiría "sin tools en ESTA request" con
/// "sin dato en absoluto". Se sigue buscando hacia atrás hasta encontrar una
/// fila con datos reales, o se agota el buffer y se devuelve `None`.
fn find_tools_source_row(rows: &[RequestRow]) -> Option<&RequestRow> {
    rows.iter().rev().find(|r| r.tools_by_server.as_ref().is_some_and(|v| !v.is_empty()))
}

/// Fila de un servidor ya combinada con su delta contra el baseline (o sin
/// baseline). Resultado de [`diff_against_baseline`]: lo que consumen tanto
/// la TUI (`draw_tools_panel`) como `--once` (`print_tools_table`) para
/// pintar la columna `Δ baseline`.
#[derive(Debug, Clone, PartialEq)]
struct ServerDiffRow {
    server: String,
    /// `"-"` para un servidor que existía en el baseline pero desapareció
    /// ahora: no hay ninguna fila [`ToolServerRow`] viva de la que sacar su
    /// tipo actual.
    kind: String,
    tools: usize,
    bytes: usize,
    /// Espejo de `ToolServerRow::deferred_tools`: `None` cuando el proxy no
    /// mandó este dato (build anterior al campo), `Some(n)` cuando sí lo
    /// midió. `None` también para las filas SINTÉTICAS de servidores
    /// desaparecidos: no hay datos vivos de qué diferir, y sintetizar un
    /// `Some(0)` inventaría una medición que nunca ocurrió para ese servidor
    /// en `current`.
    deferred_tools: Option<usize>,
    /// `current_bytes - baseline_bytes` para este servidor. `None`
    /// ÚNICAMENTE cuando no hay baseline marcado en absoluto (`baseline` es
    /// `None` completo en [`diff_against_baseline`]). Si el baseline SÍ
    /// existe pero este servidor puntual no estaba en él, el delta es el
    /// valor POSITIVO completo de `bytes` (nunca `None`): apareció después
    /// de marcar el baseline.
    delta: Option<i64>,
}

/// Calcula, por servidor, el delta de bytes contra un baseline capturado con
/// la tecla `b` (ver `App::mark_baseline`). Función PURA: no conoce
/// ratatui, no hace I/O — acá es donde vive la lógica más propensa a bugs
/// sutiles de todo este panel, por eso se testea aparte y en profundidad.
///
/// - `baseline: None` (nunca se marcó uno): TODAS las filas de `current` se
///   devuelven con `delta: None`, EN SU MISMO ORDEN ORIGINAL — esta función
///   nunca reordena `current` (el proxy ya lo entrega bytes DESC).
/// - `baseline: Some(_)`: cada servidor de `current` lleva
///   `current_bytes - baseline_bytes` (baseline implícito `0` si el servidor
///   no estaba ahí: apareció después de marcarlo).
/// - Un servidor presente en el BASELINE pero AUSENTE de `current` (el
///   usuario lo desconectó) se agrega como fila SINTÉTICA con `bytes: 0`,
///   `tools: 0`, `kind: "-"` y delta `0 - baseline_bytes` (negativo). Esta es
///   la señal de ÉXITO del flujo `b` → desactivar servidor → reiniciar
///   cliente: un servidor que desaparece del todo tiene que seguir siendo
///   VISIBLE en el panel — una fila que directamente desaparece es
///   indistinguible de "no cambió nada".
///
/// Orden del resultado: primero las filas de `current` en su orden ORIGINAL
/// (nunca reordenadas); después las filas sintéticas de servidores
/// desaparecidos, ordenadas por bytes de baseline DESCENDENTE (el que más
/// pesaba se lista primero — es la fila que más le importa al usuario) y, en
/// empate, por nombre de servidor (para que el orden sea determinístico
/// entre corridas).
fn diff_against_baseline(current: &[ToolServerRow], baseline: Option<&BTreeMap<String, usize>>) -> Vec<ServerDiffRow> {
    let mut result: Vec<ServerDiffRow> = current
        .iter()
        .map(|row| {
            let delta = baseline.map(|b| row.bytes as i64 - *b.get(&row.server).unwrap_or(&0) as i64);
            ServerDiffRow {
                server: row.server.clone(),
                kind: row.kind.clone(),
                tools: row.tools,
                bytes: row.bytes,
                deferred_tools: row.deferred_tools,
                delta,
            }
        })
        .collect();

    if let Some(baseline) = baseline {
        let mut disappeared: Vec<(&String, &usize)> =
            baseline.iter().filter(|(name, _)| !current.iter().any(|r| &r.server == *name)).collect();
        disappeared.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));

        for (name, bytes) in disappeared {
            result.push(ServerDiffRow {
                server: name.clone(),
                kind: "-".to_string(),
                tools: 0,
                bytes: 0,
                deferred_tools: None,
                delta: Some(-(*bytes as i64)),
            });
        }
    }

    result
}

/// Celda de `% de tools`: `bytes / tools_bytes * 100` con un decimal, o `-`
/// si `tools_bytes` es `None` o `0` (denominador desconocido o indefinido —
/// nunca se imprime `0.0` para un dato que en realidad no se pudo calcular,
/// mismo criterio que [`opt_tax_ratio`]).
fn tool_pct_of_total(bytes: usize, tools_bytes: Option<usize>) -> String {
    match tools_bytes {
        Some(total) if total > 0 => format!("{:.1}", bytes as f64 / total as f64 * 100.0),
        _ => "-".to_string(),
    }
}

/// Celda de `Δ baseline`: signo explícito (`+`/`-`) seguido de
/// [`format_bytes`] del valor absoluto. `-` si no hay baseline marcado
/// (`delta` es `None`). Un delta de exactamente `0` se muestra como `"0 B"`
/// SIN signo: es un dato real (el servidor no cambió), no un hueco.
fn format_delta_bytes(delta: Option<i64>) -> String {
    match delta {
        None => "-".to_string(),
        Some(0) => "0 B".to_string(),
        Some(d) if d < 0 => format!("-{}", format_bytes(d.unsigned_abs() as usize)),
        Some(d) => format!("+{}", format_bytes(d as usize)),
    }
}

/// Celda `deferred`: `"<deferred_tools>/<tools>"` (p. ej. `"3/3"` totalmente
/// diferido, `"0/5"` nada diferido, `"2/5"` diferido parcial — ver
/// `provider::ToolServerBytes::deferred_tools`). `"-"` para las filas
/// sintéticas de servidores desaparecidos (`tools == 0`, ver
/// `diff_against_baseline`): no hay tools vivas de las que mostrar fracción.
///
/// TERCER ESTADO — `"?"`: `d.tools > 0` pero `d.deferred_tools` es `None`
/// (proxy de build anterior a este campo, ver `ToolServerRow::deferred_tools`).
/// NUNCA se muestra `"0/N"` en este caso: `0/N` es una afirmación medida de
/// "nada diferido, bytes reales y desconectables", y usarla para un dato
/// ausente sería exactamente el defecto que este tipo existe para evitar
/// (absent ≠ zero).
fn deferred_cell(d: &ServerDiffRow) -> String {
    if d.tools == 0 {
        "-".to_string()
    } else {
        match d.deferred_tools {
            Some(deferred) => format!("{deferred}/{}", d.tools),
            None => "?".to_string(),
        }
    }
}

/// Celdas de una fila del panel "tools por servidor", en el mismo orden que
/// las columnas documentadas (`servidor`, `kind`, `tools`, `deferred`,
/// `bytes`, `% de tools`, `Δ baseline`). Reusada por la TUI
/// (`draw_tools_panel`) y por `--once` (`print_tools_table`) para que
/// ninguna de las dos diverja en qué muestra cada columna.
fn tools_row_cells(d: &ServerDiffRow, tools_bytes: Option<usize>) -> Vec<String> {
    vec![
        d.server.clone(),
        d.kind.clone(),
        d.tools.to_string(),
        deferred_cell(d),
        format_bytes(d.bytes),
        tool_pct_of_total(d.bytes, tools_bytes),
        format_delta_bytes(d.delta),
    ]
}

// ---------------------------------------------------------------------------
// Panel "cuota codex" (tecla `u`) — funciones puras, testeables sin terminal
// ni HTTP de por medio
// ---------------------------------------------------------------------------

/// Encuentra la fila MÁS RECIENTE de `rows` cuyo `codex_quota` sea `Some`.
/// `rows` llega en orden cronológico (más viejo primero, igual que el buffer
/// del proxy), así que se recorre desde el final. A diferencia de
/// [`find_tools_source_row`], acá `Some(_)` SIEMPRE califica: un
/// `CodexQuotaRow` presente ES el estado de cuota completo, no hay un
/// análogo del vector vacío que distinguir. `None` si ninguna fila trae
/// cuota: todo el tráfico del buffer es no-Codex (Anthropic, Gemini, OpenAI
/// vía API key) o el proxy es anterior a la captura de cuota.
fn find_quota_source_row(rows: &[RequestRow]) -> Option<&RequestRow> {
    rows.iter().rev().find(|r| r.codex_quota.is_some())
}

/// Ancho fijo (en celdas) de la barra de texto de una ventana de cuota
/// (primaria/secundaria). Calibrado contra el ancho real del panel.
const QUOTA_BAR_WIDTH: usize = 14;

/// Barra de texto de bloques llenos (`█`) y vacíos (`·`), proporcional al
/// porcentaje consumido. `percent` se clampa a `0..=100`: una cabecera
/// malformada corriente arriba no debería llegar acá, pero un clamp defensivo
/// es preferible a un `repeat` con overflow.
fn quota_bar(percent: u64) -> String {
    let clamped = percent.min(100) as usize;
    let filled = clamped * QUOTA_BAR_WIDTH / 100;
    format!("{}{}", "█".repeat(filled), "·".repeat(QUOTA_BAR_WIDTH - filled))
}

/// Segundos restantes hasta el reset de la ventana primaria, con la
/// prioridad de fuente documentada en el diseño: `primary_reset_at`
/// (absoluto) primero; si falta, `source_timestamp` (RFC 3339 de la fila
/// fuente) más `primary_reset_after_seconds` reconstruido a instante
/// absoluto. `None` si ninguna de las dos fuentes está disponible, si
/// `source_timestamp` no parsea, o si la aritmética se desborda. `now` se
/// inyecta (no se lee `chrono::Utc::now()` acá) para que la función sea PURA
/// y testeable con un reloj fijo.
///
/// Toda la aritmética usa `checked_*`: las cabeceras `x-codex-*` son datos NO
/// confiables, y un `reset_at` cercano a `i64::MIN`/`MAX` desbordaría una
/// resta directa (panic en debug, wrap silencioso en release). Ante un
/// desbordamiento preferimos `None` —que se renderiza como `"—"`— a un
/// countdown inventado: mismo principio de honestidad que el resto del módulo.
fn quota_reset_remaining(quota: &CodexQuotaRow, source_timestamp: &str, now: i64) -> Option<i64> {
    if let Some(reset_at) = quota.primary_reset_at {
        return reset_at.checked_sub(now);
    }
    let after = quota.primary_reset_after_seconds?;
    let base = chrono::DateTime::parse_from_rfc3339(source_timestamp).ok()?.timestamp();
    base.checked_add(after as i64)?.checked_sub(now)
}

/// Formatea segundos restantes como texto humano de las dos unidades más
/// significativas (`"resetea en 6d 8h"`, `"resetea en 3h 12m"`, `"resetea en
/// 45m"`). `remaining <= 0` ⇒ `"resetea ahora"` (el reset ya pasó o es
/// inminente, nunca un contador negativo). `None` ⇒ `"—"`, sin countdown
/// fabricado.
fn format_reset_countdown(remaining: Option<i64>) -> String {
    let Some(remaining) = remaining else { return "—".to_string() };
    if remaining <= 0 {
        return "resetea ahora".to_string();
    }
    let days = remaining / 86_400;
    let hours = (remaining % 86_400) / 3_600;
    let minutes = (remaining % 3_600) / 60;
    if days > 0 {
        format!("resetea en {days}d {hours}h")
    } else if hours > 0 {
        format!("resetea en {hours}h {minutes}m")
    } else {
        format!("resetea en {minutes}m")
    }
}

/// Construye las líneas de texto del panel de cuota, en el orden de render
/// documentado en el diseño. Reusada por la TUI (`draw_quota_panel`) y por
/// `--once` (`print_quota_table`) para que ninguna de las dos diverja en qué
/// muestra. Regla de honestidad transversal: todo campo ausente se renderiza
/// como `—` o se OMITE por completo — nunca un `0%` ni un valor fabricado.
///
/// - La ventana secundaria se OMITE si `secondary_window_minutes` es
///   `None`/`0`: en el tráfico observado llega vacía, y mostrar `—` para
///   algo que la cuenta ni siquiera define agregaría ruido, no información.
/// - La línea de créditos se OMITE salvo que `credits_has_credits ==
///   Some(true)`.
fn quota_lines(quota: &CodexQuotaRow, source_timestamp: &str, now: i64) -> Vec<String> {
    let mut lines = Vec::new();

    lines.push(format!(
        "plan: {} · límite: {}",
        quota.plan_type.as_deref().unwrap_or("—"),
        quota.active_limit.as_deref().unwrap_or("—"),
    ));

    match quota.primary_used_percent {
        Some(pct) => {
            let window = quota.primary_window_minutes.map(|m| format!("{m}m")).unwrap_or_else(|| "—".to_string());
            lines.push(format!("primaria: {} {pct}% · ventana {window}", quota_bar(pct)));
        }
        None => lines.push("primaria: —".to_string()),
    }

    if quota.secondary_window_minutes.is_some_and(|m| m > 0) {
        match quota.secondary_used_percent {
            Some(pct) => {
                let window = quota.secondary_window_minutes.map(|m| format!("{m}m")).unwrap_or_else(|| "—".to_string());
                lines.push(format!("secundaria: {} {pct}% · ventana {window}", quota_bar(pct)));
            }
            None => lines.push("secundaria: —".to_string()),
        }
    }

    lines.push(format_reset_countdown(quota_reset_remaining(quota, source_timestamp, now)));

    if quota.credits_has_credits == Some(true) {
        if quota.credits_unlimited == Some(true) {
            lines.push("créditos: ilimitados".to_string());
        } else {
            lines.push(format!("créditos: {}", quota.credits_balance.as_deref().unwrap_or("—")));
        }
    }

    lines
}

// ---------------------------------------------------------------------------
// Vista de columnas del panel de requests recientes
// ---------------------------------------------------------------------------

/// Vista activa del panel de requests recientes (tecla `c`, ver [`App`]).
///
/// Las dos vistas son un conjunto de columnas MUTUAMENTE EXCLUYENTE: nunca
/// se combinan en una sola tabla ancha, porque el panel ya tiene ~12
/// columnas en cualquiera de las dos y cramear las de la otra lo haría
/// ilegible. Se modela como enum (no como `bool`) para que agregar una
/// tercera vista el día de mañana no obligue a renombrar un booleano que
/// ya perdió sentido binario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RequestsView {
    /// Columnas de latencia/tokens/coste, más las tres palancas de
    /// VELOCIDAD agregadas en este slice: `effort` (`requested_effort`,
    /// `output_config.effort` pedido), `spd_req` (`requested_speed`, `speed`
    /// pedido a nivel raíz) y `spd_got` (`served_speed`, `usage.speed`
    /// REALMENTE servido — documentado por Anthropic, no observado aún en
    /// tráfico real). Van acá y no en `Context` porque son palancas de
    /// velocidad, igual que `tok/s`/`ttft_ms`, no bytes de contexto. Vista
    /// por defecto.
    #[default]
    Latency,
    /// Columnas del desglose de bytes de contexto (`tools`, `history`,
    /// `system`, `last_turn`, `other`, `total`, `tax%`, `B/tok`, `prep_us`,
    /// `msgs`), más `cliente` (`RequestRow::client`). `B/tok` es
    /// [`bytes_per_token`]: bytes medidos por token de prompt, el
    /// denominador correcto según dialecto de `upstream`. `cliente` va ACÁ y
    /// no en `Latency` porque el caso que motiva es correlacionar un salto
    /// en `tools`/`total` con atribución de cliente (`docs/optimizer-tool-search.md`
    /// §3): un harness que rompe su propio diferido de tools al pasar por un
    /// `ANTHROPIC_BASE_URL` no-first-party es la firma del impuesto de
    /// contexto que el proxy induce por su propia presencia, no una anomalía
    /// sin explicación.
    Context,
}

impl RequestsView {
    /// Cicla a la siguiente vista. Función PURA y TOTAL (cubre ambas
    /// variantes sin rama de error): Latency → Context → Latency.
    fn next(self) -> Self {
        match self {
            RequestsView::Latency => RequestsView::Context,
            RequestsView::Context => RequestsView::Latency,
        }
    }

    /// Etiqueta corta para el título del panel, en minúsculas para
    /// combinar con el resto del texto de estado de la UI.
    fn label(self) -> &'static str {
        match self {
            RequestsView::Latency => "latency",
            RequestsView::Context => "context",
        }
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
    /// Foto de `tools_by_server` (servidor → bytes) de la fila fuente del
    /// panel de tools por servidor (ver [`find_tools_source_row`]) vigente
    /// en el instante en que se marcó el baseline. `None` si en ese momento
    /// no había ninguna fila fuente disponible (proxy viejo, o ninguna
    /// petición reciente declaraba tools todavía) — no hay nada que
    /// fotografiar, así que el panel de tools queda sin baseline hasta que
    /// se vuelva a marcar con datos disponibles.
    tools_by_server: Option<BTreeMap<String, usize>>,
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
    /// Vista de columnas del panel de requests recientes, ciclable con `c`.
    /// Ver [`RequestsView`] y [`App::cycle_requests_view`] para el
    /// contrato de qué pasa cuando el panel está oculto.
    requests_view: RequestsView,
    /// Visibilidad del panel de "tools por servidor", toggleable con `s`.
    /// INDEPENDIENTE de `show_requests_panel` y de `requests_view`: las tres
    /// teclas (`p`, `c`, `s`) controlan estados ortogonales entre sí.
    show_tools_panel: bool,
    /// Visibilidad del panel de cuota de suscripción Codex, toggleable con
    /// `u`. INDEPENDIENTE de `p`/`c`/`s`: las cuatro teclas controlan
    /// estados ortogonales entre sí.
    show_quota_panel: bool,
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
            requests_view: RequestsView::Latency,
            show_tools_panel: true,
            show_quota_panel: true,
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

    /// Cicla la vista de columnas del panel de requests recientes (tecla
    /// `c`). Es un NO-OP si el panel está oculto (`show_requests_panel ==
    /// false`): cambiar qué columnas se muestran en algo que no se está
    /// mostrando sería un cambio de estado invisible para el usuario hasta
    /// que vuelva a mostrar el panel con `p` — mejor no mutar nada que
    /// mutar en silencio.
    fn cycle_requests_view(&mut self) {
        if self.show_requests_panel {
            self.requests_view = self.requests_view.next();
        }
    }

    /// Alterna la visibilidad del panel de "tools por servidor" (tecla `s`).
    /// INDEPENDIENTE de [`Self::toggle_requests_panel`]: apagar/prender uno
    /// no toca el estado del otro.
    fn toggle_tools_panel(&mut self) {
        self.show_tools_panel = !self.show_tools_panel;
    }

    /// Alterna la visibilidad del panel de cuota de suscripción Codex (tecla
    /// `u`). INDEPENDIENTE de los demás toggles: apagar/prender uno no toca
    /// el estado de los otros.
    fn toggle_quota_panel(&mut self) {
        self.show_quota_panel = !self.show_quota_panel;
    }

    /// Marca el baseline en el instante actual con los contadores crudos de
    /// cada modelo visible ahora mismo, Y TAMBIÉN con una foto de
    /// `tools_by_server` (servidor → bytes) de la fila fuente vigente del
    /// panel de tools por servidor (ver [`find_tools_source_row`]). Esta
    /// segunda foto es lo que permite calcular `Δ baseline` en ese panel
    /// (ver [`diff_against_baseline`]); si no hay fila fuente disponible en
    /// este instante, queda en `None` sin impedir que el resto del baseline
    /// (los contadores de `/stats`) se marque igual.
    fn mark_baseline(&mut self) {
        let mut by_key = HashMap::new();
        for r in &self.latest {
            by_key.insert(key_of(r), RawCounters::from_row(r));
        }

        let tools_by_server = find_tools_source_row(&self.recent_requests).and_then(|r| r.tools_by_server.as_ref()).map(
            |servers| servers.iter().map(|s| (s.server.clone(), s.bytes)).collect::<BTreeMap<_, _>>(),
        );

        self.baseline = Some(Baseline { at: Instant::now(), by_key, tools_by_server });
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
                    KeyCode::Char('c') => app.cycle_requests_view(),
                    KeyCode::Char('s') => app.toggle_tools_panel(),
                    KeyCode::Char('u') => app.toggle_quota_panel(),
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

/// Arma el layout vertical y despacha cada panel a su `chunk`.
///
/// Tres paneles son toggleables de forma INDEPENDIENTE (`p` para requests
/// recientes, `s` para tools por servidor, `u` para cuota Codex): cuando uno
/// está oculto, no se reserva su espacio, para que los paneles fijos no se
/// vean apretados sin necesidad. Eso da OCHO combinaciones de visibilidad
/// posibles.
///
/// Para que las ocho queden cubiertas sin lógica especial por caso (y sin el
/// riesgo de indexar un `chunks[i]` que no exista si algún día se agrega un
/// cuarto panel toggleable), el índice de cada chunk se calcula avanzando un
/// contador (`idx`) a medida que cada panel opcional se agrega a
/// `constraints` y se dibuja — nunca se hardcodea una posición fija. La
/// longitud de `chunks` es SIEMPRE igual a la de `constraints`
/// (`Layout::split` lo garantiza), así que `idx` nunca puede quedar fuera de
/// rango mientras el código que empuja a `constraints` y el que incrementa
/// `idx` avancen en el mismo orden — que es exactamente lo que hace esta
/// función.
fn ui(f: &mut Frame, app: &App) {
    let area = f.area();
    let mut constraints = vec![
        Constraint::Length(3), // header
        Constraint::Min(5),    // tabla principal
        Constraint::Length(6), // panel antes/después
        Constraint::Length(7), // sparklines
    ];
    if app.show_requests_panel {
        constraints.push(Constraint::Length(12)); // requests recientes + leyenda
    }
    if app.show_tools_panel {
        constraints.push(Constraint::Length(10)); // tools por servidor
    }
    if app.show_quota_panel {
        constraints.push(Constraint::Length(7)); // cuota codex
    }
    constraints.push(Constraint::Length(1)); // footer

    let chunks = Layout::default().direction(Direction::Vertical).constraints(constraints).split(area);

    draw_header(f, chunks[0], app);
    draw_table(f, chunks[1], app);
    draw_before_after(f, chunks[2], app);
    draw_sparklines(f, chunks[3], app);

    let mut idx = 4;
    if app.show_requests_panel {
        draw_requests_panel(f, chunks[idx], app);
        idx += 1;
    }
    if app.show_tools_panel {
        draw_tools_panel(f, chunks[idx], app);
        idx += 1;
    }
    if app.show_quota_panel {
        draw_quota_panel(f, chunks[idx], app);
        idx += 1;
    }
    draw_footer(f, chunks[idx]);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let baseline_age = match &app.baseline {
        Some(b) => format!("baseline hace {}s", b.at.elapsed().as_secs()),
        None => "sin baseline — pulse 'b'".to_string(),
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
        (Some(_), None) => vec![Line::from("sin baseline (o el modelo no existía al marcarlo) — pulse 'b'")],
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

    let block = Block::default().borders(Borders::ALL).title(format!(
        " requests recientes · vista:{} · {} ",
        app.requests_view.label(),
        app.requests_status
    ));
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

        let header = requests_table_header(app.requests_view);

        let rows: Vec<Row> = indexed
            .iter()
            .map(|(i, r)| {
                let kinds = &outliers[*i];
                let mut cells = requests_row_cells(app.requests_view, r);
                cells.push(marker_text(kinds));
                let row = Row::new(cells);
                if kinds.is_empty() {
                    row
                } else {
                    row.style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
                }
            })
            .collect();

        let widths = requests_table_widths(app.requests_view);

        // Un terminal angosto no alcanza a mostrar todas las columnas del
        // ancho declarado (la vista Context es más ancha que Latency):
        // `ratatui::Table` recorta las columnas que no entran en vez de
        // hacer wrap o panickear, así que no hace falta guard adicional acá
        // más allá de los chequeos de área ya hechos arriba.
        f.render_widget(Table::new(rows, widths).header(header), table_area);
    }

    if legend_area.height > 0 {
        let legend = Paragraph::new(Line::from(
            "leyenda: ERR=error(status>=400) · MISS=cache-miss atípico · TTFT=TTFT lento(>=2σ) · SLOW=generación lenta(>=2σ) · TRUNC=tope de tokens (ver docs)",
        ));
        f.render_widget(legend, legend_area);
    }
}

/// Panel de "tools por servidor" (tecla `s`), INDEPENDIENTE del panel de
/// requests recientes (`p`/`c`): ambos se muestran u ocultan por separado y
/// ninguno de los dos afecta el estado del otro.
///
/// Fuente de datos: la fila MÁS RECIENTE de `app.recent_requests` cuyo
/// `tools_by_server` sea `Some` y no vacío — ver [`find_tools_source_row`].
/// Si ninguna fila califica (proxy anterior a este campo, o ninguna
/// petición reciente declaró tools todavía), se muestra una única línea
/// explicativa; nunca una caja vacía ni un panic.
///
/// El delta contra el baseline (columna `Δ baseline`) sale de
/// [`diff_against_baseline`], función PURA testeada aparte: acá solo se
/// formatea su resultado vía [`tools_row_cells`].
fn draw_tools_panel(f: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let Some(source) = find_tools_source_row(&app.recent_requests) else {
        let block = Block::default().borders(Borders::ALL).title(" tools por servidor ");
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height > 0 && inner.width > 0 {
            let text = Line::from(
                "sin desglose de tools todavía (proxy anterior a este slice, o ninguna petición reciente declara tools)",
            );
            f.render_widget(Paragraph::new(text), inner);
        }
        return;
    };

    let block = Block::default().borders(Borders::ALL).title(format!(
        " tools por servidor · fuente {} {} ",
        format_time(&source.timestamp),
        source.model.as_deref().unwrap_or("-"),
    ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // `find_tools_source_row` garantiza `Some` no vacío: este `expect` nunca
    // debería fallar, pero preferimos documentarlo explícitamente en vez de
    // un `unwrap()` mudo.
    let servers = source.tools_by_server.as_ref().expect("find_tools_source_row garantiza tools_by_server Some no vacío");
    let baseline_map = app.baseline.as_ref().and_then(|b| b.tools_by_server.as_ref());
    let diffs = diff_against_baseline(servers, baseline_map);

    let header = Row::new(vec!["servidor", "kind", "tools", "deferred", "bytes", "% tools", "Δ baseline"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let mut rows: Vec<Row> = diffs.iter().map(|d| Row::new(tools_row_cells(d, source.context_tools_bytes))).collect();

    // Separador visual antes de las filas de resumen: distingue "detalle por
    // servidor" de "totales de la petición completa".
    rows.push(Row::new(vec!["·".repeat(8); 7]));

    rows.push(Row::new(vec![
        "overhead".to_string(),
        "-".to_string(),
        "-".to_string(),
        "-".to_string(),
        opt_bytes(source.tools_overhead_bytes),
        "-".to_string(),
        "-".to_string(),
    ]));

    // El delta TOTAL es la cifra que responde "¿cuánto bajé en total?": solo
    // tiene sentido si HAY baseline marcado, y se calcula sumando los deltas
    // ya resueltos por servidor (que a su vez ya incluyen a los
    // desaparecidos con su delta negativo completo).
    let total_delta = baseline_map.map(|_| diffs.iter().map(|d| d.delta.unwrap_or(0)).sum::<i64>());
    rows.push(
        Row::new(vec![
            "TOTAL".to_string(),
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
            opt_bytes(source.context_tools_bytes),
            "-".to_string(),
            format_delta_bytes(total_delta),
        ])
        .style(Style::default().add_modifier(Modifier::BOLD)),
    );

    let widths = [
        Constraint::Length(26),
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Length(9),
        Constraint::Length(10),
        Constraint::Length(9),
        Constraint::Length(12),
    ];

    // Si hay más filas (servidores + separador + overhead + TOTAL) que
    // espacio vertical disponible, `ratatui::Table` recorta las que no
    // entran sin panickear — mismo comportamiento (documentado) que ya usa
    // `draw_requests_panel` para columnas angostas.
    f.render_widget(Table::new(rows, widths).header(header), inner);
}

/// Panel de cuota de suscripción Codex (tecla `u`), alimentado por la fila
/// MÁS RECIENTE de `app.recent_requests` cuyo `codex_quota` sea `Some` — ver
/// [`find_quota_source_row`]. Un `Paragraph` con borde, no una `Table`: la
/// cuota es un gauge de líneas de cuenta, no filas por petición (mismo
/// widget base que [`draw_before_after`]).
///
/// Si ninguna fila califica (todo el buffer es tráfico no-Codex, o el proxy
/// es anterior a la captura de cuota), se muestra una única línea
/// explicativa dentro del borde; nunca una caja vacía ni un gauge fabricado
/// en 0%.
fn draw_quota_panel(f: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let Some(source) = find_quota_source_row(&app.recent_requests) else {
        let block = Block::default().borders(Borders::ALL).title(" cuota codex ");
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height > 0 && inner.width > 0 {
            let text = Line::from(
                "sin datos de cuota (ninguna petición reciente usó el backend de Codex, o el proxy es anterior a la captura de cuota)",
            );
            f.render_widget(Paragraph::new(text), inner);
        }
        return;
    };

    // `find_quota_source_row` garantiza `codex_quota` Some: este `expect`
    // nunca debería fallar, pero preferimos documentarlo explícitamente en
    // vez de un `unwrap()` mudo.
    let quota = source.codex_quota.as_ref().expect("find_quota_source_row garantiza codex_quota Some");
    let now = chrono::Utc::now().timestamp();
    let lines: Vec<Line> = quota_lines(quota, &source.timestamp, now).into_iter().map(Line::from).collect();

    let block = Block::default().borders(Borders::ALL).title(format!(
        " cuota codex · fuente {} {} ",
        format_time(&source.timestamp),
        source.model.as_deref().unwrap_or("-"),
    ));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Header de columnas del panel/tabla de requests, según la vista activa.
/// Ver [`RequestsView`] para el contrato de qué columnas trae cada una.
fn requests_table_header<'a>(view: RequestsView) -> Row<'a> {
    let labels: Vec<&'a str> = match view {
        RequestsView::Latency => {
            vec![
                "hora", "modelo", "st", "status", "in", "out", "c_rd", "c_wr", "ttft_ms", "gen_ms", "tok/s", "effort", "spd_req",
                "spd_got", "usd", "outlier",
            ]
        }
        RequestsView::Context => {
            vec![
                "hora", "modelo", "msgs", "tools", "history", "system", "last_turn", "other", "total", "tax%", "B/tok", "prep_us",
                "cliente", "outlier",
            ]
        }
    };
    Row::new(labels).style(Style::default().add_modifier(Modifier::BOLD))
}

/// Anchos de columna del panel/tabla de requests, según la vista activa.
/// La vista Context es más ancha en total que Latency (más columnas de
/// bytes con nombres largos) — ver el comentario sobre truncado en
/// [`draw_requests_panel`].
fn requests_table_widths(view: RequestsView) -> Vec<Constraint> {
    match view {
        RequestsView::Latency => vec![
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
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(14),
        ],
        RequestsView::Context => vec![
            Constraint::Length(9),
            Constraint::Length(16),
            Constraint::Length(5),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(18),
            Constraint::Length(14),
        ],
    }
}

/// Celdas de datos de una fila (SIN el marcador de outlier, que el llamador
/// agrega al final: es común a ambas vistas y se calcula una sola vez por
/// fila en [`draw_requests_panel`] / [`print_requests_table`] /
/// [`print_context_table`]), según la vista activa.
fn requests_row_cells(view: RequestsView, r: &RequestRow) -> Vec<String> {
    match view {
        RequestsView::Latency => vec![
            format_time(&r.timestamp),
            truncate_model(r.model.as_deref()),
            if r.stream { "y" } else { "n" }.to_string(),
            r.status.to_string(),
            opt_u64(r.input_tokens),
            opt_u64(r.output_tokens),
            opt_u64(r.cache_read_tokens),
            opt_u64(r.cache_write_tokens),
            opt_fixed(r.ttft_ms, 1),
            opt_fixed(gen_ms_of(r), 1),
            tokens_per_sec_cell(r),
            opt_str_short(r.requested_effort.as_deref()),
            opt_str_short(r.requested_speed.as_deref()),
            opt_str_short(r.served_speed.as_deref()),
            opt_fixed(r.cost_estimate_usd, 4),
        ],
        RequestsView::Context => vec![
            format_time(&r.timestamp),
            truncate_model(r.model.as_deref()),
            opt_usize(r.context_messages_count),
            opt_bytes(r.context_tools_bytes),
            opt_bytes(r.context_history_bytes),
            opt_bytes(r.context_system_bytes),
            opt_bytes(r.context_last_turn_bytes),
            opt_bytes(r.context_other_bytes),
            opt_bytes(r.context_measured_bytes),
            opt_tax_ratio(r.context_tax_ratio),
            opt_fixed(bytes_per_token(r), 1),
            opt_u64(r.prepare_us),
            truncate_client(r.client.as_deref()),
        ],
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

/// Máximo de caracteres para el `User-Agent` en la columna `cliente`, para no
/// romper el ancho fijo de columna. A diferencia de [`truncate_model`], acá
/// no hay clasificación posible: el string es crudo y NUNCA se reinterpreta
/// (p. ej. no se intenta mapear a "Claude Code" / "OpenCode" a partir del
/// prefijo) — un cliente no reconocido se ve truncado, no adivinado.
const CLIENT_DISPLAY_MAX: usize = 18;

/// Trunca el `User-Agent` crudo a [`CLIENT_DISPLAY_MAX`] caracteres. `None`
/// se muestra como `-`, nunca como string vacío — mismo criterio que
/// [`truncate_model`], del que este helper es un duplicado deliberado (columna
/// distinta, ancho distinto, mismo patrón de truncado).
fn truncate_client(client: Option<&str>) -> String {
    match client {
        None => "-".to_string(),
        Some(c) if c.chars().count() <= CLIENT_DISPLAY_MAX => c.to_string(),
        Some(c) => {
            let head: String = c.chars().take(CLIENT_DISPLAY_MAX.saturating_sub(1)).collect();
            format!("{head}…")
        }
    }
}

/// `None` se renderiza como `-`, NUNCA como `0`: un `0` real (p. ej. 0 tokens
/// de caché) y un dato ausente son cosas distintas para el usuario.
fn opt_u64(v: Option<u64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "-".to_string())
}

/// Igual que [`opt_u64`] pero para `usize` (usado en `msgs`, la cantidad de
/// mensajes del historial). `None` se muestra como `-`, nunca como `0`.
fn opt_usize(v: Option<usize>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "-".to_string())
}

/// Máximo de caracteres para las celdas cortas de esfuerzo/velocidad
/// (`effort`, `spd_req`, `spd_got`). Los valores documentados hoy son todos
/// cortos (`low`|`medium`|`high`|`xhigh`|`max`; `fast`), pero se trunca de
/// todos modos para no romper el ancho fijo de columna si un proveedor
/// futuro manda algo más largo.
const SPEED_DISPLAY_MAX: usize = 8;

/// Celda corta para `effort`/`spd_req`/`spd_got`: `None` se muestra como
/// `-` (NUNCA string vacío, mismo criterio que el resto de los `opt_*`),
/// truncando valores más largos que [`SPEED_DISPLAY_MAX`] con `…` — mismo
/// patrón que [`truncate_model`].
fn opt_str_short(v: Option<&str>) -> String {
    match v {
        None => "-".to_string(),
        Some(s) if s.chars().count() <= SPEED_DISPLAY_MAX => s.to_string(),
        Some(s) => {
            let head: String = s.chars().take(SPEED_DISPLAY_MAX.saturating_sub(1)).collect();
            format!("{head}…")
        }
    }
}

/// Convierte un tamaño en bytes a una representación compacta y legible.
///
/// Convención elegida: DECIMAL (base 1000), no binaria (KiB/MiB base 1024).
/// `1_000 B = 1.0 kB`, `1_000_000 B = 1.0 MB`. Se prefiere la convención
/// decimal porque estos bytes miden el tamaño de un JSON canónico
/// re-serializado (ver `ContextBreakdown` en `src/telemetry/recent.rs`), no
/// bloques de memoria alineados a potencias de 2 — no hay ninguna razón
/// binaria de por medio, y la convención decimal es la que usan la mayoría
/// de las herramientas de red/observabilidad con las que se compara este
/// dato (curl, nginx, etc.).
///
/// Umbrales:
/// - `< 1_000` bytes → se muestra tal cual, sin decimales (`"281 B"`).
/// - hasta `999.9 kB` → kB con un decimal (`"159.1 kB"`).
/// - a partir de ahí → MB con un decimal (`"1.0 MB"`).
///
/// El salto a MB se decide DESPUÉS de redondear, no antes. Elegir la unidad
/// comparando contra `1_000_000` y redondear luego produce `"1000.0 kB"` para
/// cualquier valor entre `999_950` y `999_999`: un número que se lee como un
/// error de escala, no como un redondeo. Por eso el corte está en `999.95 kB`,
/// que es exactamente donde el formato de un decimal empezaría a mostrar
/// `1000.0`.
fn format_bytes(bytes: usize) -> String {
    if bytes < 1_000 {
        return format!("{bytes} B");
    }

    let kb = bytes as f64 / 1_000.0;
    if kb < 999.95 {
        return format!("{kb:.1} kB");
    }

    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

/// Igual que [`opt_u64`] pero aplicando [`format_bytes`] al valor presente.
/// `None` se muestra como `-`, nunca como `"0 B"`: un tamaño no medido y un
/// tamaño de cero bytes real son cosas distintas.
fn opt_bytes(v: Option<usize>) -> String {
    v.map(format_bytes).unwrap_or_else(|| "-".to_string())
}

/// Celda de `tax%`: `context_tax_ratio * 100` con un decimal, o `-` si no
/// hay dato. Mismo criterio que [`opt_fixed`] para valores no finitos
/// (NaN/inf se tratan como ausentes, nunca se imprimen tal cual).
fn opt_tax_ratio(v: Option<f64>) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{:.1}", x * 100.0),
        _ => "-".to_string(),
    }
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

/// Imprime la tabla LATENCY de requests recientes en texto plano (modo
/// `--once`), más nuevo arriba, con los mismos marcadores de outlier que la
/// TUI. Es la vista por defecto (columnas de latencia/tokens/coste, más
/// `effort`/`spd_req`/`spd_got` desde este slice). Reusa
/// [`requests_row_cells`] (mismo patrón que [`print_context_table`]) para que
/// esta vista en texto plano y la vista `Latency` de la TUI
/// (`draw_requests_panel`) nunca diverjan en qué dato muestra cada columna.
/// Ver [`print_context_table`] para la vista complementaria del desglose de
/// contexto — `--once` imprime AMBAS, una debajo de la otra (ver
/// [`run_once`]).
fn print_requests_table(rows: &[RequestRow]) {
    let outliers = classify_outliers(rows);

    println!(
        "{:<10} {:<16} {:>2} {:>6} {:>6} {:>6} {:>6} {:>6} {:>8} {:>8} {:>7} {:>7} {:>8} {:>8} {:>8} {:<14}",
        "HORA", "MODELO", "st", "status", "in", "out", "c_rd", "c_wr", "ttft_ms", "gen_ms", "tok/s", "effort", "spd_req", "spd_got",
        "usd", "outlier"
    );
    for (i, r) in rows.iter().enumerate().rev() {
        let cells = requests_row_cells(RequestsView::Latency, r);
        println!(
            "{:<10} {:<16} {:>2} {:>6} {:>6} {:>6} {:>6} {:>6} {:>8} {:>8} {:>7} {:>7} {:>8} {:>8} {:>8} {:<14}",
            cells[0],
            cells[1],
            cells[2],
            cells[3],
            cells[4],
            cells[5],
            cells[6],
            cells[7],
            cells[8],
            cells[9],
            cells[10],
            cells[11],
            cells[12],
            cells[13],
            cells[14],
            marker_text(&outliers[i]),
        );
    }
    println!(
        "leyenda: ERR=error(status>=400) · MISS=cache-miss atípico · TTFT=TTFT lento(>=2σ) · SLOW=generación lenta(>=2σ) · TRUNC=tope de tokens (ver docs/monitor-tui.md)"
    );
    println!(
        "nota: effort = output_config.effort pedido; spd_req = speed pedido (raíz); spd_got = usage.speed servido (Anthropic; documentado pero no observado aún en tráfico real)"
    );
}

/// Imprime la tabla CONTEXT de requests recientes en texto plano (modo
/// `--once`): mismo orden (más nuevo arriba) y mismos marcadores de outlier
/// que [`print_requests_table`], pero con las columnas del desglose de
/// bytes de contexto en vez de las de latencia/tokens. Reusa
/// [`requests_row_cells`] para que esta vista en texto plano y la vista
/// `Context` de la TUI (`draw_requests_panel`) nunca diverjan en qué dato
/// muestra cada columna.
fn print_context_table(rows: &[RequestRow]) {
    let outliers = classify_outliers(rows);

    println!(
        "{:<10} {:<16} {:>5} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>6} {:>6} {:>8} {:<18} {:<14}",
        "HORA", "MODELO", "msgs", "tools", "history", "system", "last_turn", "other", "total", "tax%", "B/tok", "prep_us", "cliente",
        "outlier"
    );
    for (i, r) in rows.iter().enumerate().rev() {
        let cells = requests_row_cells(RequestsView::Context, r);
        println!(
            "{:<10} {:<16} {:>5} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>6} {:>6} {:>8} {:<18} {:<14}",
            cells[0],
            cells[1],
            cells[2],
            cells[3],
            cells[4],
            cells[5],
            cells[6],
            cells[7],
            cells[8],
            cells[9],
            cells[10],
            cells[11],
            cells[12],
            marker_text(&outliers[i]),
        );
    }
    println!(
        "leyenda: ERR=error(status>=400) · MISS=cache-miss atípico · TTFT=TTFT lento(>=2σ) · SLOW=generación lenta(>=2σ) · TRUNC=tope de tokens (ver abajo)"
    );
    println!(
        "nota: tools/history/system/last_turn/other/total son BYTES (kB decimal, no tokens); tax% = (system+tools+history)/total"
    );
    println!(
        "nota: B/tok = total_bytes / prompt_tokens_total (denominador según dialecto, ver docs/monitor-tui.md §7.3.1); TRUNC = mismo total de tokens que otra fila con bodies que difieren >= 5% (tope de contexto probado, no estadística)"
    );
    println!(
        "nota: cliente = User-Agent crudo (truncado, ver docs/telemetry-per-request.md)"
    );
}

/// Imprime la tabla de "tools por servidor" en texto plano (modo `--once`).
/// Mismo pipeline que la TUI (`find_tools_source_row` +
/// `diff_against_baseline` + `tools_row_cells`), para que ninguna de las dos
/// vistas diverja en qué calcula o muestra. En `--once` NUNCA hay baseline
/// marcado (no hay sesión interactiva en la que apretar `b`), así que la
/// columna `Δ baseline` sale siempre `-` — se documenta explícitamente en la
/// salida para que no se lea como un bug.
fn print_tools_table(rows: &[RequestRow]) {
    println!("--- vista: tools por servidor ---");

    let Some(source) = find_tools_source_row(rows) else {
        println!("(sin desglose de tools disponible: proxy anterior a este slice, o ninguna fila declara tools)");
        return;
    };

    println!("fuente: {} · modelo {}", format_time(&source.timestamp), source.model.as_deref().unwrap_or("-"));

    // `find_tools_source_row` garantiza `Some` no vacío.
    let servers = source.tools_by_server.as_ref().expect("find_tools_source_row garantiza tools_by_server Some no vacío");
    let diffs = diff_against_baseline(servers, None);

    println!(
        "{:<26} {:<7} {:>6} {:>9} {:>10} {:>9} {:>12}",
        "SERVIDOR", "KIND", "TOOLS", "DEFERRED", "BYTES", "% tools", "Δ baseline"
    );
    for d in &diffs {
        let cells = tools_row_cells(d, source.context_tools_bytes);
        println!(
            "{:<26} {:<7} {:>6} {:>9} {:>10} {:>9} {:>12}",
            cells[0], cells[1], cells[2], cells[3], cells[4], cells[5], cells[6]
        );
    }
    println!("{:-<26} {:-<7} {:-<6} {:-<9} {:-<10} {:-<9} {:-<12}", "", "", "", "", "", "", "");
    println!(
        "{:<26} {:<7} {:>6} {:>9} {:>10} {:>9} {:>12}",
        "overhead",
        "-",
        "-",
        "-",
        opt_bytes(source.tools_overhead_bytes),
        "-",
        "-"
    );
    println!(
        "{:<26} {:<7} {:>6} {:>9} {:>10} {:>9} {:>12}",
        "TOTAL",
        "-",
        "-",
        "-",
        opt_bytes(source.context_tools_bytes),
        "-",
        "-"
    );
    println!(
        "nota: sum(servidores) + overhead == bytes (array `tools`: brackets/comas, wrapper de Gemini, herramientas huérfanas)"
    );
    println!(
        "nota: deferred = deferred_tools/tools por servidor (ver docs/optimizer-tool-search.md) — 0/N: nada diferido, bytes reales y desconectables; N/N: totalmente diferido; en el medio: parcial; \"?\": el proxy no midió este dato (build anterior a este campo) — dato AUSENTE, no confundir con 0/N medido"
    );
}

/// Imprime el panel de cuota de suscripción Codex en texto plano (modo
/// `--once`). Mismo pipeline puro que la TUI (`find_quota_source_row` +
/// `quota_lines`), para que ninguna de las dos vistas diverja en qué
/// muestra. El countdown usa el mismo `Utc::now()` que la TUI.
fn print_quota_table(rows: &[RequestRow]) {
    println!("--- vista: cuota codex ---");

    let Some(source) = find_quota_source_row(rows) else {
        println!("(sin datos de cuota: ninguna petición reciente usó el backend de Codex, o el proxy es anterior a la captura de cuota)");
        return;
    };

    // `find_quota_source_row` garantiza `codex_quota` Some.
    let quota = source.codex_quota.as_ref().expect("find_quota_source_row garantiza codex_quota Some");
    println!("fuente: {} · modelo {}", format_time(&source.timestamp), source.model.as_deref().unwrap_or("-"));
    let now = chrono::Utc::now().timestamp();
    for line in quota_lines(quota, &source.timestamp, now) {
        println!("{line}");
    }
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let text = Line::from(
        "q salir · b marcar baseline · r reset · ↑/↓ elegir modelo · p requests · c vista latency/context · s tools por servidor · u cuota codex",
    );
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
            client: Some("claude-cli/2.1.207 (external, sdk-cli)".to_string()),
            status,
            input_tokens: Some(100),
            output_tokens,
            cache_read_tokens,
            cache_write_tokens: Some(0),
            cost_estimate_usd: Some(0.01),
            cache_control_forced: false,
            requested_effort: None,
            requested_speed: None,
            served_speed: None,
            ttft_ms,
            total_ms,
            context_system_bytes: Some(281),
            context_tools_bytes: Some(159_100),
            context_history_bytes: Some(4_000),
            context_last_turn_bytes: Some(96),
            context_other_bytes: Some(50),
            context_measured_bytes: Some(163_527),
            context_messages_count: Some(12),
            context_tax_ratio: Some(0.9994),
            prepare_us: Some(850),
            tools_by_server: None,
            tools_overhead_bytes: None,
            codex_quota: None,
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

    // -----------------------------------------------------------------
    // prompt_tokens_total / bytes_per_token — denominador dependiente del
    // dialecto de contabilidad de caché de cada proveedor
    // -----------------------------------------------------------------

    /// Variante de `req` (arriba) que permite fijar los campos relevantes
    /// para [`prompt_tokens_total`], [`bytes_per_token`] y
    /// [`classify_truncation`]: `input_tokens`, `cache_read_tokens`,
    /// `cache_write_tokens` y `context_measured_bytes`. El resto de los
    /// campos quedan en los valores neutros de `req`.
    fn req_prompt(
        upstream: &str,
        model: &str,
        input_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        cache_write_tokens: Option<u64>,
        context_measured_bytes: Option<usize>,
    ) -> RequestRow {
        let mut r = req(upstream, model, 200, Some(10.0), 100.0, Some(50), cache_read_tokens);
        r.input_tokens = input_tokens;
        r.cache_write_tokens = cache_write_tokens;
        r.context_measured_bytes = context_measured_bytes;
        r
    }

    #[test]
    fn prompt_tokens_total_anthropic_suma_cache_read_y_write() {
        // Caso real: cache-hit grande, input_tokens irrisorio. Sumar la
        // caché es OBLIGATORIO o el denominador queda absurdo.
        let r = req_prompt("anthropic", "claude-opus-4", Some(2), Some(124_733), Some(1_355), Some(224_653));
        assert_eq!(prompt_tokens_total(&r), Some(2 + 124_733 + 1_355));
    }

    #[test]
    fn prompt_tokens_total_no_anthropic_ignora_cache_read_por_ser_subconjunto() {
        // OpenAI/Gemini: cache_read ya es SUBCONJUNTO de input_tokens.
        // Sumarlo encima sería doble conteo.
        let r = req_prompt("openai", "gpt-4o", Some(1000), Some(400), None, Some(50_000));
        assert_eq!(prompt_tokens_total(&r), Some(1000));
    }

    #[test]
    fn prompt_tokens_total_none_si_falta_input_tokens() {
        let r = req_prompt("anthropic", "claude-opus-4", None, Some(100), Some(0), Some(1_000));
        assert_eq!(prompt_tokens_total(&r), None);
    }

    #[test]
    fn bytes_per_token_anthropic_usa_la_suma_no_solo_input_tokens() {
        // input_tokens=2 solo daría 224_653/2=112_326.5 B/tok, un número que
        // gritaría "truncamiento" en el request MÁS SANO posible (cache-hit
        // grande, 200 OK). La suma da ~1.8, el valor real observado para
        // Anthropic con caché — MUY por debajo de un input_tokens-only.
        let r = req_prompt("anthropic", "claude-opus-4", Some(2), Some(124_733), Some(1_355), Some(224_653));
        let b = bytes_per_token(&r).expect("debe calcularse con todos los datos presentes");
        let expected = 224_653.0 / (2.0 + 124_733.0 + 1_355.0);
        assert!((b - expected).abs() < 1e-6, "b={b} expected={expected}");
        assert!((b - 1.8).abs() < 0.05, "b={b} debe rondar ~1.8, no 112_326 (input_tokens solo)");
    }

    #[test]
    fn bytes_per_token_openai_con_cache_read_no_dobla_el_conteo() {
        let r = req_prompt("openai", "gpt-4o", Some(1000), Some(400), None, Some(2_700));
        let b = bytes_per_token(&r).expect("debe calcularse");
        assert!((b - 2.7).abs() < 1e-9, "b={b} debe usar input_tokens=1000 solo, no 1000+400");
    }

    #[test]
    fn bytes_per_token_none_si_falta_input_tokens() {
        let r = req_prompt("anthropic", "claude-opus-4", None, Some(10), Some(0), Some(1_000));
        assert_eq!(bytes_per_token(&r), None);
    }

    #[test]
    fn bytes_per_token_none_si_falta_context_measured_bytes() {
        let r = req_prompt("openai", "gpt-4o", Some(1_000), None, None, None);
        assert_eq!(bytes_per_token(&r), None);
    }

    // -----------------------------------------------------------------
    // classify_truncation / OutlierKind::Truncated — detección de tope de
    // tokens sin constantes de bytes-por-token ni gate de MIN_GROUP_SAMPLE
    // -----------------------------------------------------------------

    #[test]
    fn classify_truncation_detecta_el_caso_real_medido_en_produccion() {
        // Caso real: dos probes con EL MISMO input_tokens=4095 (justo el
        // num_ctx de Ollama en ese momento) pero bodies de tamaño MUY
        // distinto (18.955 B vs. 28.806 B). El proveedor truncó el prompt en
        // silencio y devolvió 200 OK igual.
        let rows = vec![
            req_prompt("openai", "llama3.2:3b", Some(4095), None, None, Some(18_955)),
            req_prompt("openai", "llama3.2:3b", Some(4095), None, None, Some(28_806)),
        ];

        let result = classify_outliers(&rows);

        assert!(result[0].contains(&OutlierKind::Truncated));
        assert!(result[1].contains(&OutlierKind::Truncated));
    }

    #[test]
    fn classify_truncation_detecta_el_falso_negativo_del_umbral_fraccional_anterior() {
        // REGRESIÓN — caso real medido en producción el 2026-07-13 contra un
        // Ollama local (llama3.2:3b, num_ctx=4096), cliente OpenCode. Dos
        // filas con EL MISMO input_tokens=4095 (justo el num_ctx) pero bodies
        // de 77.579 B y 84.161 B: Ollama truncó el prompt en silencio y
        // devolvió 200 OK igual.
        //
        // Este es el par que el detector con TRUNCATION_BYTES_DELTA = 0.10
        // NO detectaba: (84161-77579)/84161 = 7.8%, por debajo del 10%
        // exigido — un falso negativo confirmado sobre un truncamiento real.
        // Fue exactamente este caso el que forzó a recalibrar el umbral de
        // 0.10 a 0.05 (ver el doc de TRUNCATION_BYTES_DELTA): con 0.05, el
        // 7.8% de este par SÍ cruza el piso y nunca debe volver a pasar
        // desapercibido.
        let rows = vec![
            req_prompt("openai", "llama3.2:3b", Some(4095), None, None, Some(77_579)),
            req_prompt("openai", "llama3.2:3b", Some(4095), None, None, Some(84_161)),
        ];

        let result = classify_outliers(&rows);

        assert!(result[0].contains(&OutlierKind::Truncated));
        assert!(result[1].contains(&OutlierKind::Truncated));
    }

    #[test]
    fn classify_truncation_detecta_delta_grande_en_body_chico() {
        // Caso del reviewer: bodies chicos (1.000 B y 1.500 B) con EL MISMO
        // input_tokens, un delta del 33% — inequívocamente truncamiento. Un
        // piso ABSOLUTO de bytes (o de "tokens implícitos" convertidos con
        // una cota de B/tok) no escala hacia bodies chicos y puede dejar
        // pasar justo este caso, que es el más típico de un `num_ctx` chico
        // de un modelo local. La regla FRACCIONAL sí lo cubre: 500/1500 =
        // 33,3% >= TRUNCATION_BYTES_DELTA (5%).
        let rows = vec![
            req_prompt("openai", "llama3.2:3b", Some(512), None, None, Some(1_000)),
            req_prompt("openai", "llama3.2:3b", Some(512), None, None, Some(1_500)),
        ];

        let result = classify_outliers(&rows);

        assert!(result[0].contains(&OutlierKind::Truncated));
        assert!(result[1].contains(&OutlierKind::Truncated));
    }

    #[test]
    fn classify_truncation_no_flaggea_delta_chico_en_body_grande() {
        // Guard de falso positivo, caso del reviewer: bodies grandes
        // (199.400 B y 200.000 B) con EL MISMO input_tokens, un delta de
        // apenas 0.3% — ruido de serialización típico (IDs, timestamps) en
        // un body grande de un flujo agéntico, NO truncamiento. Un piso
        // ABSOLUTO de bytes (p. ej. "delta_bytes / 8.0 >= 64", ~512 B) SÍ
        // flaggearía este par (600 B de delta), un falso positivo. La regla
        // FRACCIONAL correctamente lo descarta: 600/200000 = 0.3%, muy por
        // debajo de TRUNCATION_BYTES_DELTA (5%).
        let rows = vec![
            req_prompt("openai", "llama3.2:3b", Some(65_000), None, None, Some(199_400)),
            req_prompt("openai", "llama3.2:3b", Some(65_000), None, None, Some(200_000)),
        ];

        let result = classify_outliers(&rows);

        assert!(!result[0].contains(&OutlierKind::Truncated));
        assert!(!result[1].contains(&OutlierKind::Truncated));
    }

    #[test]
    fn classify_truncation_no_flaggea_si_los_bodies_son_del_mismo_tamano() {
        // ESTE ES EL TEST QUE MÁS IMPORTA: mismo input_tokens y bodies
        // dentro del 1% de diferencia (probe repetido con prompt
        // prácticamente idéntico). Sin el guard de TRUNCATION_BYTES_DELTA,
        // cualquier repetición idéntica se marcaría como "truncamiento"
        // cuando en realidad es exactamente lo esperado — el falso positivo
        // que un detector naïve `bytes/tokens > umbral` NO podría evitar.
        let rows = vec![
            req_prompt("openai", "llama3.2:3b", Some(4095), None, None, Some(20_000)),
            req_prompt("openai", "llama3.2:3b", Some(4095), None, None, Some(20_150)), // +0.75%
        ];

        let result = classify_outliers(&rows);

        assert!(!result[0].contains(&OutlierKind::Truncated));
        assert!(!result[1].contains(&OutlierKind::Truncated));
    }

    #[test]
    fn classify_truncation_anthropic_cache_hit_sano_no_flaggea() {
        // Fila Anthropic con cache-hit grande: input_tokens=2 aislado sería
        // el gatillo perfecto de un detector naïve. Como es la ÚNICA fila
        // del grupo con ese total de tokens (prompt_tokens_total suma la
        // caché), classify_truncation ni siquiera encuentra un par con el
        // que comparar — no flaggea.
        let rows = vec![req_prompt("anthropic", "claude-opus-4", Some(2), Some(124_733), Some(1_355), Some(224_653))];

        let result = classify_outliers(&rows);

        assert!(!result[0].contains(&OutlierKind::Truncated));
    }

    #[test]
    fn classify_truncation_openai_con_cache_read_denominador_es_solo_input_tokens() {
        // Dos filas OpenAI con el mismo input_tokens pero cache_read
        // presente (subconjunto): el agrupamiento debe usar SOLO
        // input_tokens como total, sin sumar cache_read — si sumara,
        // estas dos filas caerían en grupos de tokens DISTINTOS y el
        // detector nunca las compararía entre sí.
        let rows = vec![
            req_prompt("openai", "gpt-4o", Some(1000), Some(400), None, Some(10_000)),
            req_prompt("openai", "gpt-4o", Some(1000), Some(400), None, Some(15_000)),
        ];

        let result = classify_outliers(&rows);

        assert!(result[0].contains(&OutlierKind::Truncated));
        assert!(result[1].contains(&OutlierKind::Truncated));
    }

    #[test]
    fn classify_truncation_filas_sin_input_tokens_se_excluyen_sin_panic() {
        let rows = vec![
            req_prompt("openai", "llama3.2:3b", None, None, None, Some(18_955)),
            req_prompt("openai", "llama3.2:3b", Some(4095), None, None, Some(28_806)),
        ];

        let result = classify_outliers(&rows);

        assert!(result[0].is_empty());
        assert!(result[1].is_empty());
    }

    #[test]
    fn classify_truncation_una_sola_fila_no_se_puede_probar() {
        // Un solo sample no prueba nada: podría ser genuinamente un prompt
        // grande que necesita exactamente ese input_tokens. El doc de
        // OutlierKind::Truncated es explícito: hacen falta >= 2 muestras
        // para EXCLUIR la coincidencia.
        let rows = vec![req_prompt("openai", "llama3.2:3b", Some(4095), None, None, Some(77_783))];

        let result = classify_outliers(&rows);

        assert!(!result[0].contains(&OutlierKind::Truncated));
    }

    #[test]
    fn requests_row_cells_context_b_tok_es_guion_si_falta_input_tokens() {
        // Fila sin `input_tokens`: prompt_tokens_total es None, y la celda
        // B/tok (índice 10 en la vista Context) debe renderizar `-`, NUNCA
        // `0.0` — mismo criterio que el resto de las celdas opcionales del
        // panel.
        let r = req_prompt("openai", "gpt-4o", None, None, None, Some(50_000));
        let cells = requests_row_cells(RequestsView::Context, &r);
        assert_eq!(cells[10], "-");
    }

    #[test]
    fn requests_row_cells_context_b_tok_calcula_bytes_por_token() {
        let r = req_prompt("openai", "gpt-4o", Some(1_000), None, None, Some(2_700));
        let cells = requests_row_cells(RequestsView::Context, &r);
        assert_eq!(cells[10], "2.7");
    }

    /// `cliente` (índice 12) de la vista Context debe surfacear
    /// `RequestRow::client` — el defecto que este test previene: el TUI
    /// espejaba `RecentRequest` campo a campo pero nunca sumó este, así que
    /// un salto de bytes inducido por el proxy (Claude Code cayendo a carga
    /// eager) quedaba sin atribución en el panel.
    #[test]
    fn requests_row_cells_context_surface_client() {
        let mut r = req("anthropic", "claude-opus-4", 200, Some(10.0), 100.0, Some(50), Some(10));
        r.client = Some("claude-cli/2.1.207 (external, sdk-cli)".to_string());

        let cells = requests_row_cells(RequestsView::Context, &r);

        assert_eq!(cells[12], "claude-cli/2.1.20…");
    }

    /// `client: None` debe leerse como `-`, NUNCA como string vacío ni como
    /// una clasificación inventada ("desconocido", "otro"…): un cliente sin
    /// `User-Agent` es un dato ausente, no una categoría.
    #[test]
    fn requests_row_cells_context_client_none_es_guion() {
        let mut r = req("anthropic", "claude-opus-4", 200, Some(10.0), 100.0, Some(50), Some(10));
        r.client = None;

        let cells = requests_row_cells(RequestsView::Context, &r);

        assert_eq!(cells[12], "-");
    }

    /// `truncate_client` no debe truncar un `User-Agent` que ya entra en
    /// [`CLIENT_DISPLAY_MAX`] caracteres — mismo criterio que
    /// [`truncate_model`] para strings cortos.
    #[test]
    fn truncate_client_no_trunca_si_entra() {
        assert_eq!(truncate_client(Some("curl/8.0")), "curl/8.0");
    }

    #[test]
    fn classify_truncation_compone_con_otro_marcador_en_la_misma_fila() {
        // Truncated debe convivir con otro OutlierKind en la misma fila
        // (acá, Error) sin pisarlo ni excluirlo.
        let mut rows = vec![
            req_prompt("openai", "llama3.2:3b", Some(4095), None, None, Some(18_955)),
            req_prompt("openai", "llama3.2:3b", Some(4095), None, None, Some(28_806)),
        ];
        rows[0].status = 500;

        let result = classify_outliers(&rows);

        assert!(result[0].contains(&OutlierKind::Truncated));
        assert!(result[0].contains(&OutlierKind::Error));
    }

    // -----------------------------------------------------------------
    // format_bytes — convención decimal (base 1000), casos de borde
    // -----------------------------------------------------------------

    #[test]
    fn format_bytes_cero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn format_bytes_justo_debajo_del_kb() {
        assert_eq!(format_bytes(999), "999 B");
    }

    #[test]
    fn format_bytes_exactamente_un_kb_decimal() {
        assert_eq!(format_bytes(1_000), "1.0 kB");
    }

    #[test]
    fn format_bytes_1024_no_es_un_caso_especial_binario() {
        // Convención DECIMAL: 1024 bytes son 1.024 kB, que redondeado a un
        // decimal da "1.0 kB" — igual que 1000. Este test documenta que
        // NO se usa la convención binaria (que mostraría "1.0 KiB" recién
        // en 1024 y no en 1000).
        assert_eq!(format_bytes(1_024), "1.0 kB");
    }

    #[test]
    fn format_bytes_un_millon_pasa_a_mb() {
        assert_eq!(format_bytes(1_000_000), "1.0 MB");
    }

    /// La frontera real no está en `1_000_000` sino donde el redondeo a un
    /// decimal empezaría a imprimir `1000.0`. Elegir la unidad ANTES de
    /// redondear devuelve `"1000.0 kB"` para todo el tramo `999_950..=999_999`,
    /// que se lee como un error de escala. Este test es el que muerde.
    #[test]
    fn format_bytes_no_imprime_mil_kb_nunca() {
        assert_eq!(format_bytes(999_949), "999.9 kB");
        assert_eq!(format_bytes(999_950), "1.0 MB");
        assert_eq!(format_bytes(999_999), "1.0 MB");

        for bytes in [999_950_usize, 999_975, 999_999] {
            assert!(
                !format_bytes(bytes).starts_with("1000"),
                "format_bytes({bytes}) no debe rendirse como 1000.x kB"
            );
        }
    }

    // -----------------------------------------------------------------
    // RequestsView — enum total, ciclado con `c`
    // -----------------------------------------------------------------

    #[test]
    fn requests_view_next_cicla_entre_las_dos_variantes() {
        assert_eq!(RequestsView::Latency.next(), RequestsView::Context);
        assert_eq!(RequestsView::Context.next(), RequestsView::Latency);
    }

    #[test]
    fn requests_view_default_es_latency() {
        assert_eq!(RequestsView::default(), RequestsView::Latency);
    }

    #[test]
    fn cycle_requests_view_no_op_si_el_panel_esta_oculto() {
        let mut app = App::new("http://x".to_string());
        app.show_requests_panel = false;

        app.cycle_requests_view();

        assert_eq!(app.requests_view, RequestsView::Latency);
    }

    #[test]
    fn cycle_requests_view_cicla_si_el_panel_esta_visible() {
        let mut app = App::new("http://x".to_string());
        assert!(app.show_requests_panel);

        app.cycle_requests_view();
        assert_eq!(app.requests_view, RequestsView::Context);

        app.cycle_requests_view();
        assert_eq!(app.requests_view, RequestsView::Latency);
    }

    // -----------------------------------------------------------------
    // RequestRow — deserialización de un payload realista de /requests,
    // incluyendo compatibilidad con una build vieja del proxy (sin los
    // campos nuevos de este slice).
    // -----------------------------------------------------------------

    #[test]
    fn request_row_deserializa_payload_realista_con_campos_de_contexto() {
        let json = r#"{
            "timestamp": "2026-07-09T14:02:11.483Z",
            "route": "/v1/messages",
            "upstream": "anthropic",
            "model": "claude-opus-4-1",
            "stream": true,
            "status": 200,
            "input_tokens": 5000,
            "output_tokens": 412,
            "cache_read_tokens": 4200,
            "cache_write_tokens": 0,
            "cost_estimate_usd": 0.0891,
            "cache_control_forced": false,
            "ttft_ms": 780.4,
            "total_ms": 3210.9,
            "context_system_bytes": 281,
            "context_tools_bytes": 159123,
            "context_history_bytes": 4000,
            "context_last_turn_bytes": 96,
            "context_other_bytes": 50,
            "context_measured_bytes": 163550,
            "context_messages_count": 12,
            "context_tax_ratio": 0.9994,
            "prepare_us": 850
        }"#;

        let row: RequestRow = serde_json::from_str(json).expect("debe deserializar un payload con todos los campos");

        assert_eq!(row.context_system_bytes, Some(281));
        assert_eq!(row.context_tools_bytes, Some(159_123));
        assert_eq!(row.context_history_bytes, Some(4_000));
        assert_eq!(row.context_last_turn_bytes, Some(96));
        assert_eq!(row.context_other_bytes, Some(50));
        assert_eq!(row.context_measured_bytes, Some(163_550));
        assert_eq!(row.context_messages_count, Some(12));
        assert!((row.context_tax_ratio.unwrap() - 0.9994).abs() < 1e-9);
        assert_eq!(row.prepare_us, Some(850));
    }

    #[test]
    fn request_row_deserializa_build_vieja_del_proxy_sin_romper() {
        // Caso de compatibilidad real: un proxy de build ANTERIOR a este
        // slice no conoce los campos de contexto ni `prepare_us`, así que
        // ni siquiera los manda en el JSON (a diferencia de los campos
        // `Option` que YA existían, que si el proveedor no los reporta se
        // mandan como `null` explícito). El monitor NUEVO tiene que poder
        // hablar con un proxy VIEJO sin panickear ni fallar la
        // deserialización de la fila entera.
        //
        // `prepare_us` se espeja como `Option<u64>` aunque el proxy lo
        // exponga como `u64`: el espejo modela lo que el monitor puede
        // SABER, no lo que el servidor declara. Contra un proxy viejo la
        // clave no llega y el dato queda en `None`, distinguible de un
        // `Some(0)` legítimo.
        let json = r#"{
            "timestamp": "2024-01-01T00:00:00Z",
            "route": "/v1/messages",
            "upstream": "anthropic",
            "model": "claude-opus-4",
            "stream": true,
            "status": 200,
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_tokens": null,
            "cache_write_tokens": null,
            "cost_estimate_usd": null,
            "cache_control_forced": false,
            "ttft_ms": null,
            "total_ms": 100.0
        }"#;

        let row: RequestRow = serde_json::from_str(json).expect("debe deserializar aunque falten los campos nuevos");

        assert_eq!(row.context_system_bytes, None);
        assert_eq!(row.context_tools_bytes, None);
        assert_eq!(row.context_history_bytes, None);
        assert_eq!(row.context_last_turn_bytes, None);
        assert_eq!(row.context_other_bytes, None);
        assert_eq!(row.context_measured_bytes, None);
        assert_eq!(row.context_messages_count, None);
        assert_eq!(row.context_tax_ratio, None);
        // `None`, no `Some(0)`: contra un proxy viejo el dato está AUSENTE.
        // Un `0` significaría que el proxy midió cero microsegundos.
        assert_eq!(row.prepare_us, None);

        // La capa de presentación cumple la regla del proyecto: nunca `0`
        // para un dato ausente, siempre `-`.
        assert_eq!(opt_bytes(row.context_system_bytes), "-");
        assert_eq!(opt_usize(row.context_messages_count), "-");
        assert_eq!(opt_tax_ratio(row.context_tax_ratio), "-");
        assert_eq!(opt_u64(row.prepare_us), "-");
    }

    #[test]
    fn request_row_deserializa_campos_de_contexto_explicitamente_null() {
        // Variante del caso de compatibilidad, pero con las claves nuevas
        // PRESENTES y en `null` explícito (p. ej. un proxy que ya conoce el
        // campo pero no pudo calcular el desglose para esta fila puntual).
        let json = r#"{
            "timestamp": "2024-01-01T00:00:00Z",
            "route": "/v1/messages",
            "upstream": "anthropic",
            "model": "claude-opus-4",
            "stream": false,
            "status": 200,
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_tokens": null,
            "cache_write_tokens": null,
            "cost_estimate_usd": null,
            "cache_control_forced": false,
            "ttft_ms": null,
            "total_ms": 100.0,
            "context_system_bytes": null,
            "context_tools_bytes": null,
            "context_history_bytes": null,
            "context_last_turn_bytes": null,
            "context_other_bytes": null,
            "context_measured_bytes": null,
            "context_messages_count": null,
            "context_tax_ratio": null,
            "prepare_us": 12
        }"#;

        let row: RequestRow = serde_json::from_str(json).expect("debe deserializar con context_* en null explícito");

        assert_eq!(row.context_system_bytes, None);
        assert_eq!(row.context_tax_ratio, None);
        assert_eq!(row.prepare_us, Some(12));
    }

    // -----------------------------------------------------------------
    // RequestRow — nuevos campos tools_by_server / tools_overhead_bytes
    // -----------------------------------------------------------------

    #[test]
    fn request_row_deserializa_tools_by_server_presente() {
        let json = r#"{
            "timestamp": "2026-07-09T14:02:11.483Z",
            "route": "/v1/messages",
            "upstream": "anthropic",
            "model": "claude-opus-4-1",
            "stream": true,
            "status": 200,
            "input_tokens": 5000,
            "output_tokens": 412,
            "cache_read_tokens": 4200,
            "cache_write_tokens": 0,
            "cost_estimate_usd": 0.0891,
            "cache_control_forced": false,
            "ttft_ms": 780.4,
            "total_ms": 3210.9,
            "context_tools_bytes": 159080,
            "tools_by_server": [
                {"server": "(native)", "kind": "native", "tools": 29, "bytes": 86168},
                {"server": "claude_ai_Gmail", "kind": "mcp", "tools": 13, "bytes": 24321}
            ],
            "tools_overhead_bytes": 77
        }"#;

        let row: RequestRow = serde_json::from_str(json).expect("debe deserializar con tools_by_server presente");

        let servers = row.tools_by_server.expect("debe traer el desglose");
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].server, "(native)");
        assert_eq!(servers[0].kind, "native");
        assert_eq!(servers[0].tools, 29);
        assert_eq!(servers[0].bytes, 86_168);
        // Fixture de un proxy anterior a `deferred_tools`: la clave ni
        // siquiera viaja en el JSON. Con `Option<usize>` eso debe caer en
        // `None` (AUSENTE), NUNCA en `Some(0)`: un `Some(0)` afirmaría que el
        // proxy midió y confirmó "nada diferido", cuando en realidad nunca
        // midió nada — la fila entera sigue deserializando sin romper.
        assert_eq!(servers[0].deferred_tools, None);
        assert_eq!(servers[1].deferred_tools, None);
        assert_eq!(row.tools_overhead_bytes, Some(77));
    }

    /// Proxy YA con `deferred_tools` en el wire: debe deserializar el valor
    /// tal cual, no solo caer al default de la build vieja de arriba.
    #[test]
    fn request_row_deserializa_deferred_tools_presente() {
        let json = r#"{
            "timestamp": "2026-07-12T10:00:00Z",
            "route": "/v1/messages",
            "upstream": "anthropic",
            "model": "claude-opus-4-1",
            "stream": true,
            "status": 200,
            "input_tokens": 5000,
            "output_tokens": 412,
            "cache_read_tokens": null,
            "cache_write_tokens": null,
            "cost_estimate_usd": 0.0891,
            "cache_control_forced": false,
            "ttft_ms": 780.4,
            "total_ms": 3210.9,
            "context_tools_bytes": 159080,
            "tools_by_server": [
                {"server": "claude_ai_Gmail", "kind": "mcp", "tools": 3, "bytes": 6000, "deferred_tools": 3},
                {"server": "claude_ai_Google_Calendar", "kind": "mcp", "tools": 4, "bytes": 8000, "deferred_tools": 0}
            ],
            "tools_overhead_bytes": 77
        }"#;

        let row: RequestRow = serde_json::from_str(json).expect("debe deserializar con deferred_tools presente");

        let servers = row.tools_by_server.expect("debe traer el desglose");
        let gmail = servers.iter().find(|s| s.server == "claude_ai_Gmail").expect("Gmail presente");
        assert_eq!(gmail.deferred_tools, Some(3), "servidor totalmente diferido: deferred_tools == tools");

        let calendar = servers.iter().find(|s| s.server == "claude_ai_Google_Calendar").expect("Calendar presente");
        assert_eq!(calendar.deferred_tools, Some(0), "servidor NADA diferido: sus bytes son reales y desconectables");
    }

    #[test]
    fn request_row_deserializa_sin_tools_by_server_build_vieja() {
        // Proxy anterior a este slice: ni `tools_by_server` ni
        // `tools_overhead_bytes` viajan en el JSON. Deben caer en `None`,
        // igual que el resto de los campos `Option` de este struct, sin
        // panickear ni fallar la deserialización de la fila entera.
        let json = r#"{
            "timestamp": "2024-01-01T00:00:00Z",
            "route": "/v1/messages",
            "upstream": "anthropic",
            "model": "claude-opus-4",
            "stream": true,
            "status": 200,
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_tokens": null,
            "cache_write_tokens": null,
            "cost_estimate_usd": null,
            "cache_control_forced": false,
            "ttft_ms": null,
            "total_ms": 100.0
        }"#;

        let row: RequestRow = serde_json::from_str(json).expect("debe deserializar sin los campos de tools");

        assert_eq!(row.tools_by_server, None);
        assert_eq!(row.tools_overhead_bytes, None);
    }

    // -----------------------------------------------------------------
    // find_tools_source_row / diff_against_baseline — panel "tools por
    // servidor" (tecla `s`)
    // -----------------------------------------------------------------

    fn tool_row(server: &str, kind: &str, tools: usize, bytes: usize) -> ToolServerRow {
        ToolServerRow { server: server.to_string(), kind: kind.to_string(), tools, bytes, deferred_tools: Some(0) }
    }

    /// Variante de [`tool_row`] que además fija `deferred_tools`, para los
    /// tests que necesitan un servidor con diferido parcial o total (no solo
    /// el `Some(0)` por defecto de la variante simple).
    fn tool_row_deferred(server: &str, kind: &str, tools: usize, bytes: usize, deferred_tools: usize) -> ToolServerRow {
        ToolServerRow { server: server.to_string(), kind: kind.to_string(), tools, bytes, deferred_tools: Some(deferred_tools) }
    }

    /// Variante de `req` (arriba) que además permite fijar `tools_by_server`,
    /// para los tests de [`find_tools_source_row`].
    fn req_with_tools(timestamp: &str, tools_by_server: Option<Vec<ToolServerRow>>) -> RequestRow {
        let mut r = req("anthropic", "claude-opus-4", 200, Some(10.0), 100.0, Some(50), Some(10));
        r.timestamp = timestamp.to_string();
        r.tools_by_server = tools_by_server;
        r
    }

    #[test]
    fn find_tools_source_row_ninguna_fila_califica_devuelve_none() {
        let rows = vec![req_with_tools("t1", None), req_with_tools("t2", Some(vec![]))];
        assert!(find_tools_source_row(&rows).is_none());
    }

    #[test]
    fn find_tools_source_row_salta_some_vacio_y_elige_la_fila_mas_vieja_con_datos() {
        // t1 tiene datos reales; t2 es la fila MÁS RECIENTE pero declara
        // Some(vec![]) — no califica porque "declara sin tools" no es lo
        // mismo que "sin dato". Debe elegirse t1, no t2.
        let rows = vec![
            req_with_tools("t1", Some(vec![tool_row("(native)", "native", 29, 86_168)])),
            req_with_tools("t2", Some(vec![])),
        ];

        let source = find_tools_source_row(&rows).expect("t1 califica como fuente");
        assert_eq!(source.timestamp, "t1");
    }

    #[test]
    fn find_tools_source_row_elige_la_mas_nueva_entre_varias_con_datos() {
        let rows = vec![
            req_with_tools("t1", Some(vec![tool_row("(native)", "native", 29, 86_168)])),
            req_with_tools("t2", Some(vec![tool_row("(native)", "native", 30, 90_000)])),
        ];

        let source = find_tools_source_row(&rows).expect("hay filas con datos");
        assert_eq!(source.timestamp, "t2");
    }

    #[test]
    fn diff_against_baseline_sin_baseline_todos_los_deltas_son_none() {
        let current = vec![tool_row("(native)", "native", 29, 86_168), tool_row("claude_ai_Gmail", "mcp", 13, 24_321)];

        let diffs = diff_against_baseline(&current, None);

        assert_eq!(diffs.len(), 2);
        assert!(diffs.iter().all(|d| d.delta.is_none()));
    }

    #[test]
    fn diff_against_baseline_servidor_desaparecido_aparece_con_bytes_cero_y_delta_negativo() {
        let current = vec![tool_row("(native)", "native", 29, 86_168)];
        let mut baseline = BTreeMap::new();
        baseline.insert("(native)".to_string(), 86_168usize);
        baseline.insert("claude_ai_Google_Calendar".to_string(), 21_034usize);

        let diffs = diff_against_baseline(&current, Some(&baseline));

        let disappeared =
            diffs.iter().find(|d| d.server == "claude_ai_Google_Calendar").expect("debe seguir apareciendo como fila");
        assert_eq!(disappeared.bytes, 0);
        assert_eq!(disappeared.tools, 0);
        assert_eq!(disappeared.kind, "-");
        assert_eq!(disappeared.delta, Some(-21_034));
    }

    #[test]
    fn diff_against_baseline_servidor_nuevo_tiene_delta_positivo_completo() {
        let current = vec![tool_row("(native)", "native", 29, 86_168), tool_row("plugin_engram_engram", "mcp", 18, 17_737)];
        let mut baseline = BTreeMap::new();
        baseline.insert("(native)".to_string(), 86_168usize);

        let diffs = diff_against_baseline(&current, Some(&baseline));

        let new_server = diffs.iter().find(|d| d.server == "plugin_engram_engram").expect("debe estar presente");
        assert_eq!(new_server.delta, Some(17_737));
    }

    #[test]
    fn diff_against_baseline_servidor_sin_cambios_tiene_delta_cero() {
        let current = vec![tool_row("(native)", "native", 29, 86_168)];
        let mut baseline = BTreeMap::new();
        baseline.insert("(native)".to_string(), 86_168usize);

        let diffs = diff_against_baseline(&current, Some(&baseline));

        assert_eq!(diffs[0].delta, Some(0));
    }

    #[test]
    fn diff_against_baseline_orden_presentes_primero_en_orden_original_luego_desaparecidos() {
        // `current` llega bytes DESC (orden real del proxy): la función NO
        // debe reordenarlo. Los servidores desaparecidos van DESPUÉS, y entre
        // ELLOS se ordenan por bytes de baseline DESCENDENTE.
        let current = vec![tool_row("(native)", "native", 29, 86_168), tool_row("claude_ai_Gmail", "mcp", 13, 24_321)];
        let mut baseline = BTreeMap::new();
        baseline.insert("(native)".to_string(), 86_168usize);
        baseline.insert("claude_ai_Gmail".to_string(), 24_321usize);
        baseline.insert("claude_ai_Google_Calendar".to_string(), 21_034usize);
        baseline.insert("claude_ai_Google_Drive".to_string(), 9_743usize);

        let diffs = diff_against_baseline(&current, Some(&baseline));

        let names: Vec<&str> = diffs.iter().map(|d| d.server.as_str()).collect();
        assert_eq!(names, vec!["(native)", "claude_ai_Gmail", "claude_ai_Google_Calendar", "claude_ai_Google_Drive"]);
    }

    #[test]
    fn tool_pct_of_total_none_o_cero_da_guion_nunca_cero_coma_cero() {
        assert_eq!(tool_pct_of_total(1000, None), "-");
        assert_eq!(tool_pct_of_total(0, Some(0)), "-");
    }

    #[test]
    fn tool_pct_of_total_calcula_porcentaje() {
        assert_eq!(tool_pct_of_total(24_321, Some(159_080)), "15.3");
    }

    #[test]
    fn format_delta_bytes_casos() {
        assert_eq!(format_delta_bytes(None), "-");
        assert_eq!(format_delta_bytes(Some(0)), "0 B");
        assert_eq!(format_delta_bytes(Some(-55_098)), "-55.1 kB");
        assert_eq!(format_delta_bytes(Some(1_200)), "+1.2 kB");
    }

    /// `deferred_tools` sobrevive `diff_against_baseline` y se refleja en la
    /// celda `deferred` de `tools_row_cells`: los tres casos que motivan el
    /// campo (totalmente diferido, nada diferido, diferido parcial).
    #[test]
    fn diff_against_baseline_preserva_deferred_tools_y_deferred_cell_los_formatea() {
        let current = vec![
            tool_row_deferred("claude_ai_Gmail", "mcp", 3, 6_000, 3),
            tool_row_deferred("claude_ai_Google_Calendar", "mcp", 4, 8_000, 0),
            tool_row_deferred("claude_ai_Google_Drive", "mcp", 5, 10_000, 2),
        ];

        let diffs = diff_against_baseline(&current, None);

        let gmail = diffs.iter().find(|d| d.server == "claude_ai_Gmail").unwrap();
        assert_eq!(gmail.deferred_tools, Some(3));
        assert_eq!(deferred_cell(gmail), "3/3", "totalmente diferido");

        let calendar = diffs.iter().find(|d| d.server == "claude_ai_Google_Calendar").unwrap();
        assert_eq!(calendar.deferred_tools, Some(0));
        assert_eq!(deferred_cell(calendar), "0/4", "nada diferido: bytes reales y desconectables");

        let drive = diffs.iter().find(|d| d.server == "claude_ai_Google_Drive").unwrap();
        assert_eq!(deferred_cell(drive), "2/5", "diferido parcial");
    }

    /// Un servidor DESAPARECIDO (fila sintética de `diff_against_baseline`,
    /// `tools == 0`) debe mostrar `"-"` en la celda `deferred`, no `"0/0"`:
    /// no hay tools vivas de las que sacar una fracción.
    #[test]
    fn deferred_cell_guion_para_servidor_desaparecido() {
        let current = vec![tool_row("(native)", "native", 29, 86_168)];
        let mut baseline = BTreeMap::new();
        baseline.insert("claude_ai_Gmail".to_string(), 6_000usize);

        let diffs = diff_against_baseline(&current, Some(&baseline));

        let disappeared = diffs.iter().find(|d| d.server == "claude_ai_Gmail").unwrap();
        assert_eq!(deferred_cell(disappeared), "-");
    }

    /// `deferred_tools: None` (proxy de build anterior a este campo, ver
    /// `ToolServerRow::deferred_tools`) con `tools > 0` debe mostrar `"?"` en
    /// la celda `deferred`, NUNCA `"0/N"`: `0/N` es la afirmación medida de
    /// "nada diferido", y este servidor no tiene ningún dato medido de qué
    /// diferir — absent ≠ zero.
    #[test]
    fn deferred_cell_interrogacion_cuando_deferred_tools_es_none() {
        let row = ToolServerRow {
            server: "claude_ai_Gmail".to_string(),
            kind: "mcp".to_string(),
            tools: 3,
            bytes: 6_000,
            deferred_tools: None,
        };

        let diffs = diff_against_baseline(&[row], None);

        assert_eq!(deferred_cell(&diffs[0]), "?");
    }

    // -----------------------------------------------------------------
    // App — panel de tools por servidor: toggle independiente y baseline
    // -----------------------------------------------------------------

    #[test]
    fn show_tools_panel_arranca_visible_y_es_independiente_del_panel_de_requests() {
        let mut app = App::new("http://x".to_string());
        assert!(app.show_tools_panel);
        assert!(app.show_requests_panel);

        app.toggle_tools_panel();
        assert!(!app.show_tools_panel);
        // Apagar `s` no debe afectar `p`.
        assert!(app.show_requests_panel);
    }

    #[test]
    fn show_quota_panel_arranca_visible_y_es_independiente_de_los_demas() {
        let mut app = App::new("http://x".to_string());
        assert!(app.show_quota_panel);
        assert!(app.show_tools_panel);
        assert!(app.show_requests_panel);

        app.toggle_quota_panel();
        assert!(!app.show_quota_panel);
        // Apagar `u` no debe afectar `s` ni `p`.
        assert!(app.show_tools_panel);
        assert!(app.show_requests_panel);
    }

    #[test]
    fn mark_baseline_toma_foto_de_tools_by_server_de_la_fila_fuente() {
        let mut app = App::new("http://x".to_string());
        app.recent_requests = vec![req_with_tools("t1", Some(vec![tool_row("(native)", "native", 29, 86_168)]))];

        app.mark_baseline();

        let baseline = app.baseline.as_ref().expect("mark_baseline debe crear un baseline");
        let tools_baseline = baseline.tools_by_server.as_ref().expect("debe tomar la foto de tools_by_server");
        assert_eq!(tools_baseline.get("(native)"), Some(&86_168));
    }

    #[test]
    fn mark_baseline_sin_fila_fuente_deja_tools_by_server_en_none() {
        let mut app = App::new("http://x".to_string());
        // recent_requests vacío: no hay fila fuente que fotografiar.
        app.mark_baseline();

        let baseline = app.baseline.as_ref().expect("mark_baseline debe crear un baseline igual");
        assert!(baseline.tools_by_server.is_none());
    }

    // -----------------------------------------------------------------
    // find_quota_source_row / quota_bar / countdown / quota_lines — panel
    // de cuota Codex (tecla `u`)
    // -----------------------------------------------------------------

    /// Fixture de `CodexQuotaRow` con todos los campos presentes, para los
    /// tests que no necesitan variar ningún campo puntual.
    fn full_quota() -> CodexQuotaRow {
        CodexQuotaRow {
            plan_type: Some("plus".to_string()),
            active_limit: Some("primary".to_string()),
            credits_balance: Some("42".to_string()),
            primary_used_percent: Some(4),
            secondary_used_percent: Some(0),
            primary_window_minutes: Some(300),
            secondary_window_minutes: Some(0),
            primary_reset_after_seconds: Some(3_600),
            primary_reset_at: Some(1_000_000),
            secondary_reset_at: None,
            credits_has_credits: Some(true),
            credits_unlimited: Some(false),
        }
    }

    /// Variante de `req` que además permite fijar `codex_quota`, para los
    /// tests de [`find_quota_source_row`] y de render.
    fn req_with_quota(timestamp: &str, codex_quota: Option<CodexQuotaRow>) -> RequestRow {
        let mut r = req("openai", "gpt-5.5", 200, Some(10.0), 100.0, Some(50), None);
        r.timestamp = timestamp.to_string();
        r.codex_quota = codex_quota;
        r
    }

    #[test]
    fn find_quota_source_row_elige_la_fila_mas_reciente_con_dato() {
        let rows =
            vec![req_with_quota("t1", Some(full_quota())), req_with_quota("t2", None), req_with_quota("t3", Some(full_quota()))];

        let source = find_quota_source_row(&rows).expect("t3 califica como fuente");
        assert_eq!(source.timestamp, "t3");
    }

    #[test]
    fn find_quota_source_row_ninguna_fila_califica_devuelve_none() {
        let rows = vec![req_with_quota("t1", None), req_with_quota("t2", None)];
        assert!(find_quota_source_row(&rows).is_none());
    }

    #[test]
    fn find_quota_source_row_salta_filas_none_mas_nuevas_y_usa_la_ultima_con_dato() {
        // t2 es la fila MÁS RECIENTE pero no trae cuota (tráfico no-Codex
        // intercalado): la fuente debe seguir siendo t1.
        let rows = vec![req_with_quota("t1", Some(full_quota())), req_with_quota("t2", None)];

        let source = find_quota_source_row(&rows).expect("t1 califica como fuente");
        assert_eq!(source.timestamp, "t1");
    }

    #[test]
    fn quota_bar_extremos_todo_vacio_o_todo_lleno() {
        assert_eq!(quota_bar(0), "·".repeat(QUOTA_BAR_WIDTH));
        assert_eq!(quota_bar(100), "█".repeat(QUOTA_BAR_WIDTH));
    }

    #[test]
    fn quota_bar_relleno_proporcional_al_porcentaje() {
        let bar = quota_bar(4);
        let expected_filled = 4 * QUOTA_BAR_WIDTH / 100;
        assert_eq!(bar.chars().filter(|&c| c == '█').count(), expected_filled);
        assert_eq!(bar.chars().count(), QUOTA_BAR_WIDTH);
    }

    #[test]
    fn quota_bar_clampa_valores_mayores_a_cien() {
        assert_eq!(quota_bar(150), quota_bar(100));
    }

    #[test]
    fn format_reset_countdown_none_da_guion() {
        assert_eq!(format_reset_countdown(None), "—");
    }

    #[test]
    fn format_reset_countdown_remaining_no_positivo_resetea_ahora() {
        assert_eq!(format_reset_countdown(Some(0)), "resetea ahora");
        assert_eq!(format_reset_countdown(Some(-10)), "resetea ahora");
    }

    #[test]
    fn format_reset_countdown_descompone_dias_y_horas() {
        let remaining = 6 * 86_400 + 8 * 3_600;
        assert_eq!(format_reset_countdown(Some(remaining)), "resetea en 6d 8h");
    }

    #[test]
    fn format_reset_countdown_descompone_horas_y_minutos_sin_dias() {
        let remaining = 3 * 3_600 + 12 * 60;
        assert_eq!(format_reset_countdown(Some(remaining)), "resetea en 3h 12m");
    }

    #[test]
    fn format_reset_countdown_solo_minutos_sin_horas_ni_dias() {
        assert_eq!(format_reset_countdown(Some(45 * 60)), "resetea en 45m");
    }

    #[test]
    fn quota_reset_remaining_prefiere_reset_at_absoluto() {
        let mut quota = full_quota();
        quota.primary_reset_at = Some(1_000_500);
        quota.primary_reset_after_seconds = Some(999_999); // no debe usarse
        let remaining = quota_reset_remaining(&quota, "2024-01-01T00:00:00Z", 1_000_000);
        assert_eq!(remaining, Some(500));
    }

    #[test]
    fn quota_reset_remaining_fallback_a_timestamp_mas_after_seconds() {
        let mut quota = full_quota();
        quota.primary_reset_at = None;
        quota.primary_reset_after_seconds = Some(3_600);
        let timestamp = "2024-01-01T00:00:00Z";
        let base = chrono::DateTime::parse_from_rfc3339(timestamp).unwrap().timestamp();
        let remaining = quota_reset_remaining(&quota, timestamp, base + 1_000);
        assert_eq!(remaining, Some(3_600 - 1_000));
    }

    #[test]
    fn quota_reset_remaining_none_si_ambas_fuentes_faltan() {
        let mut quota = full_quota();
        quota.primary_reset_at = None;
        quota.primary_reset_after_seconds = None;
        assert_eq!(quota_reset_remaining(&quota, "2024-01-01T00:00:00Z", 0), None);
    }

    /// Regresión: un `reset_at` cercano a `i64::MIN` (cabecera `x-codex-*`
    /// adversaria o corrupta) NO debe desbordar la resta —panic en debug, wrap
    /// silencioso en release—, sino degradar a `None` (que se renderiza `"—"`).
    #[test]
    fn quota_reset_remaining_no_desborda_con_reset_at_extremo() {
        let mut quota = full_quota();
        quota.primary_reset_at = Some(i64::MIN);
        assert_eq!(quota_reset_remaining(&quota, "2024-01-01T00:00:00Z", 1_750_000_000), None);

        quota.primary_reset_at = Some(i64::MAX);
        assert_eq!(quota_reset_remaining(&quota, "2024-01-01T00:00:00Z", -1), None);
    }

    #[test]
    fn quota_lines_oculta_secundaria_cuando_window_es_cero_o_ausente() {
        let mut quota = full_quota();
        quota.secondary_window_minutes = Some(0);
        let lines = quota_lines(&quota, "2024-01-01T00:00:00Z", 1_000_000);
        assert!(!lines.iter().any(|l| l.starts_with("secundaria")));

        quota.secondary_window_minutes = None;
        let lines = quota_lines(&quota, "2024-01-01T00:00:00Z", 1_000_000);
        assert!(!lines.iter().any(|l| l.starts_with("secundaria")));
    }

    #[test]
    fn quota_lines_muestra_secundaria_cuando_window_mayor_a_cero() {
        let mut quota = full_quota();
        quota.secondary_window_minutes = Some(10_080);
        quota.secondary_used_percent = Some(12);
        let lines = quota_lines(&quota, "2024-01-01T00:00:00Z", 1_000_000);
        assert!(lines.iter().any(|l| l.starts_with("secundaria")));
    }

    #[test]
    fn quota_lines_omite_creditos_si_has_credits_no_es_true() {
        let mut quota = full_quota();
        quota.credits_has_credits = Some(false);
        let lines = quota_lines(&quota, "2024-01-01T00:00:00Z", 1_000_000);
        assert!(!lines.iter().any(|l| l.starts_with("créditos")));

        quota.credits_has_credits = None;
        let lines = quota_lines(&quota, "2024-01-01T00:00:00Z", 1_000_000);
        assert!(!lines.iter().any(|l| l.starts_with("créditos")));
    }

    #[test]
    fn quota_lines_muestra_ilimitados_cuando_credits_unlimited_true() {
        let mut quota = full_quota();
        quota.credits_has_credits = Some(true);
        quota.credits_unlimited = Some(true);
        let lines = quota_lines(&quota, "2024-01-01T00:00:00Z", 1_000_000);
        assert!(lines.iter().any(|l| l == "créditos: ilimitados"));
    }

    #[test]
    fn quota_lines_plan_y_limite_ausentes_muestran_guion() {
        let mut quota = full_quota();
        quota.plan_type = None;
        quota.active_limit = None;
        let lines = quota_lines(&quota, "2024-01-01T00:00:00Z", 1_000_000);
        assert_eq!(lines[0], "plan: — · límite: —");
    }

    // -----------------------------------------------------------------
    // RequestRow — deserialización de codex_quota (presente y ausente)
    // -----------------------------------------------------------------

    #[test]
    fn request_row_deserializa_codex_quota_presente() {
        let json = r#"{
            "timestamp": "2026-07-13T10:00:00Z",
            "route": "/v1/responses",
            "upstream": "openai",
            "model": "gpt-5.5",
            "stream": true,
            "status": 200,
            "cache_control_forced": false,
            "total_ms": 100.0,
            "codex_quota": {
                "plan_type": "plus",
                "active_limit": "primary",
                "credits_balance": "42",
                "primary_used_percent": 4,
                "secondary_used_percent": 0,
                "primary_window_minutes": 300,
                "secondary_window_minutes": 0,
                "primary_reset_after_seconds": 3600,
                "primary_reset_at": 1735689600,
                "secondary_reset_at": null,
                "credits_has_credits": true,
                "credits_unlimited": false
            }
        }"#;

        let row: RequestRow = serde_json::from_str(json).expect("debe deserializar con codex_quota presente");
        let quota = row.codex_quota.expect("debe traer la cuota");
        assert_eq!(quota.plan_type.as_deref(), Some("plus"));
        assert_eq!(quota.primary_used_percent, Some(4));
        assert_eq!(quota.secondary_reset_at, None);
    }

    #[test]
    fn request_row_deserializa_sin_codex_quota_build_vieja() {
        // Proxy anterior a esta rebanada: la clave ni siquiera viaja en el
        // JSON. Debe caer en `None`, sin panickear ni fallar la
        // deserialización de la fila entera — mismo contrato que
        // `tools_by_server`/`prepare_us`.
        let json = r#"{
            "timestamp": "2024-01-01T00:00:00Z",
            "route": "/v1/messages",
            "upstream": "anthropic",
            "model": "claude-opus-4",
            "stream": true,
            "status": 200,
            "cache_control_forced": false,
            "total_ms": 100.0
        }"#;

        let row: RequestRow = serde_json::from_str(json).expect("debe deserializar sin codex_quota");
        assert_eq!(row.codex_quota, None);
    }

    // -----------------------------------------------------------------
    // effort / spd_req / spd_got — columnas nuevas de la vista Latency
    // -----------------------------------------------------------------

    #[test]
    fn opt_str_short_none_da_guion() {
        assert_eq!(opt_str_short(None), "-");
    }

    #[test]
    fn opt_str_short_no_trunca_si_entra_justo() {
        // "standard" mide exactamente SPEED_DISPLAY_MAX (8) caracteres.
        assert_eq!(opt_str_short(Some("standard")), "standard");
    }

    #[test]
    fn opt_str_short_trunca_valores_mas_largos_que_el_maximo() {
        assert_eq!(opt_str_short(Some("extralongvalue")), "extralo…");
    }

    /// Un proxy ANTERIOR a este slice no manda las claves
    /// `requested_effort`/`requested_speed`/`served_speed` en absoluto (ni
    /// siquiera como `null`): `serde` debe tratar la ausencia como `None` sin
    /// necesidad de `#[serde(default)]` (mismo comportamiento ya documentado
    /// para `prepare_us`), y esas celdas deben renderizar `-`.
    #[test]
    fn request_row_deserializa_effort_speed_ausentes_como_none_en_proxy_viejo() {
        let json = r#"{
            "timestamp": "2024-01-01T00:00:00Z",
            "route": "/v1/messages",
            "upstream": "anthropic",
            "stream": false,
            "status": 200,
            "cache_control_forced": false,
            "total_ms": 100.0
        }"#;
        let row: RequestRow = serde_json::from_str(json).unwrap();

        assert_eq!(row.requested_effort, None);
        assert_eq!(row.requested_speed, None);
        assert_eq!(row.served_speed, None);
        assert_eq!(opt_str_short(row.requested_effort.as_deref()), "-");
        assert_eq!(opt_str_short(row.requested_speed.as_deref()), "-");
        assert_eq!(opt_str_short(row.served_speed.as_deref()), "-");
    }

    /// Con las tres claves presentes en el JSON (un proxy de este slice, en
    /// una petición real de Claude Code con `output_config.effort: "high"`),
    /// deben deserializar a sus valores exactos.
    #[test]
    fn request_row_deserializa_effort_speed_presentes() {
        let json = r#"{
            "timestamp": "2024-01-01T00:00:00Z",
            "route": "/v1/messages",
            "upstream": "anthropic",
            "stream": false,
            "status": 200,
            "cache_control_forced": false,
            "total_ms": 100.0,
            "requested_effort": "high",
            "requested_speed": "fast",
            "served_speed": "fast"
        }"#;
        let row: RequestRow = serde_json::from_str(json).unwrap();

        assert_eq!(row.requested_effort.as_deref(), Some("high"));
        assert_eq!(row.requested_speed.as_deref(), Some("fast"));
        assert_eq!(row.served_speed.as_deref(), Some("fast"));
    }
}
