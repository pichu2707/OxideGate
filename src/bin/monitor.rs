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
//!             (Latency → Context → Cache → Toll); no-op si el panel está
//!             oculto (`p`)
//!   s         mostrar/ocultar el panel de "tools por servidor" (desglose
//!             de bytes de herramientas MCP, con delta contra el baseline
//!             marcado con `b`); INDEPENDIENTE de `p`/`c`
//!   g         mostrar/ocultar el contador de potencia de la máquina
//!             (vatios y uso de GPU); arranca OCULTO porque muestrear
//!             cuesta ~24 ms por poll
//!   u         mostrar/ocultar el panel de cuota de suscripción Codex
//!             (uso de cuota); INDEPENDIENTE de `p`/`c`/`s`
//!   f         filtrar el panel de requests recientes al modelo seleccionado
//!             con `↑`/`↓`; arranca APAGADO, porque el panel es un feed
//!             global por defecto
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table, TableState};
use ratatui::{Frame, Terminal};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

/// Intervalo entre polls a `/stats`.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Muestra de la GPU en un instante: lo que la máquina está gastando AHORA.
///
/// # Por qué el monitor invoca `nvidia-smi` y el proxy no
///
/// Medido en esta máquina: **arrancar** `nvidia-smi` cuesta **23,81 ms** de
/// mediana, seis veces el overhead TOTAL del proxy: **3,79 ms** entre
/// `prepare_us` y `scan_us` (ver `docs/telemetry-per-request.md` §4.1). Por eso
/// el proxy no lo invoca: pagaría ese arranque en cada petición y el
/// instrumento pasaría a ser el gasto dominante.
///
/// El monitor refresca cada segundo, así que esos 23,81 ms son el **2,4%** de
/// su ciclo — y solo se pagan mientras el panel está visible.
///
/// # Corrección: el proxy SÍ mide energía, con otro mecanismo
///
/// Una versión anterior de este comentario decía que atribuir por petición
/// «exige NVML en vez de un subproceso». **Es falso**, y lo desmiente una
/// medición: un `nvidia-smi -lms 200` **persistente** paga el arranque UNA vez
/// y luego escupe una muestra cada 200 ms por **0,1% de un core** (50 muestras
/// en 10 s). Lo caro era arrancarlo, no leerlo.
///
/// El proxy hace eso desde #92 y publica `energy_wh` por petición
/// (`telemetry::power`). Este panel sigue siendo otra cosa.
///
/// # Lo que este panel NO es
///
/// **No atribuye a una petición.** Dice «la máquina está a 258 W», no «esta
/// petición costó 2,3 Wh». Eso lo dice la columna del proxy.
///
/// Y no separa cargar el modelo de inferir con él. Para los VATIOS da igual
/// —esa energía se gasta de verdad— pero cualquier cifra derivada por token
/// heredaría la distorsión; para eso están `load_us`/`eval_us`.
#[derive(Debug, Clone, PartialEq)]
struct GpuSample {
    nombre: String,
    util_pct: u16,
    vatios: f64,
    /// Límite de la tarjeta. Es la línea roja del cuentarrevoluciones: sin
    /// ella, un número de vatios no dice si vas holgado o al máximo.
    vatios_max: f64,
    mem_usada_mb: u64,
    mem_total_mb: u64,
    grados: u16,
}

impl GpuSample {
    /// Fracción del límite de potencia, de 0 a 1. Cero si no hay límite
    /// legible: sin denominador no hay aguja, y un `1.0` inventado diría que
    /// la tarjeta está al tope.
    fn fraccion_potencia(&self) -> f64 {
        if self.vatios_max <= 0.0 {
            return 0.0;
        }
        (self.vatios / self.vatios_max).clamp(0.0, 1.0)
    }
}

/// Parsea una línea de `nvidia-smi --format=csv,noheader,nounits`.
///
/// Función PURA para poder afirmar sobre ella sin una GPU delante — que es
/// justamente la máquina donde correrá el CI.
///
/// Devuelve `None` ante cualquier cosa que no sean los siete campos esperados
/// y numéricos. **No rellena con ceros**: un `0%` y `0 W` en el panel se
/// leerían como «la máquina no está haciendo nada», y lo cierto sería «no lo
/// sé». Mismo contrato de ausencia honesta que el resto del monitor.
fn parse_gpu_sample(linea: &str) -> Option<GpuSample> {
    let campos: Vec<&str> = linea.split(',').map(str::trim).collect();
    if campos.len() != 7 || campos[0].is_empty() {
        return None;
    }
    Some(GpuSample {
        nombre: campos[0].to_string(),
        util_pct: campos[1].parse().ok()?,
        vatios: campos[2].parse().ok()?,
        vatios_max: campos[3].parse().ok()?,
        mem_usada_mb: campos[4].parse().ok()?,
        mem_total_mb: campos[5].parse().ok()?,
        grados: campos[6].parse().ok()?,
    })
}

/// Un modelo que ollama tiene RESIDENTE en memoria ahora mismo.
///
/// # Por qué el panel de potencia necesita esto
///
/// El contador dice **cuánto** gasta la máquina. Esto dice **de qué**.
///
/// Y sobre todo resuelve una contaminación del camino **OpenAI-compatible**:
/// por ahí ollama **no expone** `load_duration`, así que `total_ms` y `tok/s`
/// de una petición fría incluyen cargar el modelo sin que nada lo diga.
///
/// Cuánto pesa esa carga depende por completo de cuánto se genere: medido
/// entre el **54%** del tiempo (200 tokens) y el **98%** (un token). Por eso no
/// hay un número que valga como constante — hay un aviso.
///
/// Por la ruta **nativa** (`/api/chat`) sí se separa, con `load_us`. Por la
/// compatible no se puede, pero sí se puede DECIR si el modelo estaba
/// residente.
/// Quien mire el panel sabe entonces si la cifra que está viendo es de
/// inferencia o lleva una carga dentro — que es la diferencia entre un número
/// que se puede usar y uno que engaña.
#[derive(Debug, Clone, PartialEq)]
struct OllamaModel {
    nombre: String,
    vram_bytes: u64,
    /// Segundos hasta que ollama lo descargue. `None` si no trae fecha
    /// legible: el modelo cuenta igual, solo que sin cuenta atrás.
    caduca_en: Option<i64>,
    cuantizacion: Option<String>,
    parametros: Option<String>,
}

/// Parsea la respuesta de `GET /api/ps` de ollama.
///
/// Función PURA: el CI no tiene ollama delante, igual que no tiene GPU.
///
/// Una lista vacía significa **«no hay ningún modelo cargado»**, que es un dato
/// real —la próxima petición pagará la carga— y NO lo mismo que «no se pudo
/// preguntar». Esa distinción la lleva el `Option` de [`App::ollama`], no esta
/// función.
fn parse_ollama_ps(cuerpo: &str) -> Vec<OllamaModel> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(cuerpo) else {
        return Vec::new();
    };
    let Some(models) = v.get("models").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    let ahora = chrono::Utc::now();
    models
        .iter()
        .filter_map(|m| {
            Some(OllamaModel {
                nombre: m.get("name")?.as_str()?.to_string(),
                vram_bytes: m.get("size_vram").and_then(|x| x.as_u64()).unwrap_or(0),
                caduca_en: m
                    .get("expires_at")
                    .and_then(|x| x.as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| (t.with_timezone(&chrono::Utc) - ahora).num_seconds()),
                cuantizacion: m
                    .get("details")
                    .and_then(|d| d.get("quantization_level"))
                    .and_then(|x| x.as_str())
                    .map(str::to_string),
                parametros: m
                    .get("details")
                    .and_then(|d| d.get("parameter_size"))
                    .and_then(|x| x.as_str())
                    .map(str::to_string),
            })
        })
        .collect()
}

/// Pregunta a ollama qué modelos tiene cargados.
///
/// `None` = no se pudo preguntar (no hay ollama, otro servicio en el puerto,
/// timeout). `Some(vec![])` = ollama contestó y **no hay nada cargado**. La
/// distinción importa: lo segundo dice que la próxima petición pagará la carga.
///
/// Medido: 0,09 ms de mediana. Es 265 veces más barato que `nvidia-smi`, así
/// que va en el mismo poll sin discusión.
fn sample_ollama(client: &reqwest::blocking::Client, base: &str) -> Option<Vec<OllamaModel>> {
    let cuerpo = client
        .get(format!("{base}/api/ps"))
        .timeout(Duration::from_millis(500))
        .send()
        .ok()?
        .text()
        .ok()?;
    // Un 200 con basura no es ollama: se trata como "no se pudo preguntar",
    // no como "cero modelos".
    if !cuerpo.contains("models") {
        return None;
    }
    Some(parse_ollama_ps(&cuerpo))
}

/// Campos que se le piden a `nvidia-smi`, en el orden que espera
/// [`parse_gpu_sample`]. Van juntos para que no puedan divergir.
const GPU_QUERY: &str =
    "name,utilization.gpu,power.draw,power.limit,memory.used,memory.total,temperature.gpu";

/// Lee la GPU invocando `nvidia-smi`. `None` si no está instalado, si falla, o
/// si su salida no es la esperada — nunca una muestra fabricada.
///
/// Solo la llama el poll cuando el panel está VISIBLE: lo que no se enseña no
/// se paga, y de paso una GPU con el driver colgado no congela un TUI en el
/// que nadie está mirando ese panel.
fn sample_gpu() -> Option<GpuSample> {
    let salida = std::process::Command::new("nvidia-smi")
        .args([
            &format!("--query-gpu={GPU_QUERY}"),
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !salida.status.success() {
        return None;
    }
    // Primera tarjeta: con varias GPUs esto mide una, y el panel lo dice
    // nombrándola. Agregar varias sin decir cuál sería peor que enseñar una.
    parse_gpu_sample(String::from_utf8_lossy(&salida.stdout).lines().next()?)
}
/// Cuántas muestras se recuerdan por modelo para los sparklines (~2 min a 1
/// muestra/seg). Acotado para no crecer sin límite en una sesión larga.
const HISTORY_CAP: usize = 120;

/// Busca un flag saltándose SIEMPRE `argv[0]`, igual que en `main.rs`. Está
/// duplicado a propósito: el crate no tiene target de librería, así que los
/// dos binarios no comparten código. Duplicar seis líneas es preferible a
/// introducir un `lib.rs` solo para esto.
fn wants_flag(args: &[String], long: &str, short: &str) -> bool {
    args.iter().skip(1).any(|a| a == long || a == short)
}

fn usage_text() -> String {
    format!(
        "oxidegate-monitor {version} — panel en vivo sobre un proxy OxideGate

USO:
    oxidegate-monitor                TUI interactiva (necesita un terminal)
    oxidegate-monitor --once         Vuelca el estado en texto plano y sale
    oxidegate-monitor --url <url>    Apunta a un /stats concreto
    oxidegate-monitor --help         Muestra esta ayuda
    oxidegate-monitor --version      Muestra la versión

    --once es el único modo que funciona sin TUI: úsalo por pipe, en CI, o
    cuando no haya un TTY detrás.

DE DÓNDE SACA LA URL DE /stats (por orden de prioridad):
    1. --url <url>
    2. OXIDEGATE_STATS_URL
    3. http://127.0.0.1:$OXIDEGATE_PORT/stats  (puerto por defecto 8080, el
       mismo que usa el proxy: con ambos en la misma OXIDEGATE_PORT no hace
       falta configurar nada)

    La URL de /requests se deriva de la anterior, salvo que
    OXIDEGATE_REQUESTS_URL la fije explícitamente.
",
        version = env!("CARGO_PKG_VERSION")
    )
}

fn version_text() -> String {
    format!("oxidegate-monitor {}", env!("CARGO_PKG_VERSION"))
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Antes de `setup_terminal()`: pedir ayuda sin un TTY detrás fallaba con
    // "No such device or address", que hacía el binario indescriptible desde
    // un pipe o un script.
    if wants_flag(&args, "--help", "-h") {
        print!("{}", usage_text());
        return Ok(());
    }
    if wants_flag(&args, "--version", "-V") {
        println!("{}", version_text());
        return Ok(());
    }

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
fn resolve_requests_url_inner(
    stats_url: &str,
    requests_url_env: Option<String>,
    port_env: Option<String>,
) -> String {
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
            println!(
                "{:<10} {:<20} {:>6} {:>8} {:>9} {:>10} {:>8}",
                "PROVEEDOR", "MODELO", "REQ", "tok/s", "TTFT ms", "coste $", "err%"
            );
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
            // una conversación: imprime VARIAS vistas (Latency, Context y
            // Toll), no una sola, cada una con su propio header — el usuario
            // no tiene forma de "apretar `c`" en un snapshot que ya salió.
            //
            // `Cache` sigue sin salir acá, y es una asimetría heredada, no una
            // decisión: el mismo argumento le aplica igual.
            println!("--- vista: latency ---");
            print_requests_table(&rows);
            println!();
            println!("--- vista: context ---");
            print_context_table(&rows);
            println!();
            println!("--- vista: toll ---");
            print_toll_table(&rows);
            println!();
            print_tools_table(&rows);
            println!();
            print_quota_table(&rows);
            println!();
            print_sessions_table(url);
        }
        Err(e) => {
            println!(
                "/requests no disponible en {requests_url} ({e}) — puede ser una build del proxy anterior a este endpoint"
            );
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

/// Marca de la fila seleccionada en la tabla de modelos.
///
/// Va ADEMÁS del color de fondo, no en su lugar: el fondo azul se pierde en
/// un terminal sin color, en una captura de texto y en un `TERM=dumb`, y en
/// esos casos la tabla vuelve a no decir qué está seleccionado — que es
/// justamente el problema que este símbolo cierra.
const SELECTION_SYMBOL: &str = "▶ ";

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
    /// Energía BRUTA que la máquina consumió MIENTRAS la petición estuvo
    /// abierta, reposo incluido, y el reposo equivalente de esa misma ventana
    /// (espejo de `RecentRequest::energy_wh` / `energy_idle_wh`).
    ///
    /// **La columna pinta la NETA**, `bruta − reposo`, porque es la cifra que
    /// se compara con `usd`: lo atribuible al trabajo. Las dos llegan por
    /// separado a propósito — el proxy no resta por dentro, y el monitor
    /// enseña la resta hecha pero no la inventa: si falta cualquiera de las
    /// dos, la celda es el guion de dato ausente.
    ///
    /// `None` con upstream remoto, sin `nvidia-smi`, o con un proxy anterior
    /// a este campo. El monitor NO distingue esos casos y no lo intenta.
    energy_wh: Option<f64>,
    energy_idle_wh: Option<f64>,
    /// Pico de potencia dentro de la ventana, en vatios. No se pinta en la
    /// tabla —el panel `g` ya enseña la aguja— pero llega para que la fila
    /// esté completa frente al contrato de `/requests`.
    #[allow(dead_code)]
    power_peak_w: Option<f64>,
    /// Cuántas muestras REALES sostienen la energía de esta fila.
    ///
    /// No tiene columna propia porque es una advertencia SOBRE el dato, no
    /// otro dato. Por debajo de [`MUESTRAS_MINIMAS`] la celda `Wh_net` sale
    /// con `~` delante: el número viene de interpolar entre las dos muestras
    /// que rodean una petición más corta que la cadencia del muestreador.
    /// Sigue siendo honesto, pero pintarlo con la misma cara que uno sostenido
    /// por veinte muestras sería fingir una precisión que no hay.
    energy_samples: Option<u32>,
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
    /// Bytes del cuerpo de la petición. Denominador del porcentaje de peaje
    /// en la vista `Toll`: es lo que de verdad se paga, no solo lo medido.
    prompt_bytes: Option<usize>,
    /// Los tres bloques del PEAJE FIJO, lo que se paga antes de escribir una
    /// palabra y en CADA petición de la sesión.
    ///
    /// Mismo contrato `None` que el resto de espejos: «no se pudo ver», nunca
    /// cero. La vista `Toll` los pinta con el guion de dato ausente, porque un
    /// `0` ahí se leería como «este bloque es gratis» — que es justo la
    /// conclusión contraria a la correcta.
    instructions: Option<InstructionsRow>,
    hooks: Option<HooksRow>,
    skills: Option<SkillsRow>,
    /// Qué cubo cayó dentro del prefijo cacheado, ESTIMADO por el proxy
    /// (espejo de `RecentRequest::cache_by_section`).
    ///
    /// `None` cubre dos casos que el monitor NO puede distinguir y por eso no
    /// intenta: un proxy anterior a este campo, y un proxy actual que no pudo
    /// atribuir. Los dos se pintan igual —con el guion de dato ausente— porque
    /// inventar una diferencia que no se puede observar sería peor que no
    /// mostrarla.
    cache_by_section: Option<CacheBySectionRow>,
    /// Fracción del input PAGADO por sección. Mismo contrato `None` que
    /// `cache_by_section`, del que depende.
    input_share_by_section: Option<SectionShareRow>,
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
    /// Microsegundos dentro del ESCANEO de la respuesta: la otra mitad del
    /// overhead propio del proxy. Mismo contrato `None` que
    /// [`Self::prepare_us`] y por el mismo motivo — un proxy anterior a este
    /// campo no lo manda, y eso es ausencia real, no un cero.
    scan_us: Option<u64>,

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
    /// Señal de carga diferida de herramientas (`tool_search`) del dialecto
    /// OpenAI/Codex Responses (ver [`ToolSearchRow`] y, del lado del proxy,
    /// `provider::ToolSearchSignal`). El diferenciador eager-vs-lazy por
    /// cliente. `None` en Anthropic/Gemini/OpenAI-Chat (no aplica), si el body
    /// no parseó, o si el proxy es de una build anterior a este campo y ni
    /// manda la clave — mismo criterio que el resto de los campos opcionales.
    /// Se renderiza con [`tsearch_cell`] en la vista Context.
    tool_search: Option<ToolSearchRow>,
    /// Señal de honestidad sobre la atribución de `tools_by_server` (espejo de
    /// `telemetry::logger::RequestMetric::tools_flattened`). `Some(true)` avisa
    /// de que el cubo `(native)` de esta fila puede ocultar MCP aplanado
    /// (`pi`/`opencode`, que no usan `mcp__`); `Some(false)` = `(native)`
    /// verificado; `None` = no aplica o proxy viejo. Se renderiza con
    /// [`flattened_cell`] en la vista Context.
    tools_flattened: Option<bool>,
    /// Estado de cuota de suscripción Codex de esta petición puntual (ver
    /// [`CodexQuotaRow`]). `Some` únicamente si la petición se enrutó al
    /// backend de Codex vía OAuth y el upstream mandó al menos una cabecera
    /// `x-codex-*`; `None` para el resto del tráfico (Anthropic, Gemini,
    /// OpenAI vía API key) y para un proxy anterior a esta captura. Fuente
    /// del panel de cuota (tecla `u`, ver [`find_quota_source_row`]).
    codex_quota: Option<CodexQuotaRow>,
}

/// Fila de `GET /sessions`: espejo local y liviano de `SessionStatsRow`.
///
/// `source` e `is_session` viajan juntos a propósito: una `key` bajo
/// `unattributed` es el `User-Agent`, **no una identidad**, y agrupa a TODAS
/// las sesiones no atribuidas de ese harness. Ver `docs/telemetry-by-session.md`.
#[derive(Debug, Clone, Deserialize)]
struct SessionRow {
    source: String,
    key: String,
    /// `false` en el cubo de fallback. El panel lo marca para que nadie lo
    /// lea como una sesión más.
    #[serde(default)]
    is_session: bool,
    #[serde(default)]
    requests: u64,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cost_usd: f64,
}

/// Respuesta completa de `GET /sessions`.
#[derive(Debug, Clone, Deserialize, Default)]
struct SessionsPayload {
    #[serde(default)]
    sessions: Vec<SessionRow>,
    /// `true` si el proxy dejó de admitir claves nuevas: las filas son una
    /// COTA INFERIOR, no el total.
    #[serde(default)]
    saturated: bool,
}

/// URL de `/sessions`, derivada del `/stats` configurado.
fn resolve_sessions_url(stats_url: &str) -> String {
    resolve_sessions_url_inner(
        stats_url,
        std::env::var("OXIDEGATE_SESSIONS_URL").ok(),
        std::env::var("OXIDEGATE_PORT").ok(),
    )
}

/// Núcleo puro de [`resolve_sessions_url`], con el entorno ya leído: misma
/// razón que en `resolve_requests_url_inner` (testear sin mutar `std::env`).
fn resolve_sessions_url_inner(
    stats_url: &str,
    sessions_url_env: Option<String>,
    port_env: Option<String>,
) -> String {
    if let Some(url) = sessions_url_env {
        return url;
    }
    if let Some(prefix) = stats_url.strip_suffix("/stats") {
        return format!("{prefix}/sessions");
    }
    let port = port_env.unwrap_or_else(|| "8080".to_string());
    format!("http://127.0.0.1:{port}/sessions")
}

/// Líneas del panel de sesión.
///
/// Función pura para poder fijar por test lo que de verdad importa: que un
/// cubo de fallback NO se lea como una sesión, y que la saturación se declare.
fn session_lines(rows: &[SessionRow], saturated: bool) -> Vec<String> {
    let mut out = Vec::new();

    if saturated {
        out.push(
            "⚠ saturado: se dejaron de admitir sesiones nuevas — estas cifras son una cota inferior"
                .to_string(),
        );
    }

    if rows.is_empty() {
        out.push("sin sesiones medidas todavía".to_string());
        return out;
    }

    for r in rows.iter().take(6) {
        let marca = if r.is_session {
            String::new()
        } else {
            "  [sin atribuir]".to_string()
        };
        out.push(format!(
            "{:<22} {:<13} {:>4} req  {:>7} in  {:>6} out  ${:.4}{}",
            truncate_client(Some(r.key.as_str())),
            r.source,
            r.requests,
            r.input_tokens,
            r.output_tokens,
            r.cost_usd,
            marca
        ));
    }

    if rows.len() > 6 {
        out.push(format!("… y {} sesiones más", rows.len() - 6));
    }
    out
}

/// Bloque de instrucciones del usuario: espejo de
/// `provider::instructions::InstructionsBlock`.
///
/// `format` NO se deserializa: hoy solo hay una variante y la vista no
/// ramifica por dialecto. Cuando haya más de una, entra aquí — antes no.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct InstructionsRow {
    bytes: usize,
}

/// Salida de los hooks de `SessionStart`: espejo de
/// `provider::hooks::HooksBlock`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct HooksRow {
    bytes: usize,
    /// Marcas `hook success:` contadas. Se pinta al lado de los bytes porque
    /// «19 kB en 3 hooks» acciona y «19 kB» no: dice si el peaje viene de uno
    /// caro o de muchos baratos.
    declared: usize,
}

