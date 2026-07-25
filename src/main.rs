//! Punto de entrada (Levanta el servidor local)
mod config;
mod middleware;
mod optimizer;
mod provider;
mod state;
mod telemetry;

use axum::{
    Router,
    routing::{get, post},
};
use config::AppConfig;
use state::AppState;
use std::net::SocketAddr;
use std::sync::Arc;
use telemetry::TelemetrySink;

/// Busca un flag saltándose SIEMPRE `argv[0]`: un binario invocado por un
/// path que contenga `-h` no es una petición de ayuda.
fn wants_flag(args: &[String], long: &str, short: &str) -> bool {
    args.iter().skip(1).any(|a| a == long || a == short)
}

fn usage_text() -> String {
    format!(
        "oxidegate {version} — proxy local que mide el coste real de contexto

USO:
    oxidegate                          Levanta el proxy y se queda escuchando
    oxidegate up                       Medidor + panel de una vez, en un terminal
    oxidegate run <cliente> [cmd...]   Lanza un cliente ya cableado a este proxy
    oxidegate doctor                   ¿Está funcionando? Diagnostica y explica
    oxidegate --help                   Muestra esta ayuda
    oxidegate --version                Muestra la versión

CABLEAR UN CLIENTE (`run`):
    oxidegate run claude               Claude Code, con ANTHROPIC_BASE_URL puesta
    oxidegate run gemini               Gemini CLI, con GOOGLE_GEMINI_BASE_URL
    oxidegate run openai python app.py Cualquier SDK OpenAI-compatible
    oxidegate run opencode             Explica cómo hacerlo (va por fichero)

    `run` pone la variable correcta con la forma correcta y lanza el cliente.
    El `/v1` va en unos clientes sí y en otros no, y equivocarse da un 404 que
    parece que la herramienta está rota: esto lo elimina. Requiere que el proxy
    ya esté corriendo — si no lo está, lo dice en vez de dejarte un cliente
    hablando con un puerto muerto.

VARIABLES DE ENTORNO:
    OXIDEGATE_PORT   Puerto de escucha (por defecto 8080). Conviene cambiarlo:
                     Apache, Tomcat y compañía suelen tener el 8080 ocupado, y
                     un cliente apuntando a su servidor no da ningún error
                     evidente.

RUTAS QUE SIRVE:
    GET  /health     Liveness barata. Los clientes la sondean para decidir si
                     enrutan por aquí; si devuelve 404 caen al proveedor
                     directo en silencio, sin error y sin log.
    GET  /stats      Agregado en vivo por (proveedor, modelo).
    GET  /sessions   Agregado por sesion: que costo cada sesion de trabajo.
    GET  /requests   Detalle de los últimos requests individuales.

VER TAMBIÉN:
    oxidegate-monitor        Panel en vivo sobre este proxy (--once para
                             un volcado de texto plano sin TUI).
",
        version = env!("CARGO_PKG_VERSION")
    )
}

fn version_text() -> String {
    format!("oxidegate {}", env!("CARGO_PKG_VERSION"))
}

/// Mensaje de un bind fallido. Es función pura para poder afirmar en tests
/// que el consejo cambia con la causa: sugerir `OXIDEGATE_PORT` ante un
/// fallo de permisos manda al usuario a perseguir el problema equivocado.
fn bind_error_message(port: u16, kind: std::io::ErrorKind, detail: &str) -> String {
    if kind == std::io::ErrorKind::AddrInUse {
        // El ejemplo se mueve con el puerto que falló. Un ejemplo fijo hacía
        // que quien ya estaba en 8899 leyera "el 8899 está ocupado, usa el
        // 8899", que no es un consejo. Por defecto se sugiere el mismo puerto
        // que recomiendan el README y los caveats de la fórmula.
        const RECOMENDADO: u16 = 8899;
        let sugerido = if port == RECOMENDADO {
            RECOMENDADO + 1
        } else {
            RECOMENDADO
        };
        format!(
            "oxidegate: el puerto {port} ya está ocupado.\n  \
             Puede ser otra instancia de OxideGate ya corriendo, o cualquier\n  \
             otro servidor. Elige uno libre:  OXIDEGATE_PORT={sugerido} oxidegate"
        )
    } else {
        format!("oxidegate: no se pudo escuchar en el puerto {port}: {detail}")
    }
}

