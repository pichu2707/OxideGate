//! Harness de benchmark: caracteriza cómo escala cada proveedor con el tamaño
//! del INPUT, a igualdad de condiciones (barrida controlada).
//!
//! Estrategia:
//!   1. Genera prompts de relleno de tamaño creciente y **output fijo chico**
//!      (para aislar el efecto del input en TTFT y coste de prefill).
//!   2. Dispara cada tamaño N veces por proveedor **a través de OxideGate**, así
//!      es el propio proxy quien mide (misma pipeline que en producción).
//!   3. Lee la telemetría que OxideGate escribió y arma una tabla comparable:
//!      proveedor × tamaño → input_tokens, TTFT, total, coste, tokens/seg.
//!
//! Solo automatiza los proveedores con API key (Gemini, OpenAI). Anthropic
//! (Claude Max/OAuth) se alimenta a mano por la sesión redirigida; sus filas
//! caen en la misma telemetría y entran al mismo reporte.
//!
//! Uso:
//!   OXIDEGATE_PORT=8899 GEMINI_API_KEY=... OPENAI_API_KEY=... cargo run --bin bench
//!
//! Las keys se leen de un archivo `.env` en la raíz (cargado con dotenvy) o del
//! entorno. Variables:
//!   OXIDEGATE_PORT   puerto de OxideGate (default 8899)
//!   API_KEY_GEMINI   habilita la barrida de Gemini
//!   API_KEY_OPENAI   habilita la barrida de OpenAI
//!   BENCH_REPEATS    repeticiones por tamaño (default 3)
//!   GEMINI_MODEL     modelo Gemini (default gemini-2.0-flash)
//!   OPENAI_MODEL     modelo OpenAI (default gpt-4o-mini)
use serde_json::{json, Value};
use std::env;
use std::path::PathBuf;
use std::time::Duration;

/// Tamaños de relleno (bytes de texto) a barrer. El 0 es "solo la instrucción".
const SIZES: &[usize] = &[0, 1_000, 5_000, 20_000, 50_000];

#[tokio::main]
async fn main() {
    // Carga el `.env` de la raíz si existe (no falla si no está).
    dotenvy::dotenv().ok();

    let port = env::var("OXIDEGATE_PORT").unwrap_or_else(|_| "8899".to_string());
    let base = format!("http://localhost:{port}");
    let repeats: usize = env::var("BENCH_REPEATS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);

    let client = reqwest::Client::new();

    // Marcamos cuántas filas de telemetría había ANTES, para reportar solo las
    // que genera esta corrida.
    let telemetry_path = telemetry_path();
    let rows_before = count_lines(&telemetry_path);

    println!("🏋️  OxideGate benchmark — barrida de tamaño de input");
    println!("    proxy: {base} | repeticiones por tamaño: {repeats}");
    println!("    tamaños (bytes de relleno): {SIZES:?}\n");

    let gemini_key = env::var("API_KEY_GEMINI").ok();
    let openai_key = env::var("API_KEY_OPENAI").ok();

    if gemini_key.is_none() && openai_key.is_none() {
        eprintln!("⚠️  Sin API_KEY_GEMINI ni API_KEY_OPENAI: nada que barrer.");
        eprintln!("    (Anthropic se alimenta a mano por la sesión redirigida.)");
        return;
    }

    let mut sent = 0usize;
    for &size in SIZES {
        for run in 0..repeats {
            // Relleno de longitud EXACTA `size`, con un prefijo único de ancho
            // fijo para reventar la caché sin alterar el byte-count del bucket.
            let prompt = build_prompt(size, run);

            if let Some(key) = &gemini_key {
                let model = env::var("GEMINI_MODEL")
                    .unwrap_or_else(|_| "gemini-2.0-flash".to_string());
                if let Err(e) = fire_gemini(&client, &base, key, &model, &prompt).await {
                    eprintln!("  gemini size={size} run={run}: {e}");
                } else {
                    sent += 1;
                }
            }
            if let Some(key) = &openai_key {
                let model =
                    env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
                if let Err(e) = fire_openai(&client, &base, key, &model, &prompt).await {
                    eprintln!("  openai size={size} run={run}: {e}");
                } else {
                    sent += 1;
                }
            }
            // Respiro para no chocar con rate limits.
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        println!("  ✓ tamaño {size} B barrido");
    }

    println!("\n📤 {sent} peticiones enviadas. Esperando el flush de telemetría…");
    // La telemetría se escribe async fuera del camino crítico: damos tiempo.
    tokio::time::sleep(Duration::from_secs(3)).await;

    report(&telemetry_path, rows_before);
}

/// Construye un prompt con instrucción fija + relleno de `size` bytes exactos.
/// `run` genera un prefijo único de ancho fijo (8 chars) para evitar caché.
fn build_prompt(size: usize, run: usize) -> String {
    let instruction = "Responde únicamente con la palabra: ok.\n";
    let unique = format!("r{run:07}"); // 8 chars, ancho fijo
    let mut filler = String::with_capacity(size);
    // Relleno neutro repetido hasta alcanzar exactamente `size` bytes.
    const PAD: &str = "lorem ipsum dolor sit amet consectetur adipiscing elit ";
    while filler.len() < size {
        filler.push_str(PAD);
    }
    filler.truncate(size);
    format!("{unique} {instruction}{filler}")
}

/// Dispara una petición a Gemini (Responses vía `:streamGenerateContent?alt=sse`).
async fn fire_gemini(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
    prompt: &str,
) -> Result<(), String> {
    let url =
        format!("{base}/v1beta/models/{model}:streamGenerateContent?alt=sse&key={key}");
    // `thinkingBudget: 0` desactiva el "thinking" (modelos 2.5+). Sin esto, con
    // un output chico el modelo gasta todo el presupuesto pensando y devuelve un
    // cuerpo VACÍO (sin usage) — contaminaría la barrida de tamaño de input.
    let body = json!({
        "contents": [{ "parts": [{ "text": prompt }] }],
        "generationConfig": {
            "maxOutputTokens": 16,
            "thinkingConfig": { "thinkingBudget": 0 }
        }
    });
    drain(client.post(&url).json(&body)).await
}

/// Dispara una petición a OpenAI (Responses API, streaming).
async fn fire_openai(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
    prompt: &str,
) -> Result<(), String> {
    let url = format!("{base}/v1/responses");
    let body = json!({
        "model": model,
        "input": prompt,
        "max_output_tokens": 16,
        "stream": true
    });
    drain(client.post(&url).bearer_auth(key).json(&body)).await
}

/// Envía la petición y consume la respuesta entera, para que el stream se cierre
/// y OxideGate emita la métrica. El contenido no nos interesa.
async fn drain(req: reqwest::RequestBuilder) -> Result<(), String> {
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let _ = resp.bytes().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("status {status}"));
    }
    Ok(())
}