/// Listado de skills declaradas: espejo de `provider::skills::SkillsBlock`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct SkillsRow {
    listing_bytes: usize,
    /// Entradas del listado. Mismo motivo que en [`HooksRow::declared`].
    declared: usize,
}

/// Bytes de cada sección que cayeron dentro del prefijo cacheado: espejo de
/// `telemetry::cache_attribution::CacheBySection`.
///
/// **Es una ESTIMACIÓN del proxy, no una medición**, y el monitor la trata
/// como tal: vive en su propia vista y nunca se mezcla con las columnas de
/// `Context`, que son bytes medidos. Ver `docs/telemetry-per-request.md` §4.11.
///
/// `method` se deserializa para poder MOSTRARLO, no para ramificar: si el
/// proxy cambia de algoritmo, quien mira la tabla tiene que poder verlo sin
/// que el monitor decida nada por su cuenta.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct CacheBySectionRow {
    /// Algoritmo con el que el proxy estimó el reparto (`prefix_walk_v1`).
    method: String,
    tools_cached_bytes: usize,
    system_cached_bytes: usize,
    history_cached_bytes: usize,
    /// Bytes del ÚLTIMO TURNO atribuidos a caché. Debería ser 0 casi siempre
    /// —el turno nuevo es contenido nuevo—, así que un valor alto y sostenido
    /// es la señal de que el método dejó de describir el tráfico. Se pinta
    /// como columna propia justamente para que se pueda vigilar.
    last_turn_cached_bytes: usize,
    #[allow(dead_code)]
    other_cached_bytes: usize,
}

/// Fracción del input pagado por sección: espejo de
/// `telemetry::section_share::SectionShare`.
///
/// Se deserializa `method` para MOSTRARLO, no para ramificar. Son fracciones
/// de 0 a 1: el monitor las pinta como porcentaje y nunca las multiplica por
/// nada — convertirlas en dinero es decisión de quien mira, no del panel.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct SectionShareRow {
    #[allow(dead_code)]
    method: String,
    tools_share: f64,
    system_share: f64,
    history_share: f64,
    last_turn_share: f64,
    #[allow(dead_code)]
    other_share: f64,
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