/// Cómo se cablea un cliente para que su tráfico pase por el proxy.
///
/// La distinción entre las dos variantes NO es cosmética: `run` puede lanzar
/// por sí mismo los clientes que se cablean con una variable de entorno, pero
/// no los que exigen tocar un fichero de configuración. Modelar los dos casos
/// evita que el subcomando finja un cableado que no ha hecho — y un cableado
/// a medias devuelve al usuario exactamente al silencio que este eje existe
/// para eliminar.
enum ClientWiring {
    /// Se cablea exportando `var`. `needs_v1` decide si la base lleva `/v1`.
    Env {
        var: &'static str,
        needs_v1: bool,
    },
    /// Se cablea en un fichero: `run` no puede hacerlo, solo explicarlo.
    ConfigFile { hint: &'static str },
}

/// Clientes conocidos, en el orden en que se le listan al usuario.
const KNOWN_CLIENTS: [&str; 4] = ["claude", "gemini", "openai", "opencode"];

/// Resuelve el cableado de un cliente por nombre.
///
/// Esta tabla es la que el usuario tenía que reproducir a mano leyendo el
/// README: qué variable, y si la base lleva `/v1` o no. El `/v1` va en unos sí
/// y en otros no —Claude Code y Gemini construyen la ruta ellos mismos, los
/// clientes OpenAI-compatible esperan la base con `/v1`— y equivocarse produce
/// un 404 que parece que la herramienta está rota.
fn wiring_for(client: &str) -> Option<ClientWiring> {
    match client {
        "claude" => Some(ClientWiring::Env {
            var: "ANTHROPIC_BASE_URL",
            needs_v1: false,
        }),
        "gemini" => Some(ClientWiring::Env {
            var: "GOOGLE_GEMINI_BASE_URL",
            needs_v1: false,
        }),
        "openai" => Some(ClientWiring::Env {
            var: "OPENAI_BASE_URL",
            needs_v1: true,
        }),
        "opencode" => Some(ClientWiring::ConfigFile {
            hint: "OpenCode se cablea en ~/.config/opencode/opencode.json, no por \
                   entorno:\n     provider.<nombre>.options.baseURL = \
                   \"http://127.0.0.1:{PUERTO}/v1\"  (CON /v1)\n   \
                   Ver «Cablear cada cliente» en el README.",
        }),
        _ => None,
    }
}

/// Ejecutable que lanza `run <cliente>` cuando no se le pasa un comando.
///
/// `openai` devuelve `None` a propósito: no es un programa, es una familia de
/// SDKs. Ahí el comando lo pone el usuario (`oxidegate run openai python app.py`).
fn default_binary(client: &str) -> Option<&'static str> {
    match client {
        "claude" => Some("claude"),
        "gemini" => Some("gemini"),
        _ => None,
    }
}

/// Base-URL que hay que darle al cliente, con el `/v1` puesto solo donde toca.
fn wiring_base_url(port: u16, needs_v1: bool) -> String {
    if needs_v1 {
        format!("http://127.0.0.1:{port}/v1")
    } else {
        format!("http://127.0.0.1:{port}")
    }
}

/// Mensaje para un cliente que no sabemos cablear.
///
/// Lista los conocidos en vez de fallar en seco: quien escribe mal el nombre
/// necesita ver el correcto, no un "cliente no soportado".
fn unknown_client_message(client: &str) -> String {
    format!(
        "oxidegate: no sé cablear `{client}`.\n  \
         Clientes conocidos: {}\n  \
         Para cualquier otro, exporta su base-URL a mano — ver «Cablear cada \
         cliente» en el README.",
        KNOWN_CLIENTS.join(", ")
    )
}

/// Aviso de que el proxy no está escuchando en el puerto que `run` iba a usar.
///
/// Lanzar el cliente igualmente lo dejaría hablando con un puerto muerto: unos
/// clientes fallan con un error de conexión críptico y otros caen al proveedor
/// directo en silencio. Detenerse aquí y decir cómo arrancarlo evita las dos.
fn proxy_down_message(port: u16) -> String {
    format!(
        "oxidegate: no hay ningún proxy escuchando en 127.0.0.1:{port}.\n  \
         Arráncalo primero, en otra terminal:  OXIDEGATE_PORT={port} oxidegate\n  \
         (o exporta OXIDEGATE_PORT con el puerto donde ya lo tengas)"
    )
}

