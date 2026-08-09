//! Banco de captura: guarda el cuerpo CRUDO que manda un harness y lo reenvía
//! a un modelo LOCAL, para poder medir qué inyecta cada herramienta sin gastar
//! un solo token.
//!
//! # Por qué existe
//!
//! Medir el peaje fijo exige leer el cuerpo real de la petición. El método de
//! [`docs/fixed-toll-claude-code.md`] §4 —una sonda que guarda el body y
//! devuelve `400`— solo está verificado para Claude Code. Con Codex falló, y
//! caro: `codex exec` ignoró `OPENAI_BASE_URL`, se fue a su auth de
//! suscripción guardada y **gastó 16.185 tokens de cuota real sin capturar
//! nada**.
//!
//! Este banco arregla las dos mitades de ese fallo.
//!
//! # 1. Reenvía en vez de devolver un error
//!
//! Un harness que recibe un `400` puede reintentar, cambiar de ruta o caer a
//! otro backend, y entonces lo capturado no es la petición normal sino la de
//! después del fallo. Con una respuesta de verdad el harness termina, y lo que
//! queda en disco es exactamente lo que manda a diario.
//!
//! El modelo es LOCAL (ollama) a propósito: **el bloque de instrucciones lo
//! inyecta el HARNESS, no el modelo**, así que medir contra un modelo de tu
//! máquina mide lo mismo que contra uno de pago. Eso separa el método del
//! precio, que es lo único transferible de estas mediciones
//! (`docs/fixed-toll-claude-code.md` §5) — y hace que cualquiera pueda
//! reproducirlas sin cuenta, sin cuota y sin red.
//!
//! # 2. El aislamiento es lo que lo hace seguro
//!
//! Apuntar bien no basta: hay que quitar de en medio las credenciales a las
//! que el harness podría caer. Con `CODEX_HOME` en un directorio sin
//! `auth.json`, Codex **no puede** gastar cuota aunque el apuntado falle — el
//! peor caso es un error de auth. Ver `docs/banco-de-captura.md` para el
//! procedimiento por herramienta.
//!
//! Este binario refuerza esa garantía por su lado: **el upstream es siempre
//! `127.0.0.1`**, no se lee de una variable de entorno y no se reenvía ninguna
//! cabecera del harness (podrían llevar credenciales). Si alguien lo cambia
//! para apuntar fuera, el coste deja de ser cero.
//!
//! # Uso
//!
//! ```sh
//! cargo run --example captura
//! # en otra terminal, con la config del harness aislada y apuntando al banco
//! ```
//!
//! Variables:
//!   CAPTURA_PORT     puerto del banco (default 8912)
//!   CAPTURA_DIR      dónde guardar los cuerpos (default ./capturas)
//!   CAPTURA_MODELO   modelo local al que reenviar (default qwen2.5:7b)
//!   CAPTURA_OLLAMA   puerto de ollama (default 11434)
//!
//! Vive en `examples/` por el mismo motivo que `bench.rs`: **Cargo no instala
//! examples**, así que una herramienta de medición no acaba en el PATH de
//! nadie. Y sí se compila en `cargo test`, así que no se pudre en silencio.
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{StatusCode, Uri, header},
    response::Response,
    routing::post,
};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Todo lo que el banco necesita saber. El upstream NO viaja aquí como URL
/// completa a propósito: solo el puerto, y el host es constante.
struct Banco {
    dir: PathBuf,
    modelo: String,
    ollama_port: u16,
    cliente: reqwest::Client,
    contador: AtomicUsize,
}

impl Banco {
    /// Nombre de fichero para la siguiente captura: ordinal + ruta saneada,
    /// para que el orden de llegada se lea de un `ls`.
    fn siguiente(&self, ruta: &str) -> PathBuf {
        let n = self.contador.fetch_add(1, Ordering::Relaxed) + 1;
        let limpio: String = ruta
            .trim_matches('/')
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let limpio = if limpio.is_empty() { "raiz" } else { &limpio };
        self.dir.join(format!("{n:02}-{limpio}.json"))
    }
}