/// Señal de carga diferida de herramientas: espejo local y liviano de
/// `provider::ToolSearchSignal` (ver ese tipo en el proxy para el contrato
/// completo). Solo se MUESTRA (vía [`tsearch_cell`]), nunca se decide nada en
/// base a sus valores. A diferencia de `deferred_tools` (que mide
/// `defer_loading` en `tools[]`, siempre eager en este dialecto), esta señal
/// mide los items `tool_search_*` de `input[]`: el único sitio donde el
/// dialecto Responses/Codex expone qué se difirió de verdad.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct ToolSearchRow {
    /// `true` si el body traía algún item `tool_search_call`/`tool_search_output`
    /// en `input[]`: comportamiento LAZY confirmado en esta petición. `false`
    /// en un request Responses/Codex sin esos items: EAGER confirmado.
    used: bool,
    /// Cuántas herramientas con `defer_loading: true` se cargaron vía
    /// `tool_search_output`. `0` cuando `used == false`, y también posible con
    /// `used == true` si solo hubo un `tool_search_call` sin output.
    deferred_loaded: usize,
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
fn fetch_requests(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<Vec<RequestRow>, String> {
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
    if value.is_finite() { Some(value) } else { None }
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
    if gen_ms > 0.0 { Some(gen_ms) } else { None }
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
        groups
            .entry((r.upstream.clone(), r.model.clone()))
            .or_default()
            .push(i);
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
        let Some(tokens) = prompt_tokens_total(&rows[i]) else {
            continue;
        };
        let Some(bytes) = rows[i].context_measured_bytes else {
            continue;
        };
        by_token.entry(tokens).or_default().push((i, bytes));
    }

    for samples in by_token.values() {
        // Hacen falta >= 2 muestras para EXCLUIR la coincidencia: un solo
        // sample con ese total de tokens no prueba nada (podría ser
        // genuinamente el tamaño real del prompt).
        if samples.len() < 2 {
            continue;
        }

        let min_bytes = samples
            .iter()
            .map(|(_, b)| *b)
            .min()
            .expect("samples no está vacío (len >= 2)");
        let max_bytes = samples
            .iter()
            .map(|(_, b)| *b)
            .max()
            .expect("samples no está vacío (len >= 2)");
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
    let values: Vec<f64> = indices
        .iter()
        .filter_map(|&i| rows[i].ttft_ms)
        .filter(|v| v.is_finite())
        .collect();

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
fn classify_slow_generation(
    rows: &[RequestRow],
    indices: &[usize],
    result: &mut [Vec<OutlierKind>],
) {
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

        let hits = others
            .iter()
            .filter(|&&j| rows[j].cache_read_tokens.is_some_and(|v| v > 0))
            .count();
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
fn compute_window_delta(
    baseline: &RawCounters,
    current: &RawCounters,
    elapsed_secs: f64,
) -> WindowDelta {
    let d_requests = current.requests.saturating_sub(baseline.requests);
    let d_output_tokens = current.output_tokens.saturating_sub(baseline.output_tokens);
    let d_input_tokens = current.input_tokens.saturating_sub(baseline.input_tokens);
    let d_cache_read = current
        .cache_read_tokens
        .saturating_sub(baseline.cache_read_tokens);
    let d_cache_write = current
        .cache_write_tokens
        .saturating_sub(baseline.cache_write_tokens);
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
    rows.iter()
        .rev()
        .find(|r| r.tools_by_server.as_ref().is_some_and(|v| !v.is_empty()))
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
fn diff_against_baseline(
    current: &[ToolServerRow],
    baseline: Option<&BTreeMap<String, usize>>,
) -> Vec<ServerDiffRow> {
    let mut result: Vec<ServerDiffRow> = current
        .iter()
        .map(|row| {
            let delta =
                baseline.map(|b| row.bytes as i64 - *b.get(&row.server).unwrap_or(&0) as i64);
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
        let mut disappeared: Vec<(&String, &usize)> = baseline
            .iter()
            .filter(|(name, _)| !current.iter().any(|r| &r.server == *name))
            .collect();
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
    format!(
        "{}{}",
        "█".repeat(filled),
        "·".repeat(QUOTA_BAR_WIDTH - filled)
    )
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
    let base = chrono::DateTime::parse_from_rfc3339(source_timestamp)
        .ok()?
        .timestamp();
    base.checked_add(after as i64)?.checked_sub(now)
}

/// Formatea segundos restantes como texto humano de las dos unidades más
/// significativas (`"resetea en 6d 8h"`, `"resetea en 3h 12m"`, `"resetea en
/// 45m"`). `remaining <= 0` ⇒ `"resetea ahora"` (el reset ya pasó o es
/// inminente, nunca un contador negativo). `None` ⇒ `"—"`, sin countdown
/// fabricado.
fn format_reset_countdown(remaining: Option<i64>) -> String {
    let Some(remaining) = remaining else {
        return "—".to_string();
    };
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
            let window = quota
                .primary_window_minutes
                .map(|m| format!("{m}m"))
                .unwrap_or_else(|| "—".to_string());
            lines.push(format!(
                "primaria: {} {pct}% · ventana {window}",
                quota_bar(pct)
            ));
        }
        None => lines.push("primaria: —".to_string()),
    }

    if quota.secondary_window_minutes.is_some_and(|m| m > 0) {
        match quota.secondary_used_percent {
            Some(pct) => {
                let window = quota
                    .secondary_window_minutes
                    .map(|m| format!("{m}m"))
                    .unwrap_or_else(|| "—".to_string());
                lines.push(format!(
                    "secundaria: {} {pct}% · ventana {window}",
                    quota_bar(pct)
                ));
            }
            None => lines.push("secundaria: —".to_string()),
        }
    }

    lines.push(format_reset_countdown(quota_reset_remaining(
        quota,
        source_timestamp,
        now,
    )));

    if quota.credits_has_credits == Some(true) {
        if quota.credits_unlimited == Some(true) {
            lines.push("créditos: ilimitados".to_string());
        } else {
            lines.push(format!(
                "créditos: {}",
                quota.credits_balance.as_deref().unwrap_or("—")
            ));
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
    /// `msgs`), más `cliente` (`RequestRow::client`), `tsearch`
    /// (`RequestRow::tool_search`, el diferenciador eager-vs-lazy — ver
    /// [`tsearch_cell`]) y `flat` (`RequestRow::tools_flattened`, honestidad
    /// del cubo `(native)` — ver [`flattened_cell`]). `B/tok` es
    /// [`bytes_per_token`]: bytes medidos por token de prompt, el
    /// denominador correcto según dialecto de `upstream`. `cliente` va ACÁ y
    /// no en `Latency` porque el caso que motiva es correlacionar un salto
    /// en `tools`/`total` con atribución de cliente (`docs/optimizer-tool-search.md`
    /// §3): un harness que rompe su propio diferido de tools al pasar por un
    /// `ANTHROPIC_BASE_URL` no-first-party es la firma del impuesto de
    /// contexto que el proxy induce por su propia presencia, no una anomalía
    /// sin explicación.
    Context,
    /// Columnas de la ATRIBUCIÓN DE CACHÉ por sección (`cache_by_section`):
    /// qué cubo cayó dentro del prefijo cacheado, y qué fracción de cada uno.
    ///
    /// **Vive aparte de `Context` a propósito, no por falta de sitio.** Las
    /// columnas de `Context` son bytes MEDIDOS; estas son una ESTIMACIÓN del
    /// proxy (§4.11). Ponerlas en la misma tabla invitaría a leerlas con la
    /// misma confianza, que es exactamente el error que el campo anidado del
    /// lado del proxy existe para evitar. La separación de vistas es la misma
    /// decisión, llevada a la UI.
    ///
    /// Incluye `lt$` (`last_turn_cached_bytes`) aunque casi siempre valga
    /// cero: es el FALSADOR del método. Un valor alto y sostenido ahí
    /// significa que el paseo por el prefijo dejó de describir el tráfico, y
    /// una columna que solo se mira cuando algo va mal no sirve si no está
    /// siempre visible.
    Cache,
    /// Columnas del PEAJE FIJO: los tres bloques que el harness inyecta antes
    /// de que el usuario escriba nada —`instructions` (48%), `hooks` (29%) y
    /// `skills` (23%)— con su total y qué fracción de lo pagado son.
    ///
    /// **Vive aparte de `Context` por la misma clase de motivo que `Cache`,
    /// no por falta de sitio** — aunque también la hubiera: `Context` ya son
    /// 164 columnas.
    ///
    /// Las columnas de `Context` PARTICIONAN el prompt: `tools`, `history`,
    /// `system`, `last_turn` y `other` suman el total. Estas no. Son una
    /// ATRIBUCIÓN dentro de esos mismos cubos, así que ponerlas en la misma
    /// tabla invitaría a sumarlas al total y contar los bytes dos veces. La
    /// separación de vistas es lo que impide esa lectura.
    Toll,
}

impl RequestsView {
    /// Cicla a la siguiente vista. Función PURA y TOTAL (cubre las tres
    /// variantes sin rama de error): Latency → Context → Cache → Latency.
    fn next(self) -> Self {
        match self {
            RequestsView::Latency => RequestsView::Context,
            RequestsView::Context => RequestsView::Cache,
            RequestsView::Cache => RequestsView::Toll,
            RequestsView::Toll => RequestsView::Latency,
        }
    }

    /// Etiqueta corta para el título del panel, en minúsculas para
    /// combinar con el resto del texto de estado de la UI.
    fn label(self) -> &'static str {
        match self {
            RequestsView::Latency => "latency",
            RequestsView::Context => "context",
            RequestsView::Cache => "cache",
            RequestsView::Toll => "toll",
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
    /// Offset del viewport de la tabla de modelos. Guarda SOLO la posición
    /// del scroll: la fila seleccionada es `selected`, y se copia acá en cada
    /// dibujado (ver [`draw_table`]). Persistirlo entre frames es lo que hace
    /// que la tabla scrollee como una lista y no salte.
    models_scroll: TableState,
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
    /// Si el panel de requests recientes se estrecha al modelo seleccionado
    /// en la tabla (tecla `f`). Arranca APAGADO: el panel es un feed global
    /// por defecto, y estrecharlo es una decisión del usuario, no del
    /// monitor.
    ///
    /// ORTOGONAL a `show_requests_panel` y a `requests_view`: filtrar no
    /// abre el panel ni cambia sus columnas.
    filter_requests_by_model: bool,
    /// Visibilidad del panel de "tools por servidor", toggleable con `s`.
    /// INDEPENDIENTE de `show_requests_panel` y de `requests_view`: las tres
    /// teclas (`p`, `c`, `s`) controlan estados ortogonales entre sí.
    show_tools_panel: bool,
    /// Visibilidad del panel de cuota de suscripción Codex, toggleable con
    /// `u`. INDEPENDIENTE de `p`/`c`/`s`: las cuatro teclas controlan
    /// estados ortogonales entre sí.
    show_quota_panel: bool,
    /// Panel de sesión: INDEPENDIENTE del resto, igual que el de cuota.
    show_sessions_panel: bool,
    /// Panel del contador de potencia (`g`). Arranca OCULTO a proposito:
    /// muestrear cuesta ~24 ms por poll, y lo que no se enseña no se paga.
    show_gpu_panel: bool,
    /// Ultima muestra de la GPU. `None` = no se pudo leer, NUNCA un cero.
    gpu: Option<GpuSample>,
    /// Historial de vatios para el sparkline del panel. Mismo cupo que el
    /// resto de historiales del monitor.
    gpu_watts: VecDeque<u64>,
    /// Modelos que ollama tiene residentes. `None` = no se pudo preguntar;
    /// `Some(vec![])` = contestó y no hay ninguno cargado, que es un dato
    /// distinto: la próxima petición pagará la carga.
    ollama: Option<Vec<OllamaModel>>,
    /// Base de ollama. `OLLAMA_HOST` la cambia, igual que hace su propio CLI.
    ollama_url: String,
    sessions: SessionsPayload,
    sessions_url: String,
}

impl App {
    fn new(url: String) -> Self {
        let sessions_url = resolve_sessions_url(&url);
        Self {
            url,
            latest: Vec::new(),
            baseline: None,
            history: HashMap::new(),
            prev_poll: None,
            selected: 0,
            models_scroll: TableState::default(),
            status: "esperando el primer poll...".to_string(),
            recent_requests: Vec::new(),
            requests_status: "esperando el primer poll...".to_string(),
            show_requests_panel: true,
            requests_view: RequestsView::Latency,
            filter_requests_by_model: false,
            show_tools_panel: true,
            show_quota_panel: true,
            show_sessions_panel: true,
            show_gpu_panel: false,
            gpu: None,
            gpu_watts: VecDeque::new(),
            ollama: None,
            ollama_url: std::env::var("OLLAMA_HOST")
                .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string()),
            sessions: SessionsPayload::default(),
            sessions_url,
        }
    }

    /// Hace un fetch de `/stats` y de `/requests` cada tick y actualiza todo
    /// el estado derivado. Ambos fetches son independientes entre sí: si uno
    /// falla, el otro sigue actualizándose con normalidad.
    fn poll(&mut self, client: &reqwest::blocking::Client, url: &str, requests_url: &str) {
        self.poll_stats(client, url);
        self.poll_requests(client, requests_url);
        self.poll_sessions(client);
        self.poll_gpu(client);
    }

    /// Muestrea la GPU, pero SOLO con el panel visible: la lectura cuesta
    /// ~24 ms y no hay motivo para pagarlos por algo que nadie esta mirando.
    /// De paso, una GPU con el driver colgado no congela un TUI cuyo panel de
    /// potencia esta cerrado.
    fn poll_gpu(&mut self, client: &reqwest::blocking::Client) {
        if !self.show_gpu_panel {
            return;
        }
        self.gpu = sample_gpu();
        // 0,09 ms de mediana: 265 veces mas barato que `nvidia-smi`, asi que
        // entra en el mismo poll sin discusion.
        self.ollama = sample_ollama(client, &self.ollama_url);
        // Solo entran vatios REALES: una lectura fallida no empuja un cero al
        // historial, porque el sparkline se leeria como una caida de consumo.
        if let Some(g) = self.gpu.as_ref() {
            self.gpu_watts.push_back(g.vatios.round().max(0.0) as u64);
            if self.gpu_watts.len() > HISTORY_CAP {
                self.gpu_watts.pop_front();
            }
        }
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

    /// Sondea `/sessions`. Degrada con gracia igual que `/requests`: un
    /// proxy anterior a este endpoint responde 404 y el panel lo dice, en vez
    /// de dejar cifras viejas que parecerían actuales.
    fn poll_sessions(&mut self, client: &reqwest::blocking::Client) {
        let url = self.sessions_url.clone();
        match client
            .get(&url)
            .send()
            .and_then(|r| r.json::<SessionsPayload>())
        {
            Ok(p) => self.sessions = p,
            Err(_) => self.sessions = SessionsPayload::default(),
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

    /// Alterna el filtro del panel de requests por el modelo seleccionado
    /// (tecla `f`). No abre el panel si está oculto ni toca sus columnas: es
    /// un estado ortogonal a `p` y a `c`, igual que el resto de toggles.
    fn toggle_requests_filter(&mut self) {
        self.filter_requests_by_model = !self.filter_requests_by_model;
    }

    /// Clave `(upstream, model)` con la que filtrar el panel de requests, o
    /// `None` si el filtro está apagado O si todavía no hay fila
    /// seleccionada.
    ///
    /// Los dos casos devuelven `None` a propósito: filtrar contra una
    /// selección que no existe dejaría el panel vacío sin que el usuario
    /// pueda hacer nada al respecto, y un panel vacío por accidente miente
    /// tanto como un número inventado.
    fn requests_filter_key(&self) -> Option<ModelKey> {
        if !self.filter_requests_by_model {
            return None;
        }
        self.selected_row().map(key_of)
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

    /// Alterna el panel de sesión sin tocar los demás.
    fn toggle_sessions_panel(&mut self) {
        self.show_sessions_panel = !self.show_sessions_panel;
    }

    /// Alterna el contador de potencia. Al ocultarlo se OLVIDA la ultima
    /// muestra: dejarla congelada haria que al reabrir el panel se leyera
    /// como el estado actual cuando es el de hace un rato.
    fn toggle_gpu_panel(&mut self) {
        self.show_gpu_panel = !self.show_gpu_panel;
        if !self.show_gpu_panel {
            self.gpu = None;
            self.gpu_watts.clear();
            self.ollama = None;
        }
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

        let tools_by_server = find_tools_source_row(&self.recent_requests)
            .and_then(|r| r.tools_by_server.as_ref())
            .map(|servers| {
                servers
                    .iter()
                    .map(|s| (s.server.clone(), s.bytes))
                    .collect::<BTreeMap<_, _>>()
            });

        self.baseline = Some(Baseline {
            at: Instant::now(),
            by_key,
            tools_by_server,
        });
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

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    url: &str,
    requests_url: &str,
) -> io::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut app = App::new(url.to_string());
    let mut last_poll = Instant::now() - POLL_INTERVAL; // fuerza un poll inmediato

    loop {
        terminal.draw(|f| ui(f, &mut app))?;

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
                    // `f` de filtro: estrecha el panel de requests al modelo
                    // que esté seleccionado en la tabla.
                    KeyCode::Char('f') => app.toggle_requests_filter(),
                    KeyCode::Char('s') => app.toggle_tools_panel(),
                    KeyCode::Char('u') => app.toggle_quota_panel(),
                    // `e` de sEsión: `s` ya es tools.
                    KeyCode::Char('e') => app.toggle_sessions_panel(),
                    // `g` de GPU: el contador de potencia de la maquina.
                    KeyCode::Char('g') => app.toggle_gpu_panel(),
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
fn ui(f: &mut Frame, app: &mut App) {
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
    if app.show_sessions_panel {
        constraints.push(Constraint::Length(9)); // gasto por sesión
    }
    if app.show_gpu_panel {
        constraints.push(Constraint::Length(8)); // contador de potencia
    }
    constraints.push(Constraint::Length(1)); // footer

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

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
    if app.show_sessions_panel {
        draw_sessions_panel(f, chunks[idx], app);
        idx += 1;
    }
    if app.show_gpu_panel {
        draw_gpu_panel(f, chunks[idx], app);
        idx += 1;
    }
    draw_footer(f, chunks[idx]);
}

/// Línea de modelos residentes para el panel de potencia.
///
/// Tres estados, y los tres significan cosas distintas:
///
/// - **`None`**: no se pudo preguntar a ollama. No se afirma nada.
/// - **Lista vacía**: ollama contestó y **no hay ningún modelo cargado**. Es un
///   dato, no un hueco: la próxima petición pagará la carga, que medida contra
///   ollama fue entre el **54% y el 98%** del tiempo según cuánto se generase.
///   En VATIOS pesa mucho menos —cargar mueve memoria, no calcula— pero en
///   `total_ms` y `tok/s` está entera.
/// - **Con modelos**: lo que está residente, su VRAM y cuánto le queda antes de
///   que ollama lo descargue.
///
/// Ese tercer caso es el que hace legible al contador: **el gauge dice cuánto,
/// esto dice de qué**. Y el segundo es el que avisa de que la siguiente cifra
/// que veas puede llevar una carga dentro sin que nada más lo diga.
fn linea_modelos_residentes(app: &App) -> Line<'static> {
    let Some(modelos) = app.ollama.as_ref() else {
        return Line::from(Span::styled(
            "modelos: sin lectura de ollama",
            Style::default().fg(Color::DarkGray),
        ));
    };
    if modelos.is_empty() {
        return Line::from(Span::styled(
            "modelos: ninguno cargado — la próxima petición paga la carga",
            Style::default().fg(Color::Yellow),
        ));
    }
    let texto: Vec<String> = modelos
        .iter()
        .map(|m| {
            let caduca = match m.caduca_en {
                Some(s) if s > 0 => format!(" ·{}m", s / 60),
                Some(_) => " ·caducado".to_string(),
                None => String::new(),
            };
            let q = m
                .cuantizacion
                .as_deref()
                .map(|q| format!(" {q}"))
                .unwrap_or_default();
            format!(
                "{}{q} {:.1} GB{caduca}",
                m.nombre,
                m.vram_bytes as f64 / 1e9
            )
        })
        .collect();
    Line::from(Span::styled(
        texto.join("  ·  "),
        Style::default().fg(Color::Cyan),
    ))
}

/// Contador de potencia (`g`): qué le está costando a la máquina el modelo que
/// tiene cargado, ahora mismo.
///
/// Es el complemento del coste en dólares. `estimate_cost_usd` traduce tokens a
/// dinero para un proveedor remoto y devuelve `None` para un modelo local —
/// correcto, nadie te factura. Pero **sí pagas**: pagas vatios. Este panel es
/// esa mitad.
///
/// La aguja va sobre el **límite de la tarjeta**, no sobre el pico visto ni
/// sobre un máximo inventado: un cuentarrevoluciones sin línea roja no dice si
/// vas holgado o al tope.
///
/// Sin lectura no se pinta un cero. `-` significa «no lo sé» —no hay
/// `nvidia-smi`, no hay driver, o la salida no era la esperada— y un `0 W`
/// diría «la máquina no está gastando nada», que es una afirmación distinta.
/// Línea de reposo y pico del histórico que el MONITOR lleva visto.
///
/// Es el par que hace legible un número de vatios: sin el reposo no se puede
/// restar nada, y sin el pico no se sabe si 258 W es mucho. El mismo par que
/// el proxy publica por petición (`energy_idle_wh` / `power_peak_w`), pero de
/// otra fuente y con otro alcance — y por eso lo dice.
///
/// **No es el reposo de la tarjeta**: es lo más bajo que el monitor le ha
/// visto desde que se abrió el panel. Si la GPU nunca estuvo ociosa en ese
/// rato, será alto. Decirlo cuesta una palabra y evita leerlo como una
/// especificación del fabricante.
fn linea_reposo_y_pico(app: &App) -> Line<'static> {
    let (Some(min), Some(max)) = (
        app.gpu_watts.iter().copied().min(),
        app.gpu_watts.iter().copied().max(),
    ) else {
        return Line::from(Span::styled(
            "reposo/pico: aún sin histórico",
            Style::default().fg(Color::DarkGray),
        ));
    };
    Line::from(Span::styled(
        format!("reposo {min} W   pico {max} W   (visto por el monitor, no de la tarjeta)"),
        Style::default().fg(Color::DarkGray),
    ))
}

fn draw_gpu_panel(f: &mut Frame, area: Rect, app: &App) {
    let Some(g) = app.gpu.as_ref() else {
        let aviso =
            Paragraph::new("sin lectura de GPU — ¿nvidia-smi instalado? (dato AUSENTE, no cero)")
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" potencia de la máquina "),
                );
        f.render_widget(aviso, area);
        return;
    };

    let bloques = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    let pct = (g.fraccion_potencia() * 100.0).round() as u16;
    // El color es la línea roja: verde holgado, amarillo apretando, rojo al
    // tope. Un número solo no comunica "esto es mucho" de un vistazo.
    let color = match pct {
        0..=59 => Color::Green,
        60..=84 => Color::Yellow,
        _ => Color::Red,
    };
    let texto = vec![
        Line::from(vec![
            Span::styled(
                format!("{:>6.1} W", g.vatios),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" / {:.0} W límite  ({pct}%)", g.vatios_max)),
        ]),
        Line::from(format!(
            "uso {:>3}%   {} °C   VRAM {} / {} MB",
            g.util_pct, g.grados, g.mem_usada_mb, g.mem_total_mb
        )),
        linea_reposo_y_pico(app),
        Line::from(Span::styled(
            g.nombre.clone(),
            Style::default().fg(Color::DarkGray),
        )),
        linea_modelos_residentes(app),
    ];
    f.render_widget(
        Paragraph::new(texto).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" potencia de la máquina "),
        ),
        bloques[0],
    );

    // Historial de vatios. Escala local igual que el resto de sparklines del
    // monitor: sobre 0..límite, un consumo que oscila entre 40 y 260 W en una
    // tarjeta de 320 se vería casi plano.
    let datos: Vec<u64> = app.gpu_watts.iter().copied().collect();
    let escala = sparkline_visible_scale(&datos);
    f.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::ALL).title(format!(
                " vatios (histórico · rango {}..{}) ",
                escala.min, escala.max
            )))
            .data(&escala.data)
            .max(escala.render_max)
            .style(Style::default().fg(color)),
        bloques[1],
    );
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let baseline_age = match &app.baseline {
        Some(b) => format!("baseline hace {}s", b.at.elapsed().as_secs()),
        None => "sin baseline — pulse 'b'".to_string(),
    };

    let text = vec![
        Line::from(vec![
            Span::styled(
                "OxideGate · monitor en vivo",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::raw(&app.url),
        ]),
        Line::from(vec![
            Span::raw(format!("estado: {}", app.status)),
            Span::raw("  |  "),
            Span::raw(baseline_age),
        ]),
    ];

    f.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

/// Título de la tabla de modelos, con la posición dentro de la lista.
///
/// La tabla casi nunca cabe entera: con los paneles abiertos le quedan tres
/// filas de datos. Tres de doce no dicen si quedan modelos por debajo, así
/// que la posición va en el título — es el único sitio que sobrevive al
/// recorte del viewport.
///
/// Función PURA para poder testear el formato sin terminal de por medio,
/// igual que el resto de la matemática de este archivo.
fn models_title(selected: usize, total: usize) -> String {
    if total == 0 {
        return " modelos (total acumulado) ".to_string();
    }
    format!(" modelos ({}/{} · total acumulado) ", selected + 1, total)
}

/// Tabla de agregados por modelo, con VIEWPORT: se dibuja con estado
/// (`TableState`) y no como widget plano.
///
/// La diferencia no es cosmética. Un `Table` sin estado se recorta a la
/// altura del área y ya: con todos los paneles abiertos la tabla se queda en
/// unas pocas filas, y a partir de la última visible la selección se pintaba
/// FUERA de la pantalla — `↑`/`↓` seguían moviéndola y el panel
/// ANTES/DESPUÉS seguía respondiendo, pero la tabla no se enteraba de que
/// tenía que desplazarse. Con estado, ratatui arrastra el offset para que la
/// fila seleccionada esté siempre dentro del viewport, y las primeras filas
/// SALEN de la vista en vez de quedarse ancladas arriba.
///
/// El offset vive en `app.models_scroll` y NO se recrea por frame a
/// propósito: un estado nuevo en cada dibujado pegaría la selección al borde
/// inferior al bajar, en vez de scrollear como una lista normal.
fn draw_table(f: &mut Frame, area: Rect, app: &mut App) {
    let header = Row::new(vec![
        "MODELO",
        "REQ",
        "tok/s",
        "TTFT ms",
        "cache-hit",
        "coste $",
        "redun%",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .latest
        .iter()
        // El resaltado de la fila seleccionada NO se pinta acá comparando
        // contra `app.selected`: lo hace `row_highlight_style` a partir del
        // estado de la tabla, que es el mismo que decide el scroll. Dos
        // mecanismos para la misma decisión se desincronizan en cuanto uno
        // se toca — y ese desajuste era exactamente el bug: la fila creía
        // estar resaltada mientras el viewport la dejaba fuera de pantalla.
        .map(|r| {
            Row::new(vec![
                Cell::from(format!("{}/{}", r.upstream, r.model)),
                Cell::from(r.requests.to_string()),
                Cell::from(format!("{:.1}", r.avg_tokens_per_sec)),
                Cell::from(format!("{:.1}", r.avg_ttft_ms)),
                Cell::from(format!("{:.1}%", r.cache_hit_rate() * 100.0)),
                Cell::from(format!("{:.4}", r.cost_usd)),
                Cell::from(format!("{:.1}%", r.redundancy_rate * 100.0)),
            ])
        })
        .collect();

    let total = app.latest.len();
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
        .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
        .highlight_symbol(SELECTION_SYMBOL)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(models_title(app.selected, total)),
        );

    // La selección se sincroniza en el dibujado y no en `select_prev`/
    // `select_next`: `app.selected` sigue siendo la ÚNICA fuente de verdad
    // (la leen `selected_row`, `selected_delta` y el filtro de requests), y
    // `models_scroll` aporta solo el offset del viewport.
    app.models_scroll
        .select(if total == 0 { None } else { Some(app.selected) });
    f.render_stateful_widget(table, area, &mut app.models_scroll);
}

fn draw_before_after(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" ANTES/DESPUÉS (ventana desde baseline) ");

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
        (Some(_), None) => vec![Line::from(
            "sin baseline (o el modelo no existía al marcarlo) — pulse 'b'",
        )],
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
    let throughput_scale = sparkline_visible_scale(&throughput_data);
    let ttft_scale = sparkline_visible_scale(&ttft_data);

    let throughput_sparkline = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(format!(
            " tok/s (histórico · rango {}..{}) ",
            throughput_scale.min, throughput_scale.max
        )))
        .data(&throughput_scale.data)
        .max(throughput_scale.render_max)
        .style(Style::default().fg(Color::Green));

    let ttft_sparkline = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(format!(
            " TTFT ms (histórico · rango {}..{}) ",
            ttft_scale.min, ttft_scale.max
        )))
        .data(&ttft_scale.data)
        .max(ttft_scale.render_max)
        .style(Style::default().fg(Color::Yellow));

    f.render_widget(throughput_sparkline, chunks[0]);
    f.render_widget(ttft_sparkline, chunks[1]);
}

struct SparklineScale {
    data: Vec<u64>,
    render_max: u64,
    min: u64,
    max: u64,
}

/// Convierte una serie absoluta en una serie visible para sparkline.
///
/// Ratatui dibuja el sparkline desde cero. Eso funciona cuando el cero tiene
/// significado visual, pero falla para métricas que viven siempre en una banda
/// alta: `980, 990, 1000` se ve como una pared llena, aunque haya forma real.
/// Para que el panel muestre QUÉ HACE la señal, desplazamos la serie al mínimo
/// observado y renderizamos el rango local con un 25% de aire superior. El
/// título conserva el rango absoluto para no mentir sobre la magnitud real.
fn sparkline_visible_scale(data: &[u64]) -> SparklineScale {
    let min = data.iter().copied().min().unwrap_or(0);
    let max = data.iter().copied().max().unwrap_or(0);

    if data.is_empty() || max == 0 {
        return SparklineScale {
            data: vec![0; data.len()],
            render_max: 1,
            min,
            max,
        };
    }

    if min == max {
        return SparklineScale {
            data: vec![1; data.len()],
            render_max: 2,
            min,
            max,
        };
    }

    let shifted: Vec<u64> = data.iter().map(|v| v.saturating_sub(min)).collect();
    let range = max.saturating_sub(min);
    let render_max = range.saturating_add((range / 4).max(1));

    SparklineScale {
        data: shifted,
        render_max,
        min,
        max,
    }
}

/// Panel de requests recientes, más nuevo arriba (ver comentario de
/// inversión más abajo), con marcadores de outlier por fila. Nunca indexa el
/// área sin antes chequear que tiene alto/ancho positivo: en una terminal
/// muy chica el `Constraint::Length(12)` de arriba puede terminar recortado
/// a un área de 0 filas, y `Layout::split` sobre un área vacía no debe
/// panickear el render.
/// ¿Esta fila de `/requests` pertenece al `(upstream, model)` dado?
///
/// El upstream forma parte de la comparación porque forma parte de la clave:
/// el mismo nombre de modelo servido por dos proveedores son DOS filas
/// distintas en `/stats`, y mezclarlas acá haría que el filtro enseñara
/// tráfico que no es el que seleccionaste.
///
/// El caso `unknown` no es un apaño: `/stats` agrupa bajo esa clave los
/// requests que fallaron antes de conocer el modelo (ver
/// `StatsAggregator::ingest` en `telemetry/stats.rs`), mientras que
/// `/requests` los deja con `model: null`. Son el MISMO tráfico con dos
/// nombres, y sin traducir esa equivalencia seleccionar la fila `unknown`
/// daría un panel vacío para siempre.
fn request_matches_model(r: &RequestRow, key: &ModelKey) -> bool {
    let (upstream, model) = key;
    if &r.upstream != upstream {
        return false;
    }
    match r.model.as_deref() {
        Some(m) => m == model,
        None => model == "unknown",
    }
}

/// Índices de `rows` a pintar en el panel de requests, EN ORDEN DE PINTADO:
/// más reciente arriba, que es como se lee un panel de últimos eventos.
///
/// Devuelve índices sobre el vector ORIGINAL y no filas clonadas para que
/// los marcadores de [`classify_outliers`] —calculados sobre el orden y el
/// conjunto completos— se puedan seguir indexando sin desalinearse.
///
/// El filtro es de PINTADO, no de estadística: la σ que decide qué es un
/// outlier se sigue calculando sobre todo el tráfico reciente. Es
/// deliberado. "Lento" solo significa algo contra el resto del tráfico, y
/// recalcularlo sobre las dos o tres peticiones de un modelo suelto
/// produciría outliers de ruido.
fn visible_request_indices(rows: &[RequestRow], filter: Option<&ModelKey>) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .rev()
        .filter(|(_, r)| filter.is_none_or(|key| request_matches_model(r, key)))
        .map(|(i, _)| i)
        .collect()
}

fn draw_requests_panel(f: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let filter = app.requests_filter_key();
    // El filtro se ANUNCIA en el título. Un panel estrechado y uno con poco
    // tráfico se ven igual, y confundirlos hace pensar que el proxy no está
    // recibiendo nada.
    let filter_label = match (&filter, app.filter_requests_by_model) {
        (Some((upstream, model)), _) => format!(" · SOLO {upstream}/{model} (f)"),
        (None, true) => " · filtro (f) activo pero sin modelo seleccionado".to_string(),
        (None, false) => String::new(),
    };
    let block = Block::default().borders(Borders::ALL).title(format!(
        " requests recientes · vista:{} · {}{} ",
        app.requests_view.label(),
        app.requests_status,
        filter_label
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

        // El buffer llega en orden cronológico (más viejo primero); acá se
        // invierte para mostrar MÁS NUEVO ARRIBA, que es como se lee un
        // panel de "últimos eventos", y se estrecha al modelo seleccionado si
        // el filtro (`f`) está puesto. `classify_outliers` se calculó sobre
        // el orden y el conjunto originales para que las estadísticas del
        // grupo no cambien — ver [`visible_request_indices`].
        let mut indexed = visible_request_indices(&app.recent_requests, filter.as_ref());

        // La tabla reserva su propia primera fila para el header.
        let capacity = (table_area.height - 1) as usize;
        indexed.truncate(capacity);

        let header = requests_table_header(app.requests_view);

        let rows: Vec<Row> = indexed
            .iter()
            .map(|&i| {
                let r = &app.recent_requests[i];
                let kinds = &outliers[i];
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

        // Filtrar puede dejar cero filas, y una caja vacía no distingue "este
        // modelo no ha pedido nada últimamente" de "el panel está roto". Se
        // dice cuál de las dos es.
        if let Some((upstream, model)) = filter.as_ref().filter(|_| rows.is_empty()) {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(
                        "sin peticiones recientes de {upstream}/{model} en el buffer — 'f' quita el filtro"
                    ),
                    Style::default().fg(Color::DarkGray),
                ))),
                table_area,
            );
        } else {
            // Un terminal angosto no alcanza a mostrar todas las columnas del
            // ancho declarado (la vista Context es más ancha que Latency):
            // `ratatui::Table` recorta las columnas que no entran en vez de
            // hacer wrap o panickear, así que no hace falta guard adicional acá
            // más allá de los chequeos de área ya hechos arriba.
            f.render_widget(Table::new(rows, widths).header(header), table_area);
        }
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
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" tools por servidor ");
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
    let servers = source
        .tools_by_server
        .as_ref()
        .expect("find_tools_source_row garantiza tools_by_server Some no vacío");
    let baseline_map = app
        .baseline
        .as_ref()
        .and_then(|b| b.tools_by_server.as_ref());
    let diffs = diff_against_baseline(servers, baseline_map);

    let header = Row::new(vec![
        "servidor",
        "kind",
        "tools",
        "deferred",
        "bytes",
        "% tools",
        "Δ baseline",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let mut rows: Vec<Row> = diffs
        .iter()
        .map(|d| Row::new(tools_row_cells(d, source.context_tools_bytes)))
        .collect();

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
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" cuota codex ");
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
    let quota = source
        .codex_quota
        .as_ref()
        .expect("find_quota_source_row garantiza codex_quota Some");
    let now = chrono::Utc::now().timestamp();
    let lines: Vec<Line> = quota_lines(quota, &source.timestamp, now)
        .into_iter()
        .map(Line::from)
        .collect();

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
    Row::new(requests_table_labels(view)).style(Style::default().add_modifier(Modifier::BOLD))
}

/// Etiquetas de columna de una vista, separadas de [`requests_table_header`]
/// para que un test pueda CONTARLAS.
///
/// `ratatui::Row` no expone cuántas celdas lleva, así que mientras las
/// etiquetas vivieran dentro del constructor el guardián de alineación solo
/// podía comparar anchos contra celdas — y la cabecera, que es la tercera
/// pieza que tiene que cuadrar, quedaba sin comprobar pese a que el nombre del
/// test la nombraba.
fn requests_table_labels<'a>(view: RequestsView) -> Vec<&'a str> {
    match view {
        RequestsView::Latency => {
            vec![
                "hora", "modelo", "st", "status", "in", "out", "c_rd", "c_wr", "ttft_ms", "gen_ms",
                "tok/s", "effort", "spd_req", "spd_got", "usd", "Wh_net", "outlier",
            ]
        }
        RequestsView::Context => {
            vec![
                "hora",
                "modelo",
                "msgs",
                "tools",
                "history",
                "system",
                "last_turn",
                "other",
                "total",
                "tax%",
                "B/tok",
                "prep_us",
                "scan_us",
                "prox%",
                "cliente",
                "tsearch",
                "flat",
                "outlier",
            ]
        }
        RequestsView::Cache => {
            vec![
                "hora", "modelo", "total", "cch%", "tools%", "hist%", "lt$", "$tools", "$hist",
                "$lt", "outlier",
            ]
        }
        RequestsView::Toll => {
            vec![
                "hora", "modelo", "instr", "hooks", "nh", "skills", "nsk", "peaje", "%prom",
                "outlier",
            ]
        }
    }
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
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(18),
            Constraint::Length(7),
            Constraint::Length(5),
            Constraint::Length(14),
        ],
        RequestsView::Cache => vec![
            Constraint::Length(9),
            Constraint::Length(16),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Length(15),
            Constraint::Length(14),
        ],
        // 89 columnas + separadores. Deliberadamente estrecha: cabe donde
        // `Context` (164) no cabe, que es parte del motivo de que exista.
        RequestsView::Toll => vec![
            Constraint::Length(9),
            Constraint::Length(16),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(4),
            Constraint::Length(9),
            Constraint::Length(4),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(14),
        ],
    }
}