/// Lee la telemetría de esta corrida y arma la tabla comparativa.
fn report(path: &PathBuf, skip: usize) {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let rows: Vec<Value> = content
        .lines()
        .skip(skip)
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    if rows.is_empty() {
        println!("(sin filas nuevas de telemetría — ¿está OxideGate arriba?)");
        return;
    }

    // Agrupamos por (proveedor, prompt_bytes): el byte-count es nuestra variable
    // controlada, estable entre repeticiones del mismo tamaño.
    let mut keys: Vec<(String, usize)> = Vec::new();
    for r in &rows {
        let up = r["upstream"].as_str().unwrap_or("?").to_string();
        let pb = r["prompt_bytes"].as_u64().unwrap_or(0) as usize;
        if !keys.contains(&(up.clone(), pb)) {
            keys.push((up, pb));
        }
    }
    keys.sort();

    println!("\n📊 Resultado — promedio por proveedor × tamaño de input\n");
    println!(
        "{:<10} {:>11} {:>13} {:>10} {:>11} {:>13} {:>7}",
        "proveedor", "prompt_bytes", "input_tokens", "ttft_ms", "total_ms", "cost_usd", "n"
    );
    println!("{}", "─".repeat(80));

    for (up, pb) in &keys {
        let group: Vec<&Value> = rows
            .iter()
            .filter(|r| {
                r["upstream"].as_str() == Some(up)
                    && r["prompt_bytes"].as_u64() == Some(*pb as u64)
            })
            .collect();
        let n = group.len();
        let in_tok = avg_u64(&group, "input_tokens");
        let ttft = avg_f64(&group, "ttft_ms");
        let total = avg_f64(&group, "total_ms");
        let cost = avg_f64(&group, "cost_estimate_usd");

        println!(
            "{:<10} {:>11} {:>13} {:>10} {:>11} {:>13} {:>7}",
            up,
            pb,
            fmt_opt_u64(in_tok),
            fmt_opt_f64(ttft, 1),
            fmt_opt_f64(total, 1),
            fmt_opt_f64(cost, 6),
            n
        );
    }
    println!(
        "\nNota: los tokens NO son comparables entre proveedores (tokenizadores\n\
         distintos); compará por prompt_bytes (variable controlada) y por cost_usd."
    );
}

/// Ruta del archivo de telemetría que escribe OxideGate.
fn telemetry_path() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let mut p = PathBuf::from(home);
    p.push(".config");
    p.push("oxidegate");
    p.push("telemetry.jsonl");
    p
}

fn count_lines(path: &PathBuf) -> usize {
    std::fs::read_to_string(path)
        .map(|c| c.lines().count())
        .unwrap_or(0)
}

/// Promedia un campo entero (ignora nulls). `None` si no hay ninguno.
fn avg_u64(rows: &[&Value], field: &str) -> Option<f64> {
    let vals: Vec<u64> = rows.iter().filter_map(|r| r[field].as_u64()).collect();
    if vals.is_empty() {
        None
    } else {
        Some(vals.iter().sum::<u64>() as f64 / vals.len() as f64)
    }
}

/// Promedia un campo flotante (ignora nulls). `None` si no hay ninguno.
fn avg_f64(rows: &[&Value], field: &str) -> Option<f64> {
    let vals: Vec<f64> = rows.iter().filter_map(|r| r[field].as_f64()).collect();
    if vals.is_empty() {
        None
    } else {
        Some(vals.iter().sum::<f64>() / vals.len() as f64)
    }
}

fn fmt_opt_u64(v: Option<f64>) -> String {
    v.map(|x| format!("{:.0}", x)).unwrap_or_else(|| "—".into())
}

fn fmt_opt_f64(v: Option<f64>, decimals: usize) -> String {
    v.map(|x| format!("{:.*}", decimals, x))
        .unwrap_or_else(|| "—".into())
}