/// `true` si algo acepta conexiones TCP en ese puerto de loopback.
///
/// Comprobación deliberadamente barata: un `connect` y listo. No pide
/// `/health` porque `run` no necesita saber si el proxy está sano, solo si hay
/// alguien ahí — y un `connect` no arrastra un cliente HTTP a esta ruta.
fn proxy_is_listening(port: u16) -> bool {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    use std::time::Duration;

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Qué tiene que hacer `up` según lo que ya haya corriendo.
#[derive(Debug, PartialEq, Eq)]
enum UpPlan {
    /// Ya hay un proxy en el puerto: solo abrir el panel.
    SoloMonitor,
    /// No hay nada: arrancar el proxy y hacerse responsable de pararlo.
    ProxyYMonitor,
}

/// Decide el plan de `up`.
fn up_plan(ya_escuchando: bool) -> UpPlan {
    if ya_escuchando {
        UpPlan::SoloMonitor
    } else {
        UpPlan::ProxyYMonitor
    }
}

/// Cabecera que `up` imprime antes de ceder el terminal al panel.
///
/// `log` es `Some` solo cuando `up` arrancó el proxy él mismo. Si el proxy ya
/// estaba corriendo no se inventa una ruta: su salida es de quien lo arrancó.
fn up_banner(port: u16, log: Option<&std::path::Path>) -> String {
    match log {
        Some(p) => format!(
            "oxidegate: proxy arrancado en 127.0.0.1:{port}\n\
             salida del proxy: {}\n\
             abriendo el panel… (Ctrl-C para parar los dos)",
            p.display()
        ),
        None => format!(
            "oxidegate: ya había un proxy en 127.0.0.1:{port}, se reutiliza\n\
             abriendo el panel… (Ctrl-C cierra solo el panel)"
        ),
    }
}

/// Qué encontró `doctor` al mirar el puerto.
///
/// Las cuatro variantes son mutuamente excluyentes y cubren el espacio
/// completo. La distinción entre [`NothingListening`](DoctorFinding::NothingListening)
/// y [`SomethingElseListening`](DoctorFinding::SomethingElseListening) es la
/// que más trabajo ahorra: el 8080 lo ocupan Apache o Tomcat más a menudo de
/// lo que parece, y decirle a alguien "arranca el proxy" cuando el puerto ya
/// está cogido lo manda a perseguir el problema equivocado.
enum DoctorFinding {
    /// Nadie acepta conexiones en ese puerto.
    NothingListening,
    /// Algo contesta, pero no habla el `/health` de OxideGate.
    SomethingElseListening,
    /// OxideGate está sirviendo, pero no ha medido ni una petición.
    UpButIdle,
    /// OxideGate está midiendo. Lleva cuántas peticiones hay en el buffer.
    Measuring(usize),
}

/// Diagnóstico legible del estado del proxy.
///
/// Cada rama termina en una acción concreta. Un diagnóstico que solo describe
/// el síntoma deja al usuario donde estaba.
fn doctor_report(port: u16, finding: &DoctorFinding) -> String {
    match finding {
        DoctorFinding::NothingListening => format!(
            "✗ No hay nada escuchando en 127.0.0.1:{port}.\n  \
             El proxy no está levantado. Arráncalo:\n    \
             OXIDEGATE_PORT={port} oxidegate"
        ),
        DoctorFinding::SomethingElseListening => format!(
            "✗ Algo contesta en 127.0.0.1:{port}, pero NO ES OXIDEGATE.\n  \
             Acepta conexiones; lo que no devuelve es el /health de OxideGate.\n  \
             El 8080 lo ocupan Apache y Tomcat más a menudo de lo que parece, y\n  \
             un cliente apuntando a su servidor no da ningún error evidente.\n  \
             Elige otro puerto para el proxy y usa EL MISMO en el cliente:\n    \
             OXIDEGATE_PORT=8899 oxidegate"
        ),
        DoctorFinding::UpButIdle => format!(
            "✓ OxideGate está sirviendo en 127.0.0.1:{port}.\n\
             ✗ Pero no ha medido ni una petición.\n  \
             El tráfico no está pasando por aquí. Casi siempre es el cableado:\n    \
             oxidegate run claude        (pone la variable correcta y lanza)\n  \
             Si lo cableas a mano, cuidado con el /v1: Claude Code y Gemini van\n  \
             SIN él, los clientes OpenAI-compatible CON él. Equivocarse da un 404\n  \
             que parece que la herramienta está rota."
        ),
        DoctorFinding::Measuring(n) => format!(
            "✓ OxideGate está sirviendo en 127.0.0.1:{port}.\n\
             ✓ {n} peticiones medidas en el buffer.\n  \
             Todo en orden. Para verlas:\n    \
             oxidegate-monitor  — panel en vivo\n    \
             curl 127.0.0.1:{port}/stats  — agregado por modelo"
        ),
    }
}

/// Código de salida de `doctor`, para poder usarlo desde un script.
///
/// `0` SOLO cuando de verdad está midiendo: un proxy vivo que no mide nada no
/// es un éxito, es exactamente el fallo que este subcomando busca.
fn doctor_exit_code(finding: &DoctorFinding) -> i32 {
    match finding {
        DoctorFinding::Measuring(_) => 0,
        _ => 1,
    }
}

/// Localiza un binario hermano del que se está ejecutando.
///
/// `oxidegate` y `oxidegate-monitor` se instalan juntos (misma fórmula, mismo
/// `cargo install`), así que el hermano está al lado. Se prefiere esa ruta
/// antes que el PATH para no lanzar la versión de otra instalación.
fn sibling_binary(name: &str) -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| std::path::PathBuf::from(name))
}