/// Cambia el modelo pedido por el local. Es lo ÚNICO que se toca del cuerpo, y
/// se hace DESPUÉS de guardarlo: lo capturado es siempre el original.
///
/// Un cuerpo que no es JSON, o que no declara `model`, viaja intacto: no es
/// asunto del banco entender todos los dialectos.
fn con_modelo_local(crudo: &[u8], modelo: &str) -> Vec<u8> {
    let Ok(mut cuerpo) = serde_json::from_slice::<Value>(crudo) else {
        return crudo.to_vec();
    };
    match cuerpo.get_mut("model") {
        Some(m) => *m = Value::String(modelo.to_string()),
        None => return crudo.to_vec(),
    }
    serde_json::to_vec(&cuerpo).unwrap_or_else(|_| crudo.to_vec())
}

async fn captura(State(banco): State<Arc<Banco>>, uri: Uri, cuerpo: Bytes) -> Response {
    let ruta = uri.path().to_string();
    let destino = banco.siguiente(&ruta);
    if let Err(e) = std::fs::write(&destino, &cuerpo) {
        eprintln!("banco: no se pudo guardar {}: {e}", destino.display());
    } else {
        println!(
            "CAPTURADO {} B  {ruta}  -> {}",
            cuerpo.len(),
            destino
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
    }

    let reenviado = con_modelo_local(&cuerpo, &banco.modelo);
    // Host FIJO. No sale de una variable, no sale del request: el banco no
    // puede apuntar fuera de esta máquina.
    let url = format!("http://127.0.0.1:{}{ruta}", banco.ollama_port);

    // NINGUNA cabecera del harness se reenvía: pueden llevar credenciales, y
    // el modelo local no las necesita.
    let resp = match banco
        .cliente
        .post(&url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(reenviado)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("banco: el modelo local no contestó: {e}");
            return error_json(&format!("upstream local: {e}"));
        }
    };

    let status = resp.status();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|_| error_json("no se pudo construir la respuesta"))
}

fn error_json(msg: &str) -> Response {
    let cuerpo = serde_json::json!({"error": {"type": "banco_error", "message": msg}});
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(cuerpo.to_string()))
        .expect("respuesta de error siempre construible")
}

fn var_u16(nombre: &str, defecto: u16) -> u16 {
    std::env::var(nombre)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(defecto)
}

#[tokio::main]
async fn main() {
    let dir =
        PathBuf::from(std::env::var("CAPTURA_DIR").unwrap_or_else(|_| "./capturas".to_string()));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("banco: no se pudo crear {}: {e}", dir.display());
        std::process::exit(1);
    }

    let port = var_u16("CAPTURA_PORT", 8912);
    let banco = Arc::new(Banco {
        dir: dir.clone(),
        modelo: std::env::var("CAPTURA_MODELO").unwrap_or_else(|_| "qwen2.5:7b".to_string()),
        ollama_port: var_u16("CAPTURA_OLLAMA", 11434),
        cliente: reqwest::Client::new(),
        contador: AtomicUsize::new(0),
    });

    // Comodín: cada harness pega a una ruta distinta y el banco no tiene por
    // qué conocerlas. Lo que importa es el cuerpo, no dónde lo dejan.
    let app = Router::new()
        .route("/*ruta", post(captura))
        .with_state(banco.clone());

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("banco: no se pudo escuchar en 127.0.0.1:{port}: {e}");
            std::process::exit(1);
        }
    };

    println!("🧪 banco de captura en http://127.0.0.1:{port}");
    println!(
        "   reenvía a ollama/{} en 127.0.0.1:{}",
        banco.modelo, banco.ollama_port
    );
    println!("   cuerpos crudos en {}", dir.display());
    println!("   NADA sale de esta máquina. Coste cero.");

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("banco: {e}");
        std::process::exit(1);
    }
}
