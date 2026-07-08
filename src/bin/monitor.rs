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
//! URL del endpoint (en orden de prioridad):
//!   1. flag `--url <url>`
//!   2. env `OXIDEGATE_STATS_URL`
//!   3. `http://127.0.0.1:{OXIDEGATE_PORT}/stats` (puerto default 8080, el
//!      mismo que usa el proxy en `config.rs`: así, corriendo ambos con la
//!      misma `OXIDEGATE_PORT` —o ninguna—, el monitor apunta al proxy sin
//!      configuración extra).
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

    if once {
        run_once(&url);
        return Ok(());
    }

    install_panic_hook();
    let mut terminal = setup_terminal()?;
    let result = run_app(&mut terminal, &url);
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

// ---------------------------------------------------------------------------
// Modo headless: --once
// ---------------------------------------------------------------------------

/// Hace UN fetch de `/stats` y lo imprime como tabla de texto plano, sin raw
/// mode. Sirve para verificación headless (CI, scripts) y como fallback CLI
/// cuando no hay terminal interactiva disponible. Nunca panickea: si el
/// proxy está caído, imprime un aviso y sale limpio con código 0.
fn run_once(url: &str) {
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
        }
    }

    /// Hace un fetch de `/stats` y actualiza todo el estado derivado
    /// (historial de sparklines, contadores para el próximo poll). Nunca
    /// panickea si el proxy no responde: solo actualiza `status` y sigue.
    fn poll(&mut self, client: &reqwest::blocking::Client, url: &str) {
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

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, url: &str) -> io::Result<()> {
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
                    _ => {}
                }
            }
        }

        if last_poll.elapsed() >= POLL_INTERVAL {
            app.poll(&client, url);
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(5),    // tabla principal
            Constraint::Length(6), // panel antes/después
            Constraint::Length(7), // sparklines
            Constraint::Length(1), // footer
        ])
        .split(area);

    draw_header(f, chunks[0], app);
    draw_table(f, chunks[1], app);
    draw_before_after(f, chunks[2], app);
    draw_sparklines(f, chunks[3], app);
    draw_footer(f, chunks[4]);
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

fn draw_footer(f: &mut Frame, area: Rect) {
    let text = Line::from("q salir · b marcar baseline · r reset · ↑/↓ elegir modelo");
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
}