/// Ejecuta `oxidegate up`: deja el medidor y el panel funcionando con un solo
/// comando.
///
/// # El conflicto que resuelve
///
/// El proxy escribe en stdout y el panel necesita el terminal entero: por eso
/// vivían en dos terminales distintas. Aquí el proxy va de proceso hijo con su
/// salida a un fichero, y el panel se queda con el TTY.
///
/// Si ya hay un proxy escuchando NO se arranca otro —moriría con `AddrInUse`
/// justo cuando todo está bien— y tampoco se para al salir: no es nuestro.
async fn up_subcommand(port: u16, storage_dir: std::path::PathBuf) -> i32 {
    let plan = up_plan(proxy_is_listening(port));

    let mut hijo = None;
    let mut log_path = None;

    if plan == UpPlan::ProxyYMonitor {
        let path = storage_dir.join("proxy.log");
        let log = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("oxidegate up: no se pudo crear {}: {e}", path.display());
                return 1;
            }
        };
        let errlog = match log.try_clone() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("oxidegate up: no se pudo duplicar el log: {e}");
                return 1;
            }
        };

        match std::process::Command::new(sibling_binary("oxidegate"))
            .env("OXIDEGATE_PORT", port.to_string())
            .stdout(log)
            .stderr(errlog)
            .spawn()
        {
            Ok(c) => hijo = Some(c),
            Err(e) => {
                eprintln!("oxidegate up: no se pudo arrancar el proxy: {e}");
                return 1;
            }
        }

        // Esperar a que el listener ate el puerto antes de ceder el terminal:
        // abrir el panel contra un proxy que aún no sirve lo muestra vacío y
        // parece que no funciona.
        let mut listo = false;
        for _ in 0..40 {
            if proxy_is_listening(port) {
                listo = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        if !listo {
            eprintln!(
                "oxidegate up: el proxy no llegó a escuchar en {port}. Mira {}",
                path.display()
            );
            if let Some(mut c) = hijo {
                let _ = c.kill();
            }
            return 1;
        }
        log_path = Some(path);
    }

    println!("{}", up_banner(port, log_path.as_deref()));

    let estado = std::process::Command::new(sibling_binary("oxidegate-monitor"))
        .env("OXIDEGATE_PORT", port.to_string())
        .status();

    // Solo se para lo que se arrancó aquí.
    if let Some(mut c) = hijo {
        let _ = c.kill();
        let _ = c.wait();
    }

    match estado {
        Ok(s) => s.code().unwrap_or(0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("oxidegate up: no encuentro `oxidegate-monitor` junto a este binario ni en el PATH.");
            127
        }
        Err(e) => {
            eprintln!("oxidegate up: no se pudo abrir el panel: {e}");
            1
        }
    }
}

/// Ejecuta `oxidegate doctor`: mira el puerto y explica qué pasa.
async fn doctor_subcommand(port: u16) -> i32 {
    let finding = if !proxy_is_listening(port) {
        DoctorFinding::NothingListening
    } else {
        let http = reqwest::Client::new();
        let health = http
            .get(format!("http://127.0.0.1:{port}/health"))
            .timeout(std::time::Duration::from_secs(2))
            .send()
            .await;

        // Que conteste 200 no basta: cualquier servidor puede devolver 200 en
        // una ruta desconocida. Se exige el payload de `/health` para no
        // confundir a un vecino de puerto con el proxy.
        let is_oxidegate = match health {
            Ok(r) if r.status().is_success() => r
                .text()
                .await
                .map(|b| b.contains("\"status\"") && b.contains("\"ok\""))
                .unwrap_or(false),
            _ => false,
        };

        if !is_oxidegate {
            DoctorFinding::SomethingElseListening
        } else {
            let measured = http
                .get(format!("http://127.0.0.1:{port}/requests"))
                .timeout(std::time::Duration::from_secs(2))
                .send()
                .await
                .ok();

            let count = match measured {
                Some(r) => r
                    .json::<Vec<serde_json::Value>>()
                    .await
                    .map(|rows| rows.len())
                    .unwrap_or(0),
                None => 0,
            };

            if count == 0 {
                DoctorFinding::UpButIdle
            } else {
                DoctorFinding::Measuring(count)
            }
        }
    };

    println!("{}", doctor_report(port, &finding));
    doctor_exit_code(&finding)
}

/// Ejecuta `oxidegate run <cliente> [comando...]`.
///
/// Devuelve el código de salida con el que debe terminar el proceso. Nunca
/// hace `panic`: cada rama de fallo explica qué pasó y qué hacer.
fn run_subcommand(args: &[String], port: u16) -> i32 {
    let Some(client) = args.first() else {
        eprintln!(
            "oxidegate run: falta el cliente.\n  \
             Uso: oxidegate run <cliente> [comando...]\n  \
             Clientes conocidos: {}",
            KNOWN_CLIENTS.join(", ")
        );
        return 2;
    };

    let (var, needs_v1) = match wiring_for(client) {
        Some(ClientWiring::Env { var, needs_v1 }) => (var, needs_v1),
        Some(ClientWiring::ConfigFile { hint }) => {
            eprintln!("oxidegate run: {hint}");
            return 2;
        }
        None => {
            eprintln!("{}", unknown_client_message(client));
            return 2;
        }
    };

    let rest = &args[1..];
    let (program, program_args): (&str, &[String]) = match rest.split_first() {
        Some((first, tail)) => (first.as_str(), tail),
        None => match default_binary(client) {
            Some(bin) => (bin, &[][..]),
            None => {
                eprintln!(
                    "oxidegate run: `{client}` no es un ejecutable, es una familia de \
                     SDKs.\n  Dime qué lanzar:  oxidegate run {client} <comando> [args...]"
                );
                return 2;
            }
        },
    };

    if !proxy_is_listening(port) {
        eprintln!("{}", proxy_down_message(port));
        return 1;
    }

    let base = wiring_base_url(port, needs_v1);
    println!("oxidegate: {var}={base}  →  {program}");

    match std::process::Command::new(program)
        .args(program_args)
        .env(var, &base)
        .status()
    {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("oxidegate run: no encuentro `{program}` en el PATH.");
            127
        }
        Err(e) => {
            eprintln!("oxidegate run: no se pudo lanzar `{program}`: {e}");
            1
        }
    }
}