/// Fracción de una sección que el proxy estimó dentro del prefijo cacheado.
///
/// Devuelve el guion de dato ausente cuando la sección MIDE cero bytes: no
/// hay fracción que calcular sobre una sección vacía, y un `0%` ahí se leería
/// como "no se cacheó nada" en vez de "no había nada que cachear". Es la misma
/// distinción hueco-vs-cero que el resto del monitor respeta.
fn cached_pct_cell(cached: Option<usize>, total: Option<usize>) -> String {
    match (cached, total) {
        (Some(_), Some(0)) | (None, _) | (_, None) => "-".to_string(),
        (Some(c), Some(t)) => format!("{:.0}%", 100.0 * c as f64 / t as f64),
    }
}

/// Fracción del input pagado, como porcentaje. `-` si no hay reparto.
///
/// Se pinta como porcentaje y NUNCA se multiplica por un coste: convertir esto
/// en dinero es decisión de quien mira, no del panel. Es la misma línea que el
/// proxy traza al llamar al campo `*_share` y no `*_cost`.
fn share_cell(v: Option<f64>) -> String {
    v.map_or_else(|| "-".to_string(), |x| format!("{:.0}%", 100.0 * x))
}

/// Celdas de la vista de atribución de caché.
///
/// Se calcula aparte de [`requests_row_cells`] solo por longitud; la vista
/// sigue siendo una rama más de ese `match`.
/// Celdas de la vista `Toll`: los tres bloques del peaje fijo de una fila.
///
/// Dos reglas gobiernan esta función, y las dos son sobre honestidad, no sobre
/// formato:
///
/// **1. `null` no es cero.** Los tres campos son `Option`, y `None` significa
/// «no se pudo ver». Pintar un `0` diría que ese bloque es gratis — la
/// conclusión contraria a la correcta, y el error que documenta
/// `docs/telemetry-level-1.md`.
///
/// **2. Un total al que le falta un bloque es una COTA INFERIOR.** Se marca con
/// `≥` en vez de presentarse como el peaje. Un número que parece completo y no
/// lo está aconseja peor que no dar ninguno: quien lo lea decidirá si un plugin
/// vale su peaje con una cifra corta sin saberlo.
///
/// Y lo que esta vista NO hace: sumar bytes al contexto. Estos bytes ya los
/// cuentan las columnas `context_*`; aquí se ATRIBUYEN a quién los inyecta. Por
/// eso el porcentaje es sobre `prompt_bytes` —qué fracción de lo pagado es
/// peaje— y no un total nuevo que invitaría a contarlo dos veces.
fn toll_row_cells(r: &RequestRow) -> Vec<String> {
    let instr = r.instructions.as_ref().map(|i| i.bytes);
    let hooks = r.hooks.as_ref().map(|h| h.bytes);
    let skills = r.skills.as_ref().map(|s| s.listing_bytes);

    let presentes: Vec<usize> = [instr, hooks, skills].into_iter().flatten().collect();
    let completo = presentes.len() == 3;
    let total: Option<usize> = if presentes.is_empty() {
        None
    } else {
        Some(presentes.iter().sum())
    };

    let celda_total = match total {
        // El `≥` es la diferencia entre «esto es el peaje» y «esto es lo que
        // he podido ver del peaje».
        Some(t) if completo => format_bytes(t),
        Some(t) => format!("≥{}", format_bytes(t)),
        None => "-".to_string(),
    };

    let celda_pct = match (total, r.prompt_bytes) {
        (Some(t), Some(p)) if p > 0 => format!("{:.1}", (t as f64 / p as f64) * 100.0),
        // Sin denominador no hay fracción. No se sustituye por el total ni por
        // un cero: se declara que no se puede calcular.
        _ => "-".to_string(),
    };

    vec![
        format_time(&r.timestamp),
        truncate_model(r.model.as_deref()),
        opt_bytes(instr),
        opt_bytes(hooks),
        opt_count(r.hooks.as_ref().map(|h| h.declared)),
        opt_bytes(skills),
        opt_count(r.skills.as_ref().map(|s| s.declared)),
        celda_total,
        celda_pct,
        // Sin marcador de outlier: lo agrega el llamador, que lo calcula una
        // sola vez por fila y es común a las cuatro vistas.
    ]
}

/// Celda `prox%`: qué fracción del reloj de la petición se llevó el PROPIO
/// medidor — `(prepare_us + scan_us) / total_ms`.
///
/// Es el número que hace auditable la premisa del proyecto. Todo OxideGate se
/// apoya en que observar sale casi gratis; mientras esa cifra no exista, eso es
/// una creencia y no un dato.
///
/// Exige las DOS mitades. Con una sola no se publica un porcentaje a medias:
/// se marca ausente, porque «el medidor cuesta un 0,1%» calculado sobre la
/// mitad del overhead es exactamente la clase de número tranquilizador y falso
/// que este panel no debe dar.
fn proxy_share_cell(r: &RequestRow) -> String {
    match (r.prepare_us, r.scan_us) {
        (Some(prep), Some(scan)) if r.total_ms > 0.0 => {
            let propio_ms = (prep + scan) as f64 / 1000.0;
            format!("{:.2}", (propio_ms / r.total_ms) * 100.0)
        }
        _ => "-".to_string(),
    }
}

/// Conteo con el mismo guion de dato ausente que [`opt_bytes`]. Un conteo
/// ausente NO es un cero: «no sé cuántos hooks» y «cero hooks» son cosas
/// distintas, y la segunda significa que ese bloque no cuesta nada.
fn opt_count(v: Option<usize>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".to_string())
}

fn cache_row_cells(r: &RequestRow) -> Vec<String> {
    let Some(c) = r.cache_by_section.as_ref() else {
        // Sin atribución no se rellena con ceros: se marca ausente en cada
        // columna. Un proxy viejo y un proxy que no pudo atribuir se ven
        // igual porque el monitor no puede distinguirlos (ver el doc del
        // campo en `RequestRow`).
        return vec![
            format_time(&r.timestamp),
            truncate_model(r.model.as_deref()),
            opt_bytes(r.context_measured_bytes),
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
        ];
    };
    let cacheado = c.tools_cached_bytes
        + c.system_cached_bytes
        + c.history_cached_bytes
        + c.last_turn_cached_bytes
        + c.other_cached_bytes;
    vec![
        format_time(&r.timestamp),
        truncate_model(r.model.as_deref()),
        opt_bytes(r.context_measured_bytes),
        cached_pct_cell(Some(cacheado), r.context_measured_bytes),
        cached_pct_cell(Some(c.tools_cached_bytes), r.context_tools_bytes),
        cached_pct_cell(Some(c.history_cached_bytes), r.context_history_bytes),
        // El FALSADOR, en bytes absolutos y no en porcentaje: lo que importa
        // vigilar es si deja de ser cero, no sobre qué base.
        opt_bytes(Some(c.last_turn_cached_bytes)),
        // Y las tres columnas que justifican la vista: lo mismo, pero en
        // fracción de lo que se PAGA. La distancia entre `tools%` y `$tools`
        // es el hallazgo entero en una línea.
        share_cell(r.input_share_by_section.as_ref().map(|s| s.tools_share)),
        share_cell(r.input_share_by_section.as_ref().map(|s| s.history_share)),
        share_cell(r.input_share_by_section.as_ref().map(|s| s.last_turn_share)),
    ]
}

/// Celdas de datos de una fila (SIN el marcador de outlier, que el llamador
/// agrega al final: es común a las tres vistas y se calcula una sola vez por
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
            energia_neta_cell(r),
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
            opt_u64(r.scan_us),
            proxy_share_cell(r),
            truncate_client(r.client.as_deref()),
            tsearch_cell(r),
            flattened_cell(r),
        ],
        RequestsView::Cache => cache_row_cells(r),
        RequestsView::Toll => toll_row_cells(r),
    }
}

/// Celda `tsearch` (vista Context): el diferenciador eager-vs-lazy del
/// dialecto Responses/Codex, leído de `RequestRow::tool_search`.
///
/// - `None` → `"-"`: el proxy no lo informó (dialecto donde no aplica —
///   Anthropic/Gemini/OpenAI-Chat—, body que no parseó, o build anterior al
///   campo). Mismo criterio de `"-"` que el resto de las celdas opcionales.
/// - `Some { used: false, .. }` → `"eager"`: petición Responses/Codex medida
///   sin carga diferida este turno.
/// - `Some { used: true, deferred_loaded }` → `"lazy:N"`: el cliente ejercitó
///   la búsqueda diferida; `N` es cuántas tools cargó (`0` si solo hubo un
///   `tool_search_call` sin output).
fn tsearch_cell(r: &RequestRow) -> String {
    match &r.tool_search {
        None => "-".to_string(),
        Some(ts) if ts.used => format!("lazy:{}", ts.deferred_loaded),
        Some(_) => "eager".to_string(),
    }
}

/// Celda `flat` (vista Context): honestidad de la atribución de tools, leída de
/// `RequestRow::tools_flattened`.
///
/// - `None` → `"-"`: no aplica (Anthropic/Gemini/Chat usan `mcp__` fiable) o
///   proxy anterior al campo.
/// - `Some(true)` → `"yes"`: el `(native)` de esta fila puede ocultar MCP
///   aplanado — `pi`/`opencode` no usan `mcp__` (medido).
/// - `Some(false)` → `"no"`: hay tools `mcp__` namespaceadas, el `(native)` es
///   de fiar.
fn flattened_cell(r: &RequestRow) -> String {
    match r.tools_flattened {
        None => "-".to_string(),
        Some(true) => "yes".to_string(),
        Some(false) => "no".to_string(),
    }
}

/// Extrae `HH:MM:SS` (UTC) de un timestamp RFC3339. Si el timestamp no
/// parsea (dato corrupto o formato inesperado), devuelve el string crudo tal
/// cual llegó: mejor mostrar el dato raro que ocultarlo con un placeholder
/// engañoso.
fn format_time(timestamp: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(timestamp) {
        Ok(dt) => dt
            .with_timezone(&chrono::Utc)
            .format("%H:%M:%S")
            .to_string(),
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
            let head: String = m
                .chars()
                .take(MODEL_DISPLAY_MAX.saturating_sub(1))
                .collect();
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
            let head: String = c
                .chars()
                .take(CLIENT_DISPLAY_MAX.saturating_sub(1))
                .collect();
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
            let head: String = s
                .chars()
                .take(SPEED_DISPLAY_MAX.saturating_sub(1))
                .collect();
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

/// Advertencia que acompaña SIEMPRE a la columna `Wh_net`.
///
/// Una columna de números que no se puede sumar es una trampa puesta por el
/// instrumento si no lo dice. Dos peticiones solapadas reclaman los mismos
/// vatios: la suma daría más energía de la que la máquina gastó.
/// Muestras por debajo de las cuales la energía se marca como aproximada.
///
/// Con la cadencia del muestreador del proxy (200 ms), una petición de menos
/// de medio segundo puede caer entre dos muestras: la cifra sale de
/// interpolar, no de integrar una curva. Dos es el mínimo para que haya
/// CURVA dentro de la ventana y no solo extremos.
const MUESTRAS_MINIMAS: u32 = 2;

const NOTA_ENERGIA: &str = "energía: Wh_net = lo que la máquina gastó MIENTRAS \
     la petición estuvo abierta, menos el reposo. NO se puede sumar la columna: \
     dos peticiones solapadas reclaman los mismos vatios. `~` = pocas muestras \
     dentro de la ventana, cifra basta. Solo upstream local; el precio del kWh \
     lo pone quien lee (ver docs/telemetry-per-request.md §4.19)";

/// Celda `Wh_net`: la energía ATRIBUIBLE al trabajo de esta petición, que es
/// `energy_wh − energy_idle_wh`.
///
/// # Por qué el monitor resta y el proxy no
///
/// El proxy publica las dos mitades por separado a propósito: la atribución no
/// es limpia —si otra cosa usa la GPU a la vez, la muestra no es solo de la
/// inferencia— y un único número ya restado fingiría una precisión que no hay.
///
/// El monitor SÍ resta, porque una columna de tabla tiene que caber en ocho
/// caracteres y porque la neta es la que se compara con `usd`. Pero solo resta
/// lo que le llega: **si falta cualquiera de las dos mitades la celda es el
/// guion de ausencia**, nunca la bruta disfrazada de neta. El dato completo
/// sigue estando en `/requests`.
///
/// Escala: por debajo de 1 Wh se pinta en **milivatios-hora** con sufijo `m`.
/// Una petición local corta ronda las decenas de mWh, y `0.0000` en la
/// columna se leería como «gratis» en vez de «pequeño».
fn energia_neta_cell(r: &RequestRow) -> String {
    let (Some(bruta), Some(reposo)) = (r.energy_wh, r.energy_idle_wh) else {
        return "-".to_string();
    };
    let neta = bruta - reposo;
    if !neta.is_finite() {
        return "-".to_string();
    }
    // `~` cuando el número lo sostienen menos muestras de las mínimas: es
    // interpolación entre dos puntos de fuera de la ventana, no una curva.
    let aprox = match r.energy_samples {
        Some(n) if n < MUESTRAS_MINIMAS => "~",
        _ => "",
    };
    if neta.abs() >= 1.0 {
        format!("{aprox}{neta:.3}")
    } else {
        format!("{aprox}{:.1}m", neta * 1000.0)
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
        kinds
            .iter()
            .map(|k| k.marker())
            .collect::<Vec<_>>()
            .join("+")
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
        "{:<10} {:<16} {:>2} {:>6} {:>6} {:>6} {:>6} {:>6} {:>8} {:>8} {:>7} {:>7} {:>8} {:>8} {:>8} {:>8} {:<14}",
        "HORA",
        "MODELO",
        "st",
        "status",
        "in",
        "out",
        "c_rd",
        "c_wr",
        "ttft_ms",
        "gen_ms",
        "tok/s",
        "effort",
        "spd_req",
        "spd_got",
        "usd",
        "Wh_net",
        "outlier"
    );
    for (i, r) in rows.iter().enumerate().rev() {
        let cells = requests_row_cells(RequestsView::Latency, r);
        println!(
            "{:<10} {:<16} {:>2} {:>6} {:>6} {:>6} {:>6} {:>6} {:>8} {:>8} {:>7} {:>7} {:>8} {:>8} {:>8} {:>8} {:<14}",
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
            cells[15],
            marker_text(&outliers[i]),
        );
    }
    println!(
        "leyenda: ERR=error(status>=400) · MISS=cache-miss atípico · TTFT=TTFT lento(>=2σ) · SLOW=generación lenta(>=2σ) · TRUNC=tope de tokens (ver docs/monitor-tui.md)"
    );
    println!(
        "nota: effort = output_config.effort pedido; spd_req = speed pedido (raíz); spd_got = usage.speed servido (Anthropic; documentado pero no observado aún en tráfico real)"
    );
    println!("{NOTA_ENERGIA}");
}

/// Imprime la tabla CONTEXT de requests recientes en texto plano (modo
/// `--once`): mismo orden (más nuevo arriba) y mismos marcadores de outlier
/// que [`print_requests_table`], pero con las columnas del desglose de
/// bytes de contexto en vez de las de latencia/tokens. Reusa
/// [`requests_row_cells`] para que esta vista en texto plano y la vista
/// `Context` de la TUI (`draw_requests_panel`) nunca diverjan en qué dato
/// muestra cada columna.
/// Vista `Toll` en texto plano para `--once`.
///
/// Existe por el mismo motivo que [`print_context_table`], escrito en el
/// comentario de `--once`: **quien lee un snapshot ya impreso no puede apretar
/// `c`**. Y este es justo el número que se pega en una conversación para
/// decidir si un plugin vale su peaje, que es la pregunta que motivó la vista.
fn print_toll_table(rows: &[RequestRow]) {
    let outliers = classify_outliers(rows);

    println!(
        "{:<10} {:<16} {:>9} {:>9} {:>4} {:>9} {:>4} {:>10} {:>6} {:<14}",
        "HORA", "MODELO", "instr", "hooks", "nh", "skills", "nsk", "peaje", "%prom", "outlier"
    );
    for (i, r) in rows.iter().enumerate().rev() {
        let cells = requests_row_cells(RequestsView::Toll, r);
        println!(
            "{:<10} {:<16} {:>9} {:>9} {:>4} {:>9} {:>4} {:>10} {:>6} {:<14}",
            cells[0],
            cells[1],
            cells[2],
            cells[3],
            cells[4],
            cells[5],
            cells[6],
            cells[7],
            cells[8],
            marker_text(&outliers[i])
        );
    }
    println!(
        "nota: peaje fijo = lo que el harness inyecta ANTES de que escribas nada, en CADA petición (instructions 48% · hooks 29% · skills 23%)"
    );
    println!(
        "nota: `-` significa NO SE PUDO VER, nunca cero. `≥` en peaje = falta algún bloque, así que es una cota inferior, no el total"
    );
    println!(
        "nota: estos bytes YA los cuentan las columnas de la vista context; aquí se atribuyen a quién los inyecta — no se suman aparte (ver docs/monitor-tui.md §7.3.3)"
    );
}

fn print_context_table(rows: &[RequestRow]) {
    let outliers = classify_outliers(rows);

    println!(
        "{:<10} {:<16} {:>5} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>6} {:>6} {:>8} {:>8} {:>6} {:<18} {:<7} {:<5} {:<14}",
        "HORA",
        "MODELO",
        "msgs",
        "tools",
        "history",
        "system",
        "last_turn",
        "other",
        "total",
        "tax%",
        "B/tok",
        "prep_us",
        "scan_us",
        "prox%",
        "cliente",
        "tsearch",
        "flat",
        "outlier"
    );
    for (i, r) in rows.iter().enumerate().rev() {
        let cells = requests_row_cells(RequestsView::Context, r);
        println!(
            "{:<10} {:<16} {:>5} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>6} {:>6} {:>8} {:>8} {:>6} {:<18} {:<7} {:<5} {:<14}",
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
            cells[15],
            cells[16],
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
    println!("nota: cliente = User-Agent crudo (truncado, ver docs/telemetry-per-request.md)");
    println!(
        "nota: tsearch = carga diferida de tools (dialecto Responses/Codex): eager = sin diferido este turno; lazy:N = cargó N tools vía tool_search; - = no aplica (ver docs/telemetry-per-request.md §4.3)"
    );
    println!(
        "nota: flat = honestidad del cubo (native): yes = el cliente NO usa mcp__ (pi/opencode), el (native) puede ocultar MCP aplanado; no = (native) verificado; - = no aplica (ver docs/telemetry-per-request.md §4.4)"
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
        println!(
            "(sin desglose de tools disponible: proxy anterior a este slice, o ninguna fila declara tools)"
        );
        return;
    };

    println!(
        "fuente: {} · modelo {}",
        format_time(&source.timestamp),
        source.model.as_deref().unwrap_or("-")
    );

    // `find_tools_source_row` garantiza `Some` no vacío.
    let servers = source
        .tools_by_server
        .as_ref()
        .expect("find_tools_source_row garantiza tools_by_server Some no vacío");
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
    println!(
        "{:-<26} {:-<7} {:-<6} {:-<9} {:-<10} {:-<9} {:-<12}",
        "", "", "", "", "", "", ""
    );
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
        println!(
            "(sin datos de cuota: ninguna petición reciente usó el backend de Codex, o el proxy es anterior a la captura de cuota)"
        );
        return;
    };

    // `find_quota_source_row` garantiza `codex_quota` Some.
    let quota = source
        .codex_quota
        .as_ref()
        .expect("find_quota_source_row garantiza codex_quota Some");
    println!(
        "fuente: {} · modelo {}",
        format_time(&source.timestamp),
        source.model.as_deref().unwrap_or("-")
    );
    let now = chrono::Utc::now().timestamp();
    for line in quota_lines(quota, &source.timestamp, now) {
        println!("{line}");
    }
}

/// Vuelca el gasto por sesión en modo `--once` (sin TUI).
///
/// Sondea `/sessions` por su cuenta porque `run_once` no construye un `App`.
/// Degrada con gracia: un proxy anterior a este endpoint da 404 y se dice,
/// en vez de callar y parecer que no hay sesiones.
fn print_sessions_table(stats_url: &str) {
    println!("--- vista: gasto por sesión ---");
    let url = resolve_sessions_url(stats_url);
    let client = reqwest::blocking::Client::new();
    match client
        .get(&url)
        .send()
        .and_then(|r| r.json::<SessionsPayload>())
    {
        Ok(p) => {
            for line in session_lines(&p.sessions, p.saturated) {
                println!("{line}");
            }
            println!(
                "nota: [sin atribuir] = cubo de fallback por User-Agent, NO una sesión — agrupa todas las no atribuidas de ese harness (ver docs/telemetry-by-session.md)"
            );
        }
        Err(e) => println!(
            "(/sessions no disponible en {url} ({e}) — puede ser una build del proxy anterior a este endpoint)"
        ),
    }
}

/// Panel de gasto por sesión: responde "quién gastó", no "qué modelo".
fn draw_sessions_panel(f: &mut Frame, area: Rect, app: &App) {
    let lineas = session_lines(&app.sessions.sessions, app.sessions.saturated);
    let text: Vec<Line> = lineas.into_iter().map(Line::from).collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" gasto por sesión (e) ");
    f.render_widget(Paragraph::new(text).block(block), area);
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let text = Line::from(
        "q salir · b baseline · r reset · ↑/↓ modelo · f solo modelo sel. · p requests · c latency/context · s tools · u cuota · e sesión",
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
    // --- Ayuda y version del CLI (ver `wants_flag` / `usage_text`) ---

    fn argv(rest: &[&str]) -> Vec<String> {
        std::iter::once("oxidegate-monitor".to_string())
            .chain(rest.iter().map(|s| s.to_string()))
            .collect()
    }

    #[test]
    fn monitor_asks_for_help_with_either_spelling() {
        assert!(wants_flag(&argv(&["--help"]), "--help", "-h"));
        assert!(wants_flag(&argv(&["-h"]), "--help", "-h"));
        assert!(!wants_flag(&argv(&["--once"]), "--help", "-h"));
    }

    #[test]
    fn monitor_argv_zero_is_never_a_flag() {
        assert!(!wants_flag(&["-h".to_string()], "--help", "-h"));
    }

    #[test]
    fn monitor_usage_documents_every_knob_it_actually_reads() {
        let text = usage_text();
        // Estas cuatro son las entradas que `resolve_url` y
        // `resolve_requests_url` consultan de verdad. Documentar menos deja
        // al usuario adivinando por que el monitor mira a otro sitio.
        for knob in ["--once", "--url", "OXIDEGATE_STATS_URL", "OXIDEGATE_PORT"] {
            assert!(text.contains(knob), "falta {knob}: {text}");
        }
    }

    #[test]
    fn monitor_usage_mentions_the_headless_mode_first_class() {
        // `--once` es el unico modo que funciona sin TTY. Un usuario en CI o
        // leyendo por pipe necesita encontrarlo sin leer el codigo.
        assert!(usage_text().contains("sin TUI") || usage_text().contains("TTY"));
    }

    #[test]
    fn monitor_version_text_carries_the_crate_version() {
        assert!(version_text().contains(env!("CARGO_PKG_VERSION")));
    }

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
    fn sparkline_visible_scale_desplaza_al_minimo_para_ver_la_forma() {
        // Este es el bug real: con escala 0..1000, una serie 980..1000 se ve
        // como pared llena. El render debe usar el rango local 0..20 y dejar
        // el rango absoluto solo como etiqueta.
        let scale = sparkline_visible_scale(&[980, 1_000, 990]);

        assert_eq!(scale.data, vec![0, 20, 10]);
        assert_eq!(scale.render_max, 25);
        assert_eq!((scale.min, scale.max), (980, 1_000));
    }

    #[test]
    fn sparkline_visible_scale_no_devuelve_cero_para_series_vacias_o_planas() {
        // `Sparkline::max(0)` no aporta escala útil. Incluso sin datos o con
        // ceros mantenemos una escala mínima honesta y estable.
        let empty = sparkline_visible_scale(&[]);
        assert_eq!(empty.data, Vec::<u64>::new());
        assert_eq!(empty.render_max, 1);

        let zeroes = sparkline_visible_scale(&[0, 0, 0]);
        assert_eq!(zeroes.data, vec![0, 0, 0]);
        assert_eq!(zeroes.render_max, 1);
    }

    #[test]
    fn sparkline_visible_scale_dibuja_series_constantes_sin_pared_llena() {
        // Si todo vale lo mismo no hay "forma" que enseñar, pero tampoco debe
        // llenarse toda la caja. Una línea a media altura comunica estabilidad.
        let scale = sparkline_visible_scale(&[42, 42, 42]);

        assert_eq!(scale.data, vec![1, 1, 1]);
        assert_eq!(scale.render_max, 2);
        assert_eq!((scale.min, scale.max), (42, 42));
    }

    #[test]
    fn sparkline_visible_scale_suma_un_minimo_para_rangos_chicos() {
        // En rangos 1..3, un 25% entero sería 0. El margen mínimo evita volver
        // al mismo problema de saturación por redondeo.
        let scale = sparkline_visible_scale(&[7, 8]);

        assert_eq!(scale.data, vec![0, 1]);
        assert_eq!(scale.render_max, 2);
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

    fn raw(
        requests: u64,
        output_tokens: u64,
        ttft_sum: f64,
        ttft_count: u64,
        cost: f64,
    ) -> RawCounters {
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
        let args = vec![
            "monitor".to_string(),
            "--url".to_string(),
            "http://x:1/stats".to_string(),
        ];
        assert_eq!(resolve_url(&args), "http://x:1/stats");
    }

    // -----------------------------------------------------------------
    // resolve_requests_url_inner — precedencia, sin tocar std::env
    // -----------------------------------------------------------------

    #[test]
    fn resolve_requests_url_deriva_del_stats_url_del_flag_override() {
        // Caso `--url http://x:1/stats`: la URL de /requests se deriva
        // reemplazando el sufijo /stats por /requests.
        assert_eq!(
            resolve_requests_url_inner("http://x:1/stats", None, None),
            "http://x:1/requests"
        );
    }

    #[test]
    fn resolve_requests_url_usa_env_override_si_esta_presente() {
        assert_eq!(
            resolve_requests_url_inner(
                "http://x:1/stats",
                Some("http://y:2/requests".to_string()),
                None
            ),
            "http://y:2/requests"
        );
    }

    #[test]
    fn resolve_requests_url_env_override_gana_aunque_stats_url_termine_en_stats() {
        // El override explícito tiene prioridad sobre la derivación por
        // sustitución, incluso si esta última sería válida.
        assert_eq!(
            resolve_requests_url_inner(
                "http://x:1/stats",
                Some("http://z:3/requests".to_string()),
                Some("9999".to_string())
            ),
            "http://z:3/requests"
        );
    }

    #[test]
    fn resolve_requests_url_fallback_si_stats_url_no_termina_en_stats() {
        assert_eq!(
            resolve_requests_url_inner("http://x:1/weird", None, None),
            "http://127.0.0.1:8080/requests"
        );
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
            energy_wh: None,
            energy_idle_wh: None,
            power_peak_w: None,
            energy_samples: None,
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
            prompt_bytes: Some(100_000),
            instructions: None,
            hooks: None,
            skills: None,
            cache_by_section: None,
            input_share_by_section: None,
            prepare_us: Some(850),
            scan_us: Some(150),
            tools_by_server: None,
            tools_overhead_bytes: None,
            tool_search: None,
            tools_flattened: None,
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
            req(
                "anthropic",
                "claude-opus-4",
                200,
                Some(10.0),
                100.0,
                Some(50),
                Some(10),
            ),
            req(
                "anthropic",
                "claude-opus-4",
                200,
                Some(10.0),
                100.0,
                Some(50),
                Some(10),
            ),
            req(
                "anthropic",
                "claude-opus-4",
                200,
                Some(1000.0),
                1100.0,
                Some(50),
                Some(10),
            ),
        ];

        let result = classify_outliers(&rows);

        assert!(result.iter().all(Vec::is_empty));
    }

    #[test]
    fn classify_outliers_grupo_con_stddev_cero_no_flaggea() {
        // 5 filas con TTFT idéntico: stddev=0, no hay variación real que
        // reportar como outlier.
        let rows: Vec<RequestRow> = (0..5)
            .map(|_| {
                req(
                    "anthropic",
                    "claude-opus-4",
                    200,
                    Some(100.0),
                    200.0,
                    Some(50),
                    Some(10),
                )
            })
            .collect();

        let result = classify_outliers(&rows);

        assert!(result.iter().all(Vec::is_empty));
    }

    #[test]
    fn classify_outliers_detecta_ttft_lento_a_2_sigma() {
        // ttft = [10,10,10,10,10,100]; mean=25, stddev≈33.54,
        // threshold=mean+2*stddev≈92.08. Solo la fila de 100 debe flaggearse.
        let mut rows: Vec<RequestRow> = (0..5)
            .map(|_| {
                req(
                    "anthropic",
                    "claude-opus-4",
                    200,
                    Some(10.0),
                    200.0,
                    Some(50),
                    Some(10),
                )
            })
            .collect();
        rows.push(req(
            "anthropic",
            "claude-opus-4",
            200,
            Some(100.0),
            300.0,
            Some(50),
            Some(10),
        ));

        let result = classify_outliers(&rows);

        assert!(
            result[0..5]
                .iter()
                .all(|k| !k.contains(&OutlierKind::SlowTtft))
        );
        assert!(result[5].contains(&OutlierKind::SlowTtft));
    }

    #[test]
    fn classify_outliers_detecta_cache_miss_entre_filas_cacheadas() {
        // 4 filas con cache-hit real (cache_read_tokens > 0) + 1 fila sin
        // cache-hit: la mitad+ de las OTRAS filas del grupo tienen hit, así
        // que la fila sin hit debe flaggearse CacheMiss. Las cacheadas no.
        let mut rows: Vec<RequestRow> = (0..4)
            .map(|_| {
                req(
                    "anthropic",
                    "claude-opus-4",
                    200,
                    Some(50.0),
                    200.0,
                    Some(50),
                    Some(500),
                )
            })
            .collect();
        rows.push(req(
            "anthropic",
            "claude-opus-4",
            200,
            Some(50.0),
            200.0,
            Some(50),
            None,
        ));

        let result = classify_outliers(&rows);

        assert!(
            result[0..4]
                .iter()
                .all(|k| !k.contains(&OutlierKind::CacheMiss))
        );
        assert!(result[4].contains(&OutlierKind::CacheMiss));
    }

    #[test]
    fn classify_outliers_no_streaming_con_total_igual_a_ttft_no_es_slow_generation() {
        // total_ms == ttft_ms => gen_ms == 0: el throughput no es calculable
        // para esta fila y debe EXCLUIRSE de la métrica, no tratarse como
        // lenta, aunque el resto del grupo tenga throughput normal.
        let mut rows: Vec<RequestRow> = (0..4)
            .map(|_| {
                req(
                    "anthropic",
                    "claude-opus-4",
                    200,
                    Some(50.0),
                    550.0,
                    Some(500),
                    Some(10),
                )
            })
            .collect();
        rows.push(req(
            "anthropic",
            "claude-opus-4",
            200,
            Some(100.0),
            100.0,
            Some(500),
            Some(10),
        ));

        let result = classify_outliers(&rows);

        assert!(!result[4].contains(&OutlierKind::SlowGeneration));
    }

    #[test]
    fn classify_outliers_error_se_flaggea_incluso_con_una_sola_fila() {
        let rows = vec![req(
            "anthropic",
            "claude-opus-4",
            500,
            Some(10.0),
            100.0,
            Some(50),
            Some(10),
        )];

        let result = classify_outliers(&rows);

        assert_eq!(result[0], vec![OutlierKind::Error]);
    }

    #[test]
    fn classify_outliers_nan_en_ttft_no_panickea_y_se_excluye_de_la_media() {
        // Una fila con NaN no debería ni flaggearse a sí misma como
        // SlowTtft, ni contaminar la media/stddev usada para las demás.
        let mut rows: Vec<RequestRow> = (0..4)
            .map(|_| {
                req(
                    "anthropic",
                    "claude-opus-4",
                    200,
                    Some(10.0),
                    200.0,
                    Some(50),
                    Some(10),
                )
            })
            .collect();
        rows.push(req(
            "anthropic",
            "claude-opus-4",
            200,
            Some(f64::NAN),
            200.0,
            Some(50),
            Some(10),
        ));

        let result = classify_outliers(&rows);

        // No debe panickear (llegar acá ya lo prueba) y la fila NaN no debe
        // quedar flaggeada como SlowTtft.
        assert!(!result[4].contains(&OutlierKind::SlowTtft));
    }

    #[test]
    fn classify_outliers_none_en_ttft_se_excluye_sin_flaggear() {
        let mut rows: Vec<RequestRow> = (0..4)
            .map(|_| {
                req(
                    "anthropic",
                    "claude-opus-4",
                    200,
                    Some(10.0),
                    200.0,
                    Some(50),
                    Some(10),
                )
            })
            .collect();
        rows.push(req(
            "anthropic",
            "claude-opus-4",
            200,
            None,
            200.0,
            Some(50),
            Some(10),
        ));

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
        let mut r = req(
            upstream,
            model,
            200,
            Some(10.0),
            100.0,
            Some(50),
            cache_read_tokens,
        );
        r.input_tokens = input_tokens;
        r.cache_write_tokens = cache_write_tokens;
        r.context_measured_bytes = context_measured_bytes;
        r
    }

    #[test]
    fn prompt_tokens_total_anthropic_suma_cache_read_y_write() {
        // Caso real: cache-hit grande, input_tokens irrisorio. Sumar la
        // caché es OBLIGATORIO o el denominador queda absurdo.
        let r = req_prompt(
            "anthropic",
            "claude-opus-4",
            Some(2),
            Some(124_733),
            Some(1_355),
            Some(224_653),
        );
        assert_eq!(prompt_tokens_total(&r), Some(2 + 124_733 + 1_355));
    }

    #[test]
    fn prompt_tokens_total_no_anthropic_ignora_cache_read_por_ser_subconjunto() {
        // OpenAI/Gemini: cache_read ya es SUBCONJUNTO de input_tokens.
        // Sumarlo encima sería doble conteo.
        let r = req_prompt(
            "openai",
            "gpt-4o",
            Some(1000),
            Some(400),
            None,
            Some(50_000),
        );
        assert_eq!(prompt_tokens_total(&r), Some(1000));
    }

    #[test]
    fn prompt_tokens_total_none_si_falta_input_tokens() {
        let r = req_prompt(
            "anthropic",
            "claude-opus-4",
            None,
            Some(100),
            Some(0),
            Some(1_000),
        );
        assert_eq!(prompt_tokens_total(&r), None);
    }

    #[test]
    fn bytes_per_token_anthropic_usa_la_suma_no_solo_input_tokens() {
        // input_tokens=2 solo daría 224_653/2=112_326.5 B/tok, un número que
        // gritaría "truncamiento" en el request MÁS SANO posible (cache-hit
        // grande, 200 OK). La suma da ~1.8, el valor real observado para
        // Anthropic con caché — MUY por debajo de un input_tokens-only.
        let r = req_prompt(
            "anthropic",
            "claude-opus-4",
            Some(2),
            Some(124_733),
            Some(1_355),
            Some(224_653),
        );
        let b = bytes_per_token(&r).expect("debe calcularse con todos los datos presentes");
        let expected = 224_653.0 / (2.0 + 124_733.0 + 1_355.0);
        assert!((b - expected).abs() < 1e-6, "b={b} expected={expected}");
        assert!(
            (b - 1.8).abs() < 0.05,
            "b={b} debe rondar ~1.8, no 112_326 (input_tokens solo)"
        );
    }

    #[test]
    fn bytes_per_token_openai_con_cache_read_no_dobla_el_conteo() {
        let r = req_prompt("openai", "gpt-4o", Some(1000), Some(400), None, Some(2_700));
        let b = bytes_per_token(&r).expect("debe calcularse");
        assert!(
            (b - 2.7).abs() < 1e-9,
            "b={b} debe usar input_tokens=1000 solo, no 1000+400"
        );
    }

    #[test]
    fn bytes_per_token_none_si_falta_input_tokens() {
        let r = req_prompt(
            "anthropic",
            "claude-opus-4",
            None,
            Some(10),
            Some(0),
            Some(1_000),
        );
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
            req_prompt(
                "openai",
                "llama3.2:3b",
                Some(4095),
                None,
                None,
                Some(18_955),
            ),
            req_prompt(
                "openai",
                "llama3.2:3b",
                Some(4095),
                None,
                None,
                Some(28_806),
            ),
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
            req_prompt(
                "openai",
                "llama3.2:3b",
                Some(4095),
                None,
                None,
                Some(77_579),
            ),
            req_prompt(
                "openai",
                "llama3.2:3b",
                Some(4095),
                None,
                None,
                Some(84_161),
            ),
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
            req_prompt(
                "openai",
                "llama3.2:3b",
                Some(65_000),
                None,
                None,
                Some(199_400),
            ),
            req_prompt(
                "openai",
                "llama3.2:3b",
                Some(65_000),
                None,
                None,
                Some(200_000),
            ),
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
            req_prompt(
                "openai",
                "llama3.2:3b",
                Some(4095),
                None,
                None,
                Some(20_000),
            ),
            req_prompt(
                "openai",
                "llama3.2:3b",
                Some(4095),
                None,
                None,
                Some(20_150),
            ), // +0.75%
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
        let rows = vec![req_prompt(
            "anthropic",
            "claude-opus-4",
            Some(2),
            Some(124_733),
            Some(1_355),
            Some(224_653),
        )];

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
            req_prompt(
                "openai",
                "gpt-4o",
                Some(1000),
                Some(400),
                None,
                Some(10_000),
            ),
            req_prompt(
                "openai",
                "gpt-4o",
                Some(1000),
                Some(400),
                None,
                Some(15_000),
            ),
        ];

        let result = classify_outliers(&rows);

        assert!(result[0].contains(&OutlierKind::Truncated));
        assert!(result[1].contains(&OutlierKind::Truncated));
    }

    #[test]
    fn classify_truncation_filas_sin_input_tokens_se_excluyen_sin_panic() {
        let rows = vec![
            req_prompt("openai", "llama3.2:3b", None, None, None, Some(18_955)),
            req_prompt(
                "openai",
                "llama3.2:3b",
                Some(4095),
                None,
                None,
                Some(28_806),
            ),
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
        let rows = vec![req_prompt(
            "openai",
            "llama3.2:3b",
            Some(4095),
            None,
            None,
            Some(77_783),
        )];

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
        let mut r = req(
            "anthropic",
            "claude-opus-4",
            200,
            Some(10.0),
            100.0,
            Some(50),
            Some(10),
        );
        r.client = Some("claude-cli/2.1.207 (external, sdk-cli)".to_string());

        let cells = requests_row_cells(RequestsView::Context, &r);

        assert_eq!(cells[14], "claude-cli/2.1.20…");
    }

    /// `client: None` debe leerse como `-`, NUNCA como string vacío ni como
    /// una clasificación inventada ("desconocido", "otro"…): un cliente sin
    /// `User-Agent` es un dato ausente, no una categoría.
    #[test]
    fn requests_row_cells_context_client_none_es_guion() {
        let mut r = req(
            "anthropic",
            "claude-opus-4",
            200,
            Some(10.0),
            100.0,
            Some(50),
            Some(10),
        );
        r.client = None;

        let cells = requests_row_cells(RequestsView::Context, &r);

        assert_eq!(cells[14], "-");
    }

    /// `truncate_client` no debe truncar un `User-Agent` que ya entra en
    /// [`CLIENT_DISPLAY_MAX`] caracteres — mismo criterio que
    /// [`truncate_model`] para strings cortos.
    #[test]
    fn truncate_client_no_trunca_si_entra() {
        assert_eq!(truncate_client(Some("curl/8.0")), "curl/8.0");
    }

    /// La columna `tsearch` (índice 13) de la vista Context surfacea la señal
    /// eager-vs-lazy: `used: true` con `deferred_loaded: 3` ⇒ `"lazy:3"`.
    #[test]
    fn requests_row_cells_context_tsearch_lazy() {
        let mut r = req(
            "codex",
            "gpt-5.5",
            200,
            Some(10.0),
            100.0,
            Some(50),
            Some(10),
        );
        r.tool_search = Some(ToolSearchRow {
            used: true,
            deferred_loaded: 3,
        });

        let cells = requests_row_cells(RequestsView::Context, &r);

        assert_eq!(cells[15], "lazy:3");
    }

    /// `used: false` (petición Responses/Codex medida sin diferido este turno)
    /// ⇒ `"eager"` — EAGER confirmado, no ausencia de dato.
    #[test]
    fn tsearch_cell_eager_cuando_used_false() {
        let mut r = req(
            "codex",
            "gpt-5.5",
            200,
            Some(10.0),
            100.0,
            Some(50),
            Some(10),
        );
        r.tool_search = Some(ToolSearchRow {
            used: false,
            deferred_loaded: 0,
        });

        assert_eq!(tsearch_cell(&r), "eager");
    }

    /// `lazy:0` es un estado válido y distinto de `eager`: hubo un
    /// `tool_search_call` (mecanismo lazy ejercitado) que no llegó a cargar
    /// ninguna tool.
    #[test]
    fn tsearch_cell_lazy_cero_no_es_eager() {
        let mut r = req(
            "codex",
            "gpt-5.5",
            200,
            Some(10.0),
            100.0,
            Some(50),
            Some(10),
        );
        r.tool_search = Some(ToolSearchRow {
            used: true,
            deferred_loaded: 0,
        });

        assert_eq!(tsearch_cell(&r), "lazy:0");
    }

    /// `tool_search: None` (dialecto donde no aplica, o proxy viejo) ⇒ `"-"`,
    /// nunca string vacío ni un valor inventado.
    #[test]
    fn tsearch_cell_none_es_guion() {
        let mut r = req(
            "anthropic",
            "claude-opus-4",
            200,
            Some(10.0),
            100.0,
            Some(50),
            Some(10),
        );
        r.tool_search = None;

        assert_eq!(tsearch_cell(&r), "-");
    }

    /// La columna `flat` (índice 14) de la vista Context surfacea
    /// `tools_flattened`: `Some(true)` (pi/opencode, `(native)` no verificable)
    /// ⇒ `"yes"`.
    #[test]
    fn requests_row_cells_context_flat_yes() {
        let mut r = req(
            "codex",
            "gpt-5.5",
            200,
            Some(10.0),
            100.0,
            Some(50),
            Some(10),
        );
        r.tools_flattened = Some(true);

        let cells = requests_row_cells(RequestsView::Context, &r);

        assert_eq!(cells[16], "yes");
    }

    /// `Some(false)` (hay tools `mcp__`, `(native)` de fiar) ⇒ `"no"`; `None`
    /// (no aplica / proxy viejo) ⇒ `"-"`. Nunca string vacío.
    #[test]
    fn flattened_cell_no_y_guion() {
        let mut r = req(
            "codex",
            "gpt-5.5",
            200,
            Some(10.0),
            100.0,
            Some(50),
            Some(10),
        );
        r.tools_flattened = Some(false);
        assert_eq!(flattened_cell(&r), "no");
        r.tools_flattened = None;
        assert_eq!(flattened_cell(&r), "-");
    }

    /// Un proxy anterior al campo manda el JSON de `/requests` SIN la clave
    /// `tools_flattened`: debe deserializar a `None` (Option ausente en serde),
    /// no romper el parseo de la fila.
    #[test]
    fn request_row_deserializa_tools_flattened_ausente_como_none() {
        let json = r#"{
            "timestamp": "2026-07-24T00:00:00Z",
            "route": "/v1/codex/responses",
            "upstream": "codex",
            "model": "gpt-5.5",
            "stream": true,
            "status": 200,
            "cache_control_forced": false,
            "total_ms": 100.0
        }"#;

        let row: RequestRow = serde_json::from_str(json).unwrap();

        assert_eq!(row.tools_flattened, None);
        assert_eq!(flattened_cell(&row), "-");
    }

    /// Con la clave presente (proxy actual, opencode aplanado) se deserializa a
    /// `Some(true)` y se renderiza `"yes"`.
    #[test]
    fn request_row_deserializa_tools_flattened_presente() {
        let json = r#"{
            "timestamp": "2026-07-24T00:00:00Z",
            "route": "/v1/codex/responses",
            "upstream": "codex",
            "model": "gpt-5.5",
            "stream": true,
            "status": 200,
            "cache_control_forced": false,
            "total_ms": 100.0,
            "tools_flattened": true
        }"#;

        let row: RequestRow = serde_json::from_str(json).unwrap();

        assert_eq!(row.tools_flattened, Some(true));
        assert_eq!(flattened_cell(&row), "yes");
    }

    /// Un proxy de build anterior a este campo manda el JSON de `/requests`
    /// SIN la clave `tool_search`: debe deserializar a `None` (serde trata un
    /// `Option` ausente como `None`), no romper el parseo de la fila entera.
    #[test]
    fn request_row_deserializa_tool_search_ausente_como_none() {
        let json = r#"{
            "timestamp": "2026-07-24T00:00:00Z",
            "route": "/v1/codex/responses",
            "upstream": "codex",
            "model": "gpt-5.5",
            "stream": true,
            "status": 200,
            "cache_control_forced": false,
            "total_ms": 100.0
        }"#;

        let row: RequestRow = serde_json::from_str(json).unwrap();

        assert_eq!(row.tool_search, None);
        assert_eq!(tsearch_cell(&row), "-");
    }

    /// Con la clave presente (proxy actual, dialecto Responses/Codex lazy), se
    /// deserializa a `Some` con `used`/`deferred_loaded` exactos.
    #[test]
    fn request_row_deserializa_tool_search_presente() {
        let json = r#"{
            "timestamp": "2026-07-24T00:00:00Z",
            "route": "/v1/codex/responses",
            "upstream": "codex",
            "model": "gpt-5.5",
            "stream": true,
            "status": 200,
            "cache_control_forced": false,
            "total_ms": 100.0,
            "tool_search": {"used": true, "deferred_loaded": 7}
        }"#;

        let row: RequestRow = serde_json::from_str(json).unwrap();

        assert_eq!(
            row.tool_search,
            Some(ToolSearchRow {
                used: true,
                deferred_loaded: 7
            })
        );
        assert_eq!(tsearch_cell(&row), "lazy:7");
    }

    #[test]
    fn classify_truncation_compone_con_otro_marcador_en_la_misma_fila() {
        // Truncated debe convivir con otro OutlierKind en la misma fila
        // (acá, Error) sin pisarlo ni excluirlo.
        let mut rows = vec![
            req_prompt(
                "openai",
                "llama3.2:3b",
                Some(4095),
                None,
                None,
                Some(18_955),
            ),
            req_prompt(
                "openai",
                "llama3.2:3b",
                Some(4095),
                None,
                None,
                Some(28_806),
            ),
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

    // --- Contador de potencia (panel `g`) ---

    /// La línea real de `nvidia-smi` en esta máquina, tal cual sale.
    const MUESTRA_REAL: &str = "NVIDIA GeForce RTX 4080 SUPER, 25, 34.27, 320.00, 723, 16376, 40";

    // --- Modelos residentes (ollama `/api/ps`) ---

    /// Respuesta real de `/api/ps`, recortada a lo que se consume.
    const PS_REAL: &str = r#"{"models":[{"name":"llama3.2:3b","size":2554708622,
        "details":{"parameter_size":"3.2B","quantization_level":"Q4_K_M"},
        "expires_at":"2026-08-09T15:24:02.895598913+02:00",
        "size_vram":2554708622,"context_length":4096}]}"#;

    #[test]
    fn parsea_los_modelos_residentes_de_ollama() {
        let ms = parse_ollama_ps(PS_REAL);

        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].nombre, "llama3.2:3b");
        assert_eq!(ms[0].vram_bytes, 2_554_708_622);
        assert_eq!(ms[0].cuantizacion.as_deref(), Some("Q4_K_M"));
        assert_eq!(ms[0].parametros.as_deref(), Some("3.2B"));
    }

    /// **Sin modelos residentes la lista está VACÍA, y eso es un dato.**
    /// Significa que la próxima petición pagará la carga — medido, hasta el
    /// 98% del tiempo de una petición fría. No es lo mismo que no poder
    /// preguntar.
    #[test]
    fn ningun_modelo_cargado_es_una_lista_vacia_no_un_error() {
        assert!(parse_ollama_ps(r#"{"models":[]}"#).is_empty());
    }

    /// Basura, otro servicio en el puerto, o un ollama que cambió de forma:
    /// lista vacía. El panel distingue «no hay nada cargado» de «no se pudo
    /// preguntar» por otra vía — ver [`App::ollama`], que es `Option`.
    #[test]
    fn una_respuesta_que_no_es_de_ollama_no_inventa_modelos() {
        for basura in ["", "no soy json", "{}", r#"{"models":"no-es-lista"}"#] {
            assert!(
                parse_ollama_ps(basura).is_empty(),
                "{basura:?} no puede producir modelos"
            );
        }
    }

    /// Un modelo sin `expires_at` legible se muestra igual: el nombre y la
    /// VRAM son lo que importa, y descartarlo por no saber cuándo caduca
    /// perdería el dato principal.
    #[test]
    fn un_modelo_sin_caducidad_legible_sigue_contando() {
        let ms = parse_ollama_ps(r#"{"models":[{"name":"x","size_vram":100}]}"#);

        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].nombre, "x");
        assert!(ms[0].caduca_en.is_none(), "sin fecha, sin cuenta atrás");
    }

    #[test]
    fn parsea_la_salida_real_de_nvidia_smi() {
        let g = parse_gpu_sample(MUESTRA_REAL).expect("debe parsear la línea real");

        assert_eq!(g.nombre, "NVIDIA GeForce RTX 4080 SUPER");
        assert_eq!(g.util_pct, 25);
        assert_eq!(g.vatios, 34.27);
        assert_eq!(g.vatios_max, 320.0);
        assert_eq!(g.mem_usada_mb, 723);
        assert_eq!(g.mem_total_mb, 16_376);
        assert_eq!(g.grados, 40);
    }

    /// **Sin GPU no se fabrica un cero.** Un `0%` y `0 W` en el panel se
    /// leerían como "la máquina no está haciendo nada", que es una
    /// afirmación; lo cierto es "no lo sé". Mismo contrato que el resto del
    /// monitor.
    #[test]
    fn sin_salida_utilizable_no_hay_muestra() {
        for basura in [
            "",
            "   ",
            "Field \"inventado\" is not a valid field to query.",
            "solo, tres, campos",
            "NVIDIA, no-numero, 34.27, 320.00, 723, 16376, 40",
        ] {
            assert!(
                parse_gpu_sample(basura).is_none(),
                "{basura:?} no puede producir una muestra"
            );
        }
    }

    /// La fracción del gauge es potencia sobre el LÍMITE de la tarjeta, no
    /// sobre un máximo inventado ni sobre el pico visto. Un cuentarrevoluciones
    /// sin línea roja no dice nada.
    #[test]
    fn la_fraccion_de_potencia_es_sobre_el_limite_de_la_tarjeta() {
        let g = parse_gpu_sample("X, 50, 160.00, 320.00, 1, 2, 40").expect("parsea");

        assert!((g.fraccion_potencia() - 0.5).abs() < 1e-9);
    }

    /// Un límite de cero o ausente no puede producir una división: se declara
    /// que no hay fracción en vez de publicar un infinito o un cero falso.
    #[test]
    fn un_limite_de_cero_no_produce_fraccion() {
        let g = parse_gpu_sample("X, 50, 160.00, 0.00, 1, 2, 40").expect("parsea");

        assert_eq!(g.fraccion_potencia(), 0.0, "sin límite, sin aguja");
    }

    /// El panel es INDEPENDIENTE del resto, como `s`/`e`/`u`. Y se muestrea
    /// solo cuando está visible: lo que no se enseña no se paga.
    #[test]
    fn la_tecla_g_alterna_el_panel_sin_tocar_los_demas() {
        let mut app = App::new("http://x".to_string());
        let antes = (
            app.show_requests_panel,
            app.show_tools_panel,
            app.show_quota_panel,
            app.show_sessions_panel,
        );
        assert!(!app.show_gpu_panel, "arranca oculto: cuesta 24 ms por poll");

        app.toggle_gpu_panel();
        assert!(app.show_gpu_panel);

        app.toggle_gpu_panel();
        assert!(!app.show_gpu_panel);

        assert_eq!(
            antes,
            (
                app.show_requests_panel,
                app.show_tools_panel,
                app.show_quota_panel,
                app.show_sessions_panel,
            ),
            "no toca ningún otro panel"
        );
    }

    /// El ciclo cubre las CUATRO variantes y vuelve al inicio. Que cierre el
    /// bucle es la parte que importa: una vista alcanzable pero de la que no
    /// se pueda salir sin reiniciar sería peor que no tenerla.
    #[test]
    fn requests_view_next_cicla_entre_las_cuatro_variantes() {
        assert_eq!(RequestsView::Latency.next(), RequestsView::Context);
        assert_eq!(RequestsView::Context.next(), RequestsView::Cache);
        assert_eq!(RequestsView::Cache.next(), RequestsView::Toll);
        assert_eq!(RequestsView::Toll.next(), RequestsView::Latency);
    }

    // --- Vista Toll: el peaje fijo por petición ---

    /// Fila con los tres bloques del peaje puestos.
    fn req_con_peaje(
        instr: Option<usize>,
        hooks: Option<usize>,
        skills: Option<usize>,
    ) -> RequestRow {
        let mut r = req(
            "anthropic",
            "claude-opus-4-1",
            200,
            Some(50.0),
            100.0,
            None,
            None,
        );
        r.prompt_bytes = Some(100_000);
        r.instructions = instr.map(|bytes| InstructionsRow { bytes });
        r.hooks = hooks.map(|bytes| HooksRow { bytes, declared: 3 });
        r.skills = skills.map(|listing_bytes| SkillsRow {
            listing_bytes,
            declared: 66,
        });
        r
    }

    /// El caso normal: los tres bloques presentes, el total es su suma y el
    /// porcentaje se calcula sobre `prompt_bytes` — lo que de verdad se paga.
    #[test]
    fn el_peaje_suma_los_tres_bloques_y_los_situa_sobre_lo_pagado() {
        let celdas = toll_row_cells(&req_con_peaje(Some(30_000), Some(12_000), Some(8_000)));

        // hora, modelo, instr, hooks, nh, skills, nsk, peaje, %prom, outlier
        assert_eq!(celdas[7], format_bytes(50_000), "el total son los tres");
        assert_eq!(celdas[8], "50.0", "50.000 de 100.000 pagados");
        assert_eq!(celdas[4], "3", "hooks declarados");
        assert_eq!(celdas[6], "66", "skills declaradas");
    }

    /// **`null` NO es cero.** Un bloque que no se pudo ver se pinta con el
    /// guion de dato ausente, nunca con un `0` que se leería como "gratis".
    #[test]
    fn un_bloque_ausente_se_marca_no_se_pinta_como_cero() {
        let celdas = toll_row_cells(&req_con_peaje(Some(30_000), None, Some(8_000)));

        assert_eq!(celdas[3], "-", "hooks ausente");
        assert_eq!(celdas[4], "-", "y su conteo tampoco se inventa");
    }

    /// Y si falta alguno, el total es una COTA INFERIOR, no el peaje. Se
    /// marca, porque un número que parece completo y no lo es aconseja peor
    /// que no dar ninguno.
    #[test]
    fn con_un_bloque_ausente_el_total_se_declara_incompleto() {
        let completo = toll_row_cells(&req_con_peaje(Some(30_000), Some(12_000), Some(8_000)));
        let parcial = toll_row_cells(&req_con_peaje(Some(30_000), None, Some(8_000)));

        assert!(!completo[7].starts_with('≥'), "completo no lleva marca");
        assert!(
            parcial[7].starts_with('≥'),
            "un total al que le falta un bloque tiene que decirlo: {}",
            parcial[7]
        );
    }

    /// Sin ningún bloque no hay peaje que enseñar: todo ausente, y el total
    /// tampoco se convierte en un cero.
    #[test]
    fn sin_ningun_bloque_no_se_fabrica_un_total() {
        let celdas = toll_row_cells(&req_con_peaje(None, None, None));

        assert_eq!(celdas[7], "-", "sin una sola muestra no hay total");
        assert_eq!(celdas[8], "-", "ni porcentaje");
    }

    /// Sin `prompt_bytes` no hay denominador. El total sigue valiendo; el
    /// porcentaje no se inventa.
    #[test]
    fn sin_prompt_bytes_hay_total_pero_no_porcentaje() {
        let mut r = req_con_peaje(Some(30_000), Some(12_000), Some(8_000));
        r.prompt_bytes = None;

        let celdas = toll_row_cells(&r);

        assert_eq!(celdas[7], format_bytes(50_000));
        assert_eq!(celdas[8], "-");
    }

    /// Recorre TODAS las vistas siguiendo el ciclo de [`RequestsView::next`].
    ///
    /// La lista no se escribe a mano a propósito. Antes sí, con las tres
    /// variantes cableadas, y eso convertía al guardián de abajo en un
    /// guardián que no guarda: al añadir `Toll` habría seguido en verde sin
    /// mirarla. Aquí la enumeración sale de `next()`, que es un `match`
    /// exhaustivo y el compilador obliga a cubrir cada variante nueva.
    ///
    /// Y de paso comprueba algo que ninguna lista a mano puede: una vista
    /// fuera del ciclo sería inalcanzable desde el teclado, que es un fallo por
    /// sí mismo. Si existe y no está aquí, es que no se puede llegar a ella.
    fn todas_las_vistas() -> Vec<RequestsView> {
        let mut vistas = vec![RequestsView::default()];
        loop {
            let siguiente = vistas[vistas.len() - 1].next();
            if siguiente == vistas[0] {
                return vistas;
            }
            assert!(
                !vistas.contains(&siguiente),
                "el ciclo de vistas no vuelve al inicio: {siguiente:?} repetida"
            );
            vistas.push(siguiente);
        }
    }

    /// Cabecera, anchos y celdas tienen que medir lo mismo EN CADA VISTA.
    /// Sin esto, agregar una columna a una sola de las tres piezas desalinea
    /// la tabla en silencio: `ratatui` no se queja, simplemente pinta mal.
    #[test]
    fn todas_las_vistas_tienen_cabecera_anchos_y_celdas_del_mismo_ancho() {
        let r = req(
            "anthropic",
            "claude-opus-4-8",
            200,
            Some(500.0),
            1200.0,
            Some(80),
            Some(1000),
        );
        let vistas = todas_las_vistas();
        assert!(vistas.len() >= 4, "el ciclo tiene que cubrirlas todas");
        for vista in vistas {
            let anchos = requests_table_widths(vista).len();
            let celdas = requests_row_cells(vista, &r).len();
            let etiquetas = requests_table_labels(vista).len();
            assert_eq!(
                etiquetas, anchos,
                "{vista:?}: {etiquetas} etiquetas contra {anchos} anchos"
            );
            // Los anchos incluyen la columna `outlier`, que el llamador
            // agrega aparte; por eso las celdas son una menos.
            assert_eq!(
                celdas + 1,
                anchos,
                "{vista:?}: {celdas} celdas (+outlier) contra {anchos} anchos"
            );
        }
    }

    /// La columna `Wh_net` es la RESTA, y solo se hace cuando llegan las dos
    /// mitades. El proxy manda bruta y reposo por separado a propósito.
    #[test]
    fn la_columna_de_energia_pinta_la_neta() {
        let mut r = req(
            "ollama",
            "qwen2.5:7b",
            200,
            Some(100.0),
            3382.0,
            Some(167),
            None,
        );
        r.energy_wh = Some(0.109_894);
        r.energy_idle_wh = Some(0.043_794);
        r.energy_samples = Some(17);
        // 66,1 mWh: la bruta menos el reposo, en milivatios-hora porque
        // `0.0661` se leería como casi cero.
        assert_eq!(energia_neta_cell(&r), "66.1m");
    }

    /// **Media medición no es una medición.** Si falta cualquiera de las dos
    /// mitades la celda es el guion de ausencia, NUNCA la bruta pintada como
    /// si fuera neta — que es exactamente el número que engañaría.
    #[test]
    fn sin_las_dos_mitades_la_energia_es_ausente_no_la_bruta() {
        let mut r = req(
            "ollama",
            "qwen2.5:7b",
            200,
            Some(100.0),
            3382.0,
            Some(167),
            None,
        );
        r.energy_wh = Some(0.109_894);
        r.energy_idle_wh = None;
        assert_eq!(energia_neta_cell(&r), "-");

        r.energy_wh = None;
        r.energy_idle_wh = Some(0.043_794);
        assert_eq!(energia_neta_cell(&r), "-");
    }

    /// Con upstream remoto no hay energía y la columna lo dice con el guion,
    /// igual que `usd` hace con un modelo local. Es la misma simetría por el
    /// otro lado: a nadie le facturan un modelo local, y nadie puede medir los
    /// vatios de un datacenter ajeno.
    #[test]
    fn un_modelo_remoto_no_publica_energia() {
        let r = req(
            "anthropic",
            "claude-opus-4-8",
            200,
            Some(500.0),
            1200.0,
            Some(80),
            Some(1000),
        );
        assert_eq!(energia_neta_cell(&r), "-");
    }

    /// Pocas muestras dentro de la ventana → `~`. El número sale de
    /// interpolar entre dos puntos de FUERA, y pintarlo con la misma cara que
    /// uno sostenido por diecisiete muestras fingiría una precisión que no hay.
    #[test]
    fn con_pocas_muestras_la_energia_se_marca_aproximada() {
        let mut r = req(
            "ollama",
            "qwen2.5:7b",
            200,
            Some(50.0),
            464.0,
            Some(41),
            None,
        );
        r.energy_wh = Some(0.033_418);
        r.energy_idle_wh = Some(0.006_003);
        r.energy_samples = Some(1);
        assert_eq!(energia_neta_cell(&r), "~27.4m");

        r.energy_samples = Some(3);
        assert_eq!(energia_neta_cell(&r), "27.4m");
    }

    /// El panel `g` enseña reposo y pico JUNTOS: sin el reposo no se puede
    /// restar nada, y sin el pico un número de vatios no dice si vas holgado.
    /// Es el mismo par que el proxy publica por petición, de otra fuente.
    #[test]
    fn el_panel_de_gpu_enseña_reposo_y_pico() {
        let mut app = App::new("http://x".to_string());
        for w in [258, 44, 273, 60] {
            app.gpu_watts.push_back(w);
        }
        let texto = linea_reposo_y_pico(&app)
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect::<String>();
        assert!(texto.contains("reposo 44 W"), "{texto}");
        assert!(texto.contains("pico 273 W"), "{texto}");
    }

    /// Sin histórico no se inventa un reposo. Un `0 W` ahí se leería como
    /// "la tarjeta no consume nada", que es lo contrario de "no lo sé".
    #[test]
    fn sin_historico_el_panel_no_inventa_un_reposo() {
        let app = App::new("http://x".to_string());
        let texto = linea_reposo_y_pico(&app)
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect::<String>();
        assert!(texto.contains("sin histórico"), "{texto}");
    }

    /// Por encima de 1 Wh se pinta en vatios-hora enteros, no en miles de
    /// mWh: una petición larga daría `1234.5m`, que nadie lee.
    #[test]
    fn por_encima_de_un_vatio_hora_cambia_de_unidad() {
        let mut r = req(
            "ollama",
            "qwen2.5:7b",
            200,
            Some(100.0),
            900_000.0,
            Some(50_000),
            None,
        );
        r.energy_wh = Some(3.5);
        r.energy_idle_wh = Some(1.25);
        r.energy_samples = Some(4000);
        assert_eq!(energia_neta_cell(&r), "2.250");
    }

    /// Sin `cache_by_section` la vista no inventa ceros: marca ausente cada
    /// columna derivada. Un `0%` ahí se leería como "no se cacheó nada", que
    /// es una afirmación distinta de "no se sabe".
    #[test]
    fn la_vista_cache_marca_ausente_cuando_el_proxy_no_atribuyo() {
        let mut r = req(
            "anthropic",
            "claude-opus-4-8",
            200,
            Some(500.0),
            1200.0,
            Some(80),
            Some(1000),
        );
        r.cache_by_section = None;

        let celdas = requests_row_cells(RequestsView::Cache, &r);

        assert!(
            celdas.iter().filter(|c| c.as_str() == "-").count() >= 7,
            "deberia marcar ausente casi todo: {celdas:?}"
        );
    }

    /// Una sección que mide cero bytes no tiene fracción cacheada: es hueco,
    /// no cero. Dividir daria `NaN` y un `0%` mentiría.
    #[test]
    fn cached_pct_distingue_seccion_vacia_de_seccion_sin_cachear() {
        assert_eq!(cached_pct_cell(Some(0), Some(0)), "-");
        assert_eq!(cached_pct_cell(Some(0), Some(100)), "0%");
        assert_eq!(cached_pct_cell(Some(50), Some(100)), "50%");
        assert_eq!(cached_pct_cell(None, Some(100)), "-");
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
        assert_eq!(app.requests_view, RequestsView::Cache);

        app.cycle_requests_view();
        assert_eq!(app.requests_view, RequestsView::Toll);

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

        let row: RequestRow =
            serde_json::from_str(json).expect("debe deserializar un payload con todos los campos");

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

        let row: RequestRow =
            serde_json::from_str(json).expect("debe deserializar aunque falten los campos nuevos");

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

        let row: RequestRow =
            serde_json::from_str(json).expect("debe deserializar con context_* en null explícito");

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

        let row: RequestRow =
            serde_json::from_str(json).expect("debe deserializar con tools_by_server presente");

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

        let row: RequestRow =
            serde_json::from_str(json).expect("debe deserializar con deferred_tools presente");

        let servers = row.tools_by_server.expect("debe traer el desglose");
        let gmail = servers
            .iter()
            .find(|s| s.server == "claude_ai_Gmail")
            .expect("Gmail presente");
        assert_eq!(
            gmail.deferred_tools,
            Some(3),
            "servidor totalmente diferido: deferred_tools == tools"
        );

        let calendar = servers
            .iter()
            .find(|s| s.server == "claude_ai_Google_Calendar")
            .expect("Calendar presente");
        assert_eq!(
            calendar.deferred_tools,
            Some(0),
            "servidor NADA diferido: sus bytes son reales y desconectables"
        );
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

        let row: RequestRow =
            serde_json::from_str(json).expect("debe deserializar sin los campos de tools");

        assert_eq!(row.tools_by_server, None);
        assert_eq!(row.tools_overhead_bytes, None);
    }

    // -----------------------------------------------------------------
    // find_tools_source_row / diff_against_baseline — panel "tools por
    // servidor" (tecla `s`)
    // -----------------------------------------------------------------

    fn tool_row(server: &str, kind: &str, tools: usize, bytes: usize) -> ToolServerRow {
        ToolServerRow {
            server: server.to_string(),
            kind: kind.to_string(),
            tools,
            bytes,
            deferred_tools: Some(0),
        }
    }

    /// Variante de [`tool_row`] que además fija `deferred_tools`, para los
    /// tests que necesitan un servidor con diferido parcial o total (no solo
    /// el `Some(0)` por defecto de la variante simple).
    fn tool_row_deferred(
        server: &str,
        kind: &str,
        tools: usize,
        bytes: usize,
        deferred_tools: usize,
    ) -> ToolServerRow {
        ToolServerRow {
            server: server.to_string(),
            kind: kind.to_string(),
            tools,
            bytes,
            deferred_tools: Some(deferred_tools),
        }
    }

    /// Variante de `req` (arriba) que además permite fijar `tools_by_server`,
    /// para los tests de [`find_tools_source_row`].
    fn req_with_tools(timestamp: &str, tools_by_server: Option<Vec<ToolServerRow>>) -> RequestRow {
        let mut r = req(
            "anthropic",
            "claude-opus-4",
            200,
            Some(10.0),
            100.0,
            Some(50),
            Some(10),
        );
        r.timestamp = timestamp.to_string();
        r.tools_by_server = tools_by_server;
        r
    }

    #[test]
    fn find_tools_source_row_ninguna_fila_califica_devuelve_none() {
        let rows = vec![
            req_with_tools("t1", None),
            req_with_tools("t2", Some(vec![])),
        ];
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
        let current = vec![
            tool_row("(native)", "native", 29, 86_168),
            tool_row("claude_ai_Gmail", "mcp", 13, 24_321),
        ];

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

        let disappeared = diffs
            .iter()
            .find(|d| d.server == "claude_ai_Google_Calendar")
            .expect("debe seguir apareciendo como fila");
        assert_eq!(disappeared.bytes, 0);
        assert_eq!(disappeared.tools, 0);
        assert_eq!(disappeared.kind, "-");
        assert_eq!(disappeared.delta, Some(-21_034));
    }

    #[test]
    fn diff_against_baseline_servidor_nuevo_tiene_delta_positivo_completo() {
        let current = vec![
            tool_row("(native)", "native", 29, 86_168),
            tool_row("plugin_engram_engram", "mcp", 18, 17_737),
        ];
        let mut baseline = BTreeMap::new();
        baseline.insert("(native)".to_string(), 86_168usize);

        let diffs = diff_against_baseline(&current, Some(&baseline));

        let new_server = diffs
            .iter()
            .find(|d| d.server == "plugin_engram_engram")
            .expect("debe estar presente");
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
        let current = vec![
            tool_row("(native)", "native", 29, 86_168),
            tool_row("claude_ai_Gmail", "mcp", 13, 24_321),
        ];
        let mut baseline = BTreeMap::new();
        baseline.insert("(native)".to_string(), 86_168usize);
        baseline.insert("claude_ai_Gmail".to_string(), 24_321usize);
        baseline.insert("claude_ai_Google_Calendar".to_string(), 21_034usize);
        baseline.insert("claude_ai_Google_Drive".to_string(), 9_743usize);

        let diffs = diff_against_baseline(&current, Some(&baseline));

        let names: Vec<&str> = diffs.iter().map(|d| d.server.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "(native)",
                "claude_ai_Gmail",
                "claude_ai_Google_Calendar",
                "claude_ai_Google_Drive"
            ]
        );
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

        let gmail = diffs
            .iter()
            .find(|d| d.server == "claude_ai_Gmail")
            .unwrap();
        assert_eq!(gmail.deferred_tools, Some(3));
        assert_eq!(deferred_cell(gmail), "3/3", "totalmente diferido");

        let calendar = diffs
            .iter()
            .find(|d| d.server == "claude_ai_Google_Calendar")
            .unwrap();
        assert_eq!(calendar.deferred_tools, Some(0));
        assert_eq!(
            deferred_cell(calendar),
            "0/4",
            "nada diferido: bytes reales y desconectables"
        );

        let drive = diffs
            .iter()
            .find(|d| d.server == "claude_ai_Google_Drive")
            .unwrap();
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

        let disappeared = diffs
            .iter()
            .find(|d| d.server == "claude_ai_Gmail")
            .unwrap();
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

    // --- Panel de sesión ---

    fn fila(source: &str, key: &str, is_session: bool, req: u64, cost: f64) -> SessionRow {
        SessionRow {
            source: source.to_string(),
            key: key.to_string(),
            is_session,
            requests: req,
            input_tokens: 10,
            output_tokens: 2,
            cost_usd: cost,
        }
    }

    /// El panel marca las filas de fallback. Sin la marca, quien lo mira suma
    /// una sesión concreta con el cubo que agrupa a todas las no atribuidas
    /// de ese harness — y el número parece una sesión siendo muchas.
    #[test]
    fn el_panel_distingue_una_sesion_de_un_cubo_no_atribuido() {
        let lineas = session_lines(&[fila("explicit", "s-A", true, 3, 0.5)], false);
        let no_atrib = session_lines(&[fila("unattributed", "curl/8", false, 1, 0.1)], false);

        assert!(lineas.iter().any(|l| l.contains("s-A")));
        assert!(
            no_atrib.iter().any(|l| l.contains("sin atribuir")),
            "no marca el cubo de fallback: {no_atrib:?}"
        );
        assert!(
            !lineas.iter().any(|l| l.contains("sin atribuir")),
            "marca como fallback una sesión real: {lineas:?}"
        );
    }

    /// Saturado: las filas son una cota inferior y el panel lo dice. Callarlo
    /// haría leer un total como si fuera completo.
    #[test]
    fn el_panel_declara_la_saturacion() {
        let con = session_lines(&[fila("explicit", "s", true, 1, 0.0)], true);
        let sin = session_lines(&[fila("explicit", "s", true, 1, 0.0)], false);

        assert!(
            con.iter().any(|l| l.to_lowercase().contains("satur")),
            "no declara la saturación: {con:?}"
        );
        assert!(!sin.iter().any(|l| l.to_lowercase().contains("satur")));
    }

    /// Sin datos no se pinta un cero: se dice que no hay nada medido todavía.
    #[test]
    fn el_panel_vacio_lo_dice_en_vez_de_pintar_ceros() {
        let lineas = session_lines(&[], false);

        assert!(!lineas.is_empty());
        assert!(
            lineas.iter().any(|l| l.contains("sin sesiones")),
            "no explica el vacío: {lineas:?}"
        );
    }

    /// El panel de sesión es INDEPENDIENTE de los demás, igual que el de
    /// cuota: cualquier combinación de visibilidad es válida.
    #[test]
    fn show_sessions_panel_arranca_visible_y_es_independiente() {
        let mut app = App::new("http://x/stats".to_string());
        assert!(app.show_sessions_panel);

        app.toggle_sessions_panel();
        assert!(!app.show_sessions_panel);
        assert!(app.show_quota_panel, "no debe tocar el de cuota");
        assert!(app.show_requests_panel, "no debe tocar el de requests");
    }

    /// La URL de `/sessions` se deriva del `/stats` configurado, igual que la
    /// de `/requests`: un solo puerto que configurar, no tres.
    #[test]
    fn la_url_de_sessions_se_deriva_de_la_de_stats() {
        assert_eq!(
            resolve_sessions_url_inner("http://127.0.0.1:8899/stats", None, None),
            "http://127.0.0.1:8899/sessions"
        );
        assert_eq!(
            resolve_sessions_url_inner("http://x/stats", Some("http://otro/s".to_string()), None),
            "http://otro/s",
            "el override explícito manda"
        );
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
        app.recent_requests = vec![req_with_tools(
            "t1",
            Some(vec![tool_row("(native)", "native", 29, 86_168)]),
        )];

        app.mark_baseline();

        let baseline = app
            .baseline
            .as_ref()
            .expect("mark_baseline debe crear un baseline");
        let tools_baseline = baseline
            .tools_by_server
            .as_ref()
            .expect("debe tomar la foto de tools_by_server");
        assert_eq!(tools_baseline.get("(native)"), Some(&86_168));
    }

    #[test]
    fn mark_baseline_sin_fila_fuente_deja_tools_by_server_en_none() {
        let mut app = App::new("http://x".to_string());
        // recent_requests vacío: no hay fila fuente que fotografiar.
        app.mark_baseline();

        let baseline = app
            .baseline
            .as_ref()
            .expect("mark_baseline debe crear un baseline igual");
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
        let rows = vec![
            req_with_quota("t1", Some(full_quota())),
            req_with_quota("t2", None),
            req_with_quota("t3", Some(full_quota())),
        ];

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
        let rows = vec![
            req_with_quota("t1", Some(full_quota())),
            req_with_quota("t2", None),
        ];

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
        let base = chrono::DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .timestamp();
        let remaining = quota_reset_remaining(&quota, timestamp, base + 1_000);
        assert_eq!(remaining, Some(3_600 - 1_000));
    }

    #[test]
    fn quota_reset_remaining_none_si_ambas_fuentes_faltan() {
        let mut quota = full_quota();
        quota.primary_reset_at = None;
        quota.primary_reset_after_seconds = None;
        assert_eq!(
            quota_reset_remaining(&quota, "2024-01-01T00:00:00Z", 0),
            None
        );
    }

    /// Regresión: un `reset_at` cercano a `i64::MIN` (cabecera `x-codex-*`
    /// adversaria o corrupta) NO debe desbordar la resta —panic en debug, wrap
    /// silencioso en release—, sino degradar a `None` (que se renderiza `"—"`).
    #[test]
    fn quota_reset_remaining_no_desborda_con_reset_at_extremo() {
        let mut quota = full_quota();
        quota.primary_reset_at = Some(i64::MIN);
        assert_eq!(
            quota_reset_remaining(&quota, "2024-01-01T00:00:00Z", 1_750_000_000),
            None
        );

        quota.primary_reset_at = Some(i64::MAX);
        assert_eq!(
            quota_reset_remaining(&quota, "2024-01-01T00:00:00Z", -1),
            None
        );
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

        let row: RequestRow =
            serde_json::from_str(json).expect("debe deserializar con codex_quota presente");
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

        let row: RequestRow =
            serde_json::from_str(json).expect("debe deserializar sin codex_quota");
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

    // -----------------------------------------------------------------
    // Filtro del panel de requests por el modelo seleccionado (tecla `f`)
    // -----------------------------------------------------------------

    fn key(upstream: &str, model: &str) -> ModelKey {
        (upstream.to_string(), model.to_string())
    }

    /// Fila sana de `(upstream, model)`: lo único que mira el filtro son esos
    /// dos campos, así que el resto queda en los valores neutros de [`req`].
    fn req_de(upstream: &str, model: &str) -> RequestRow {
        req(upstream, model, 200, Some(100.0), 1000.0, Some(10), None)
    }

    #[test]
    fn sin_filtro_se_ven_todas_las_filas_mas_nueva_primero() {
        let rows = vec![
            req_de("anthropic", "opus"),
            req_de("ollama", "qwen3"),
            req_de("anthropic", "opus"),
        ];

        // Índices sobre el vector ORIGINAL, en orden de pintado (más nueva
        // arriba): 2, 1, 0.
        assert_eq!(visible_request_indices(&rows, None), vec![2, 1, 0]);
    }

    #[test]
    fn el_filtro_deja_solo_el_upstream_y_modelo_seleccionados() {
        let rows = vec![
            req_de("anthropic", "opus"),
            req_de("ollama", "qwen3"),
            req_de("anthropic", "opus"),
        ];

        let solo_opus = visible_request_indices(&rows, Some(&key("anthropic", "opus")));
        assert_eq!(solo_opus, vec![2, 0]);
    }

    /// El upstream forma parte de la clave: el mismo nombre de modelo servido
    /// por dos proveedores distintos son DOS filas distintas en `/stats`, y el
    /// filtro tiene que respetar esa misma clave o mezclaría tráfico ajeno.
    #[test]
    fn el_filtro_no_mezcla_el_mismo_modelo_de_dos_upstreams() {
        let rows = vec![req_de("openai", "gpt-5"), req_de("azure", "gpt-5")];

        assert_eq!(
            visible_request_indices(&rows, Some(&key("openai", "gpt-5"))),
            vec![0]
        );
    }

    /// `/stats` agrupa los requests SIN modelo conocido bajo la clave
    /// `"unknown"` (ver `StatsAggregator::ingest` en `telemetry/stats.rs`),
    /// mientras que `/requests` los deja como `model: null`. Si el filtro no
    /// tradujera esa equivalencia, seleccionar la fila `unknown` de la tabla
    /// daría un panel vacío para siempre.
    #[test]
    fn el_filtro_traduce_unknown_a_las_filas_sin_modelo() {
        let mut sin_modelo = req("anthropic", "x", 500, None, 50.0, None, None);
        sin_modelo.model = None;
        let rows = vec![req_de("anthropic", "opus"), sin_modelo];

        assert_eq!(
            visible_request_indices(&rows, Some(&key("anthropic", "unknown"))),
            vec![1]
        );
        // Y a la inversa: la fila sin modelo NO cae dentro de un modelo real.
        assert_eq!(
            visible_request_indices(&rows, Some(&key("anthropic", "opus"))),
            vec![0]
        );
    }

    #[test]
    fn filtrar_sin_ninguna_coincidencia_devuelve_vacio_no_todo() {
        let rows = vec![req_de("anthropic", "opus")];

        assert!(visible_request_indices(&rows, Some(&key("ollama", "qwen3"))).is_empty());
    }

    /// El filtro arranca APAGADO: el panel de requests es un feed global por
    /// defecto y solo se estrecha si el usuario lo pide.
    #[test]
    fn el_filtro_de_requests_arranca_apagado_y_alterna() {
        let mut app = App::new("http://x/stats".to_string());
        assert!(!app.filter_requests_by_model);
        app.toggle_requests_filter();
        assert!(app.filter_requests_by_model);
        app.toggle_requests_filter();
        assert!(!app.filter_requests_by_model);
    }

    /// La clave del filtro sale de la fila SELECCIONADA. Sin selección
    /// (todavía no llegó ningún `/stats`) no hay clave, y el panel no puede
    /// quedarse vacío por filtrar contra la nada.
    #[test]
    fn sin_fila_seleccionada_el_filtro_no_produce_clave() {
        let mut app = App::new("http://x/stats".to_string());
        app.filter_requests_by_model = true;
        assert_eq!(app.requests_filter_key(), None);
    }

    // -----------------------------------------------------------------
    // La tabla de modelos scrollea con la selección
    // -----------------------------------------------------------------

    fn stats_row(upstream: &str, model: &str) -> StatsRow {
        StatsRow {
            upstream: upstream.to_string(),
            model: model.to_string(),
            requests: 1,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: 0.0,
            avg_ttft_ms: 0.0,
            avg_tokens_per_sec: 0.0,
            cache_hit_rate: 0.0,
            redundancy_rate: 0.0,
            error_rate: 0.0,
            ttft_ms_sum: 0.0,
            ttft_ms_count: 0,
            total_ms_sum: 0.0,
            errors: 0,
        }
    }

    fn buffer_to_string(buf: &ratatui::buffer::Buffer) -> String {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// EL BUG: con el área apretada (todos los paneles abiertos), la tabla
    /// recortaba a las primeras filas y la selección se dibujaba FUERA de la
    /// pantalla. Seleccionar el último de diez modelos tiene que traerlo al
    /// viewport, no dejarlo invisible.
    #[test]
    fn la_tabla_scrollea_hasta_la_fila_seleccionada() {
        let mut app = App::new("http://x/stats".to_string());
        // Nombres cortos a propósito: la columna MODELO es el 30% del ancho
        // y recorta lo que no entra, y acá se mide el SCROLL, no el recorte
        // de columnas.
        app.latest = (0..10).map(|i| stats_row("ol", &format!("m{i}"))).collect();
        app.selected = 9;

        // 6 de alto = 2 de bordes + 1 de header + 3 filas de datos: justo el
        // apretón que produce tener todos los paneles abiertos.
        let backend = ratatui::backend::TestBackend::new(60, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_table(f, f.area(), &mut app))
            .unwrap();

        let pintado = buffer_to_string(terminal.backend().buffer());
        assert!(
            pintado.contains("ol/m9"),
            "la fila seleccionada quedó fuera del viewport:\n{pintado}"
        );
    }

    /// El resaltado por color de fondo se pierde en terminales sin color y en
    /// capturas de texto. El símbolo de selección es la señal que sobrevive.
    #[test]
    fn la_fila_seleccionada_lleva_simbolo_visible() {
        let mut app = App::new("http://x/stats".to_string());
        // Nombres cortos a propósito: la columna MODELO es el 30% del ancho
        // y recorta lo que no entra, y acá se mide el SCROLL, no el recorte
        // de columnas.
        app.latest = (0..10).map(|i| stats_row("ol", &format!("m{i}"))).collect();
        app.selected = 9;

        let backend = ratatui::backend::TestBackend::new(60, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_table(f, f.area(), &mut app))
            .unwrap();

        let pintado = buffer_to_string(terminal.backend().buffer());
        let linea = pintado
            .lines()
            .find(|l| l.contains("ol/m9"))
            .expect("la fila seleccionada tiene que estar pintada");
        assert!(
            linea.contains(SELECTION_SYMBOL.trim()),
            "la fila seleccionada no lleva el símbolo: {linea}"
        );
    }

    /// Con el viewport en DOS filas y la selección en la 4ª posición, lo que
    /// se ve son la 3ª y la 4ª — no la 1ª y la 2ª ancladas arriba.
    ///
    /// Esa es la diferencia entre una tabla con viewport y una recortada: la
    /// recortada pinta siempre desde la primera fila y deja la selección
    /// fuera de pantalla; la que tiene viewport arrastra la ventana con la
    /// selección y las primeras filas SALEN de la vista.
    #[test]
    fn bajar_arrastra_la_ventana_y_las_primeras_filas_salen_de_vista() {
        let mut app = App::new("http://x/stats".to_string());
        app.latest = (1..=8).map(|i| stats_row("ol", &format!("m{i}"))).collect();
        app.selected = 3; // 4ª posición (índice 3) = "ol/m4"

        // 5 de alto = 2 bordes + 1 header + DOS filas de datos.
        let backend = ratatui::backend::TestBackend::new(60, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_table(f, f.area(), &mut app))
            .unwrap();

        let pintado = buffer_to_string(terminal.backend().buffer());
        assert!(
            pintado.contains("ol/m3") && pintado.contains("ol/m4"),
            "bajando a la 4ª hay que ver la 3ª y la 4ª:\n{pintado}"
        );
        assert!(
            !pintado.contains("ol/m1") && !pintado.contains("ol/m2"),
            "las dos primeras NO pueden quedarse fijas arriba:\n{pintado}"
        );
        assert!(
            pintado.contains("(4/8"),
            "el título tiene que decir la posición:\n{pintado}"
        );
    }

    /// Cuando la tabla no cabe entera, el título tiene que decir en qué
    /// posición estás: sin eso, tres filas visibles de doce no dicen si
    /// quedan modelos por debajo.
    #[test]
    fn el_titulo_de_la_tabla_cuenta_la_posicion() {
        assert_eq!(models_title(3, 12), " modelos (4/12 · total acumulado) ");
    }

    #[test]
    fn el_titulo_de_la_tabla_sin_modelos_no_inventa_posicion() {
        assert_eq!(models_title(0, 0), " modelos (total acumulado) ");
    }
}