#[tokio::main]
async fn main() {
    // Ayuda y versión se responden ANTES de tocar nada: ni configuración, ni
    // carpeta de datos, ni bind. `oxidegate --help` con una instancia ya
    // corriendo panicaba con AddrInUse — justo el momento en el que más falta
    // hace poder leer la ayuda.
    let args: Vec<String> = std::env::args().collect();

    // `run` se despacha aquí por el mismo motivo que la ayuda: no debe tocar
    // el puerto. Lanza un cliente CONTRA un proxy que ya está corriendo; si
    // intentara bindear moriría con AddrInUse justo cuando todo va bien.
    if args.get(1).is_some_and(|a| a == "run") {
        let port = std::env::var("OXIDEGATE_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8080);
        std::process::exit(run_subcommand(&args[2..], port));
    }

    // `up` tampoco bindea en este proceso: arranca el proxy como HIJO y se
    // queda de supervisor. Bindear aquí dejaría al hijo sin puerto.
    if args.get(1).is_some_and(|a| a == "up") {
        let port = std::env::var("OXIDEGATE_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8080);
        let dir = AppConfig::load().storage_dir;
        std::fs::create_dir_all(&dir).unwrap_or_default();
        std::process::exit(up_subcommand(port, dir).await);
    }

    // `doctor` tampoco bindea: inspecciona un proxy ajeno, el que ya esté
    // corriendo. Bindear aquí haría que el diagnóstico se autodestruyera.
    if args.get(1).is_some_and(|a| a == "doctor") {
        let port = std::env::var("OXIDEGATE_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8080);
        std::process::exit(doctor_subcommand(port).await);
    }

    if wants_flag(&args, "--help", "-h") {
        print!("{}", usage_text());
        return;
    }
    if wants_flag(&args, "--version", "-V") {
        println!("{}", version_text());
        return;
    }

    // Inicializamos la telemetría interna por consola
    tracing_subscriber::fmt::init();

    // Cargamos la configuración independiente de OxideGate
    let config = AppConfig::load();

    // Aseguramos que nuestra carpeta de datos exista de forma interna
    if !config.storage_dir.exists() {
        std::fs::create_dir_all(&config.storage_dir).unwrap_or_default();
    }

    println!("🚀 OxideGate inicializado en local.");
    println!(
        "📦 Almacenamiento de telemetría nativa en: {:?}",
        config.storage_dir
    );
    if config.has_opencode_env() {
        println!("🔍 Entorno OpenCode detectado en el sistema.");
    }

    // Arrancamos la task de telemetría (escribe fuera del camino crítico).
    let telemetry = TelemetrySink::spawn(config.storage_dir.clone());

    let port = config.local_port;
    let state = AppState {
        config: Arc::new(config),
        http: reqwest::Client::new(),
        telemetry,
    };

    // Definimos las rutas espejo del proxy para capturar las peticiones
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(middleware::proxy::handle_openai_route),
        )
        .route(
            "/v1/messages",
            post(middleware::proxy::handle_anthropic_route),
        )
        // OpenAI Responses API (clientes modernos: Codex, SDKs nuevos).
        .route(
            "/v1/responses",
            post(middleware::proxy::handle_openai_responses),
        )
        // Responses API de Codex (`pi`): mismo dialecto que la de arriba,
        // pero reenviada a chatgpt.com/backend-api/codex en vez de
        // api.openai.com. Body a veces comprimido en zstd.
        .route(
            "/v1/codex/responses",
            post(middleware::proxy::handle_openai_codex_responses),
        )
        // Ruta comodín de Gemini: captura `/v1beta/models/{model}:{método}`.
        .route(
            "/v1beta/*rest",
            post(middleware::proxy::handle_gemini_route),
        )
        // Liveness barata: no depende de AppState ni toma locks de
        // telemetría. La usa el plugin de OpenCode para decidir si redirige
        // tráfico de Codex hacia acá antes de tocar nada más pesado.
        .route("/health", get(middleware::health::handle_health))
        // Agregación en vivo por (proveedor, modelo): qué optimizar ahora.
        .route("/stats", get(middleware::stats::handle_stats))
        // Agregación por SESIÓN: qué costó cada sesión de trabajo, no cada
        // modelo. Endpoint aparte para no romper la forma de `/stats`.
        .route("/sessions", get(middleware::sessions::handle_sessions))
        // Detalle en vivo de los últimos requests individuales: qué request
        // puntual es atípico (outlier de coste/latencia).
        .route("/requests", get(middleware::requests::handle_requests))
        .with_state(Arc::new(state));

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    // Un `.unwrap()` aquí convertía el caso más común de todos — el puerto
    // ocupado — en un panic con backtrace, que no le dice a nadie qué hacer.
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("{}", bind_error_message(port, e.kind(), &e.to_string()));
            std::process::exit(1);
        }
    };

    println!("🛰️  Escuchando en http://{addr}");
    println!("💚 Liveness en http://{addr}/health");
    println!("📊 Estadísticas en vivo por modelo en http://{addr}/stats");
    println!("🧾 Últimos requests en vivo en http://{addr}/requests");
    axum::serve(listener, app).await.unwrap();
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn argv(rest: &[&str]) -> Vec<String> {
        std::iter::once("oxidegate".to_string())
            .chain(rest.iter().map(|s| s.to_string()))
            .collect()
    }

    #[test]
    fn asks_for_help_with_either_spelling() {
        assert!(wants_flag(&argv(&["--help"]), "--help", "-h"));
        assert!(wants_flag(&argv(&["-h"]), "--help", "-h"));
        assert!(!wants_flag(&argv(&[]), "--help", "-h"));
    }

    #[test]
    fn argv_zero_is_never_a_flag() {
        // Un binario invocado por un path que contenga "-h" no debe
        // confundirse con una petición de ayuda. Se salta argv[0] siempre.
        let disguised = vec!["-h".to_string()];
        assert!(!wants_flag(&disguised, "--help", "-h"));
    }

    #[test]
    fn usage_names_the_port_knob_and_every_route_it_serves() {
        let text = usage_text();
        // El puerto es lo primero que un usuario necesita cambiar: el default
        // 8080 lo suelen ocupar Apache y compañía.
        assert!(
            text.contains("OXIDEGATE_PORT"),
            "falta OXIDEGATE_PORT: {text}"
        );
        // /health es lo que sondean los clientes antes de enrutar. Si no se
        // documenta, un 404 ahí es indistinguible de "el proxy no arranca".
        for route in ["/health", "/stats", "/requests"] {
            assert!(text.contains(route), "falta la ruta {route}: {text}");
        }
    }

    #[test]
    fn version_text_carries_the_crate_version() {
        assert!(version_text().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn address_in_use_says_which_port_and_how_to_change_it() {
        let msg = bind_error_message(
            8080,
            std::io::ErrorKind::AddrInUse,
            "Address already in use",
        );
        assert!(msg.contains("8080"), "no nombra el puerto: {msg}");
        assert!(
            msg.contains("OXIDEGATE_PORT"),
            "no dice como cambiarlo: {msg}"
        );
    }

    #[test]
    fn the_suggested_port_is_never_the_occupied_one() {
        // Un primer intento sugería `OXIDEGATE_PORT=8899` con un ejemplo fijo,
        // así que quien ya estaba en 8899 leía "el 8899 está ocupado, usa el
        // 8899". El ejemplo tiene que moverse con el puerto que falló.
        for port in [8080u16, 8899, 9999] {
            let msg = bind_error_message(
                port,
                std::io::ErrorKind::AddrInUse,
                "Address already in use",
            );
            let suggestion = msg
                .split("OXIDEGATE_PORT=")
                .nth(1)
                .expect("el mensaje debe sugerir un puerto concreto");
            let suggested: u16 = suggestion
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .expect("la sugerencia debe ser un puerto parseable");
            assert_ne!(
                suggested, port,
                "sugiere el puerto que acaba de fallar: {msg}"
            );
        }
    }

    #[test]
    fn other_bind_errors_do_not_claim_the_port_is_taken() {
        // Un fallo de permisos no es un puerto ocupado. Sugerir
        // OXIDEGATE_PORT ahi manda al usuario a perseguir el problema
        // equivocado.
        let msg = bind_error_message(
            80,
            std::io::ErrorKind::PermissionDenied,
            "Permission denied",
        );
        assert!(!msg.contains("OXIDEGATE_PORT"), "consejo equivocado: {msg}");
        assert!(
            msg.contains("Permission denied"),
            "pierde la causa real: {msg}"
        );
    }

    // --- `oxidegate run <cliente>` ---

    /// Claude Code construye la ruta él mismo (`/v1/messages`), así que su
    /// base va SIN `/v1`. Es la mitad de la trampa que este subcomando existe
    /// para eliminar.
    #[test]
    fn claude_se_cablea_sin_v1() {
        let Some(ClientWiring::Env { var, needs_v1 }) = wiring_for("claude") else {
            panic!("claude debe cablearse por variable de entorno");
        };

        assert_eq!(var, "ANTHROPIC_BASE_URL");
        assert!(!needs_v1, "Claude Code no lleva /v1");
    }

    /// El CLI de Gemini pega `/v1beta/models/...`: su base tampoco lleva
    /// `/v1`, y encima usa una variable distinta.
    #[test]
    fn gemini_usa_su_propia_variable_y_tampoco_lleva_v1() {
        let Some(ClientWiring::Env { var, needs_v1 }) = wiring_for("gemini") else {
            panic!("gemini debe cablearse por variable de entorno");
        };

        assert_eq!(var, "GOOGLE_GEMINI_BASE_URL");
        assert!(!needs_v1, "el CLI de Gemini no lleva /v1");
    }

    /// La otra mitad de la trampa: los clientes OpenAI-compatible esperan la
    /// base CON `/v1` y le pegan `/chat/completions` detrás.
    #[test]
    fn los_clientes_openai_compatible_si_llevan_v1() {
        let Some(ClientWiring::Env { var, needs_v1 }) = wiring_for("openai") else {
            panic!("openai debe cablearse por variable de entorno");
        };

        assert_eq!(var, "OPENAI_BASE_URL");
        assert!(needs_v1, "los clientes OpenAI-compatible SÍ llevan /v1");
    }

    /// OpenCode no se cablea por entorno sino por `opencode.json`. `run` no
    /// puede lanzarlo bien, y decirlo es mejor que fingir que sí: un cableado
    /// a medias vuelve al silencio que este eje intenta eliminar.
    #[test]
    fn opencode_se_reconoce_pero_declara_que_necesita_fichero() {
        let Some(ClientWiring::ConfigFile { hint }) = wiring_for("opencode") else {
            panic!("opencode debe reconocerse como cableado por fichero");
        };

        assert!(
            hint.contains("opencode.json"),
            "no dice dónde se configura: {hint}"
        );
    }

    /// Un cliente que no conocemos no se cablea a ciegas: se listan los que sí.
    #[test]
    fn cliente_desconocido_lista_los_conocidos() {
        assert!(wiring_for("emacs").is_none());

        let msg = unknown_client_message("emacs");
        assert!(msg.contains("emacs"), "no repite lo que se pidió: {msg}");
        for conocido in ["claude", "gemini", "openai", "opencode"] {
            assert!(
                msg.contains(conocido),
                "no lista `{conocido}` entre los conocidos: {msg}"
            );
        }
    }

    /// La base apunta al puerto real del proxy, y el `/v1` se pone solo donde
    /// toca. Es exactamente el cálculo que el usuario hacía a mano y fallaba.
    #[test]
    fn la_base_url_respeta_el_puerto_y_pone_el_v1_solo_donde_toca() {
        assert_eq!(wiring_base_url(8899, false), "http://127.0.0.1:8899");
        assert_eq!(wiring_base_url(8899, true), "http://127.0.0.1:8899/v1");
        assert_eq!(wiring_base_url(8080, false), "http://127.0.0.1:8080");
    }

    /// La ayuda anuncia `run`. Un subcomando que solo existe en el README es
    /// un subcomando que nadie usa.
    #[test]
    fn la_ayuda_anuncia_el_subcomando_run() {
        let text = usage_text();

        assert!(text.contains("run"), "no menciona `run`: {text}");
        for conocido in KNOWN_CLIENTS {
            assert!(
                text.contains(conocido),
                "la ayuda no lista `{conocido}`: {text}"
            );
        }
    }

    /// La ayuda anuncia `doctor`. Un diagnóstico que el usuario no sabe que
    /// existe no diagnostica nada.
    #[test]
    fn la_ayuda_anuncia_el_subcomando_doctor() {
        let text = usage_text();

        assert!(text.contains("doctor"), "no menciona `doctor`: {text}");
    }

    /// Sin comando explícito, `run claude` lanza el binario del propio
    /// cliente. `openai` no tiene uno: es una familia de SDKs, no un
    /// ejecutable, así que ahí el comando es obligatorio.
    #[test]
    fn el_binario_por_defecto_existe_solo_donde_hay_uno() {
        assert_eq!(default_binary("claude"), Some("claude"));
        assert_eq!(default_binary("gemini"), Some("gemini"));
        assert_eq!(
            default_binary("openai"),
            None,
            "`openai` es una familia de SDKs, no un ejecutable"
        );
    }

    // --- `oxidegate up` ---

    /// Si ya hay un proxy escuchando, `up` NO arranca otro: solo abre el
    /// panel. Arrancar un segundo moriría con AddrInUse justo cuando todo
    /// está bien, que es el peor momento para un error.
    #[test]
    fn up_no_arranca_un_segundo_proxy_si_ya_hay_uno() {
        assert_eq!(up_plan(true), UpPlan::SoloMonitor);
    }

    /// Sin proxy, `up` arranca uno propio y se hace responsable de pararlo.
    #[test]
    fn up_arranca_el_proxy_si_no_hay_ninguno() {
        assert_eq!(up_plan(false), UpPlan::ProxyYMonitor);
    }

    /// El aviso dice DÓNDE queda la salida del proxy. Un proceso hijo cuyo
    /// stdout desaparece sin decir dónde es un proceso que no se puede
    /// depurar cuando falle.
    #[test]
    fn up_dice_donde_queda_el_log_del_proxy() {
        let msg = up_banner(8899, Some(std::path::Path::new("/tmp/oxi/proxy.log")));

        assert!(msg.contains("8899"), "no nombra el puerto: {msg}");
        assert!(
            msg.contains("/tmp/oxi/proxy.log"),
            "no dice dónde queda la salida: {msg}"
        );
    }

    /// Cuando el proxy ya estaba corriendo, `up` no inventa un log que no ha
    /// creado: lo dice y punto.
    #[test]
    fn up_sin_log_propio_no_menciona_ninguno() {
        let msg = up_banner(8899, None);

        assert!(msg.contains("8899"), "no nombra el puerto: {msg}");
        assert!(
            !msg.contains(".log"),
            "menciona un log que no ha creado: {msg}"
        );
    }

    // --- `oxidegate doctor` ---

    /// Sin nada escuchando, el diagnóstico nombra el puerto y dice cómo
    /// arrancar el proxy. Es el caso más común y el más fácil de resolver.
    #[test]
    fn doctor_sin_nada_escuchando_dice_como_arrancarlo() {
        let msg = doctor_report(8899, &DoctorFinding::NothingListening);

        assert!(msg.contains("8899"), "no nombra el puerto: {msg}");
        assert!(
            msg.contains("OXIDEGATE_PORT=8899 oxidegate"),
            "no dice cómo arrancarlo: {msg}"
        );
    }

    /// Hay algo en el puerto pero NO es OxideGate — el caso de Apache o Tomcat
    /// ocupando el 8080. Decir "arranca el proxy" aquí manda al usuario a
    /// perseguir el problema equivocado: el proxy quizá ya está corriendo en
    /// otro sitio, o el puerto simplemente no está libre.
    #[test]
    fn doctor_distingue_otro_servidor_de_un_proxy_ausente() {
        let msg = doctor_report(8080, &DoctorFinding::SomethingElseListening);

        assert!(msg.contains("8080"), "no nombra el puerto: {msg}");
        assert!(
            !msg.contains("OXIDEGATE_PORT=8080 oxidegate"),
            "sugiere arrancar en el puerto que ya está ocupado: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("no es oxidegate"),
            "no dice que quien contesta es otro: {msg}"
        );
    }

    /// El proxy está vivo pero no ha medido nada: el cableado es la causa
    /// probable, y el diagnóstico apunta a `run` y al `/v1` en vez de dejar
    /// al usuario adivinando.
    #[test]
    fn doctor_vivo_sin_medir_senala_el_cableado() {
        let msg = doctor_report(8899, &DoctorFinding::UpButIdle);

        assert!(
            msg.contains("oxidegate run"),
            "no ofrece la salida fácil: {msg}"
        );
        assert!(msg.contains("/v1"), "no menciona la trampa del /v1: {msg}");
    }

    /// Midiendo: dice cuántas y dónde mirar. Un diagnóstico que solo dice
    /// "todo bien" no ayuda a confirmar que lo medido es lo esperado.
    #[test]
    fn doctor_midiendo_dice_cuantas_y_donde_mirar() {
        let msg = doctor_report(8899, &DoctorFinding::Measuring(42));

        assert!(msg.contains("42"), "no dice cuántas: {msg}");
        assert!(
            msg.contains("oxidegate-monitor"),
            "no dice dónde mirar: {msg}"
        );
    }

    /// El código de salida hace `doctor` usable desde un script: 0 solo
    /// cuando de verdad está midiendo. Un proxy vivo que no mide nada NO es
    /// un éxito — es justo el fallo que este subcomando existe para detectar.
    #[test]
    fn doctor_solo_sale_con_cero_cuando_mide() {
        assert_eq!(doctor_exit_code(&DoctorFinding::Measuring(1)), 0);
        assert_ne!(doctor_exit_code(&DoctorFinding::UpButIdle), 0);
        assert_ne!(doctor_exit_code(&DoctorFinding::NothingListening), 0);
        assert_ne!(doctor_exit_code(&DoctorFinding::SomethingElseListening), 0);
    }

    /// Lanzar un cliente contra un proxy que no está levantado produce
    /// exactamente el fallo confuso que este eje quiere eliminar. El aviso
    /// nombra el puerto y dice cómo arrancarlo.
    #[test]
    fn el_aviso_de_proxy_caido_nombra_el_puerto_y_como_arrancarlo() {
        let msg = proxy_down_message(8899);

        assert!(msg.contains("8899"), "no nombra el puerto: {msg}");
        assert!(
            msg.contains("OXIDEGATE_PORT=8899 oxidegate"),
            "no dice cómo arrancarlo: {msg}"
        );
    }
}
