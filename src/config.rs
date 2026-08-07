//! Lee el entorno
use std::env;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

/// Resuelve el host de bind desde el valor crudo de `OXIDEGATE_HOST`.
/// Función pura para poder afirmar en tests sin tocar el entorno del proceso.
///
/// **Falla CERRADO**: un valor ilegible NO abre el proxy a la red. Vuelve a
/// loopback y devuelve el aviso, porque el error opuesto —exponer el proxy por
/// un typo— es el único de los dos que no se puede deshacer.
pub fn parse_bind_host(raw: Option<&str>) -> (IpAddr, Option<String>) {
    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    let Some(valor) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return (LOOPBACK, None);
    };

    // `localhost` no es una IP, pero es lo que la gente escribe.
    if valor.eq_ignore_ascii_case("localhost") {
        return (LOOPBACK, None);
    }

    match valor.parse::<IpAddr>() {
        Ok(ip) => (ip, None),
        Err(_) => (
            LOOPBACK,
            Some(format!(
                "oxidegate: OXIDEGATE_HOST={valor:?} no es una dirección IP válida.\n  \
                 Se bindea en 127.0.0.1 (solo esta máquina) para no exponer el proxy por un typo.\n  \
                 Para abrirlo a la red a propósito: OXIDEGATE_HOST=0.0.0.0"
            )),
        ),
    }
}

/// Aviso de exposición cuando el bind sale de loopback. `None` si es seguro.
///
/// No es paternalismo: `GET /requests` publica la telemetría y el campo
/// `client` es contenido controlado por el cliente (ver
/// `docs/telemetry-per-request.md` §4.5). Abrir el bind sin decirlo en voz
/// alta sería exactamente el fallo silencioso que este proyecto se niega a
/// cometer con los datos.
pub fn exposure_warning(host: IpAddr) -> Option<String> {
    if host.is_loopback() {
        return None;
    }
    Some(format!(
        "⚠️  oxidegate escucha en {host}: alcanzable desde FUERA de esta máquina.\n   \
         Quien llegue al puerto puede leer /requests (telemetría, incluido `client`\n   \
         crudo — ver docs/telemetry-per-request.md §4.5) y usar el proxy como\n   \
         pasarela hacia los proveedores. Ponlo detrás de un firewall o una red\n   \
         de confianza."
    ))
}

/// Niveles de esfuerzo que acepta el dialecto de Anthropic.
///
/// Se usa **solo como lista de valores válidos** para [`parse_force_effort`].
/// El orden en que están escritos es el del dialecto, de menos a más, pero
/// **nada lo consulta**: la palanca B mueve el nivel en las DOS direcciones.
///
/// Es deliberado, y conviene decirlo porque la intuición dice lo contrario:
/// un "optimizador" que solo bajara dejaría de servir como instrumento. El
/// A/B que midió el −20% de tokens de salida necesitó forzar `high` y forzar
/// `low` sobre la misma tarea para poder compararlas. Esta palanca es
/// *forzar* un nivel, no *bajarlo*.
///
/// Consecuencia que el operador tiene que saber: configurar un nivel más alto
/// del que pide el cliente **sube el coste**. La fila lo declara igual
/// (`effort_forced`), pero el proxy no va a impedírtelo ni a adivinar tu
/// intención.
pub const NIVELES_DE_ESFUERZO: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Resuelve la palanca B desde el valor crudo de `OXIDEGATE_FORCE_EFFORT`.
/// Función pura para poder afirmar en tests sin tocar el entorno del proceso.
///
/// **Falla CERRADO**, igual que [`parse_bind_host`]: un valor que no reconoce
/// deja la palanca APAGADA y devuelve el aviso. El error opuesto —mutar cada
/// petición por un typo, en un proxy cuya promesa es no tocar nada— es el
/// único de los dos que no se nota hasta que ya has medido mal un día entero.
///
/// Devuelve `(objetivo, aviso)`. `objetivo` es `None` si la variable está
/// ausente, vacía, o no es uno de [`NIVELES_DE_ESFUERZO`].
pub fn parse_force_effort(raw: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(valor) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return (None, None);
    };

    let normalizado = valor.to_ascii_lowercase();
    if NIVELES_DE_ESFUERZO.contains(&normalizado.as_str()) {
        return (Some(normalizado), None);
    }

    (
        None,
        Some(format!(
            "oxidegate: OXIDEGATE_FORCE_EFFORT={valor:?} no es un nivel de esfuerzo.\n  \
             La palanca queda APAGADA: el proxy no muta ninguna petición.\n  \
             Niveles válidos: {}",
            NIVELES_DE_ESFUERZO.join(", ")
        )),
    )
}

pub struct AppConfig {
    pub local_port: u16,
    /// Interfaz donde bindea el proxy. Por defecto loopback: abrirse a la red
    /// es una decisión consciente vía `OXIDEGATE_HOST`, nunca el default.
    pub bind_host: IpAddr,
    /// Aviso pendiente de imprimir si `OXIDEGATE_HOST` traía basura. Se
    /// arrastra en vez de imprimirse aquí porque `load()` corre también en
    /// subcomandos que no bindean y no deben ensuciar su salida.
    pub bind_host_warning: Option<String>,
    pub target_openai_url: String,
    pub target_anthropic_url: String,
    /// Host raíz de Gemini (sin path). El path `/v1beta/models/...` lo preserva
    /// el proxy tal cual llega del cliente, así que aquí va solo el origen.
    pub target_gemini_url: String,
    /// Base de la Responses API de Codex (`chatgpt.com/backend-api/codex`,
    /// NO `api.openai.com`): es el backend que usa el cliente `pi` de Codex,
    /// autenticado con la sesión de ChatGPT en vez de una API key de OpenAI.
    /// Ruta local `/v1/codex/responses` la reenvía a `{target_codex_url}/responses`.
    pub target_codex_url: String,
    pub storage_dir: PathBuf,
    /// Palanca A del optimizador: fuerza un breakpoint de `cache_control` en
    /// las peticiones a Anthropic que no gestionan su propio prompt caching.
    ///
    /// OxideGate es ANTE TODO un medidor transparente: por defecto no muta
    /// ningún request. Este flag es la única excepción deliberada — activa una
    /// mutación real del body saliente (ver `provider/anthropic.rs`), por eso
    /// arranca APAGADO y hay que prenderlo a propósito con
    /// `OXIDEGATE_FORCE_CACHE=true`.
    pub force_prompt_cache: bool,
    /// Palanca B del optimizador: fuerza `output_config.effort` en las
    /// peticiones a Anthropic. `None` = apagada, que es el default.
    ///
    /// Medida: `--effort low` da **−20,0% de tokens de salida y −22,0% de
    /// reloj** sin coste de exactitud sobre razonamiento de respuesta cerrada
    /// (45/45 en 25 problemas). Lo que NO está medido son las tareas
    /// abiertas —código, diseño—, y por eso esto arranca apagado y se
    /// documenta con el hueco a la vista (ver `docs/optimizer-effort.md`).
    ///
    /// A diferencia de la palanca A, esta SOBRESCRIBE lo que el cliente pidió:
    /// medido, Claude Code manda `{"effort": "high"}` explícito en cada
    /// petición, así que una palanca que solo actuara ante su ausencia no
    /// haría nada nunca. Por eso la fila de telemetría publica las DOS cosas:
    /// `requested_effort` (lo que pidió el cliente, leído antes de mutar) y
    /// `effort_forced` (lo que impuso el proxy).
    pub force_effort: Option<String>,
    /// Aviso pendiente de imprimir si `OXIDEGATE_FORCE_EFFORT` traía basura.
    /// Se arrastra por el mismo motivo que [`Self::bind_host_warning`].
    pub force_effort_warning: Option<String>,
}

impl AppConfig {
    pub fn load() -> Self {
        // Buscamos la carpeta HOME del usuario para guardar nuestros propios datos de forma limpia
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let mut storage_dir = PathBuf::from(home);
        storage_dir.push(".config");
        storage_dir.push("oxidegate"); // Nuestra propia carpeta independiente

        let (bind_host, bind_host_warning) =
            parse_bind_host(env::var("OXIDEGATE_HOST").ok().as_deref());
        let (force_effort, force_effort_warning) =
            parse_force_effort(env::var("OXIDEGATE_FORCE_EFFORT").ok().as_deref());

        Self {
            local_port: env::var("OXIDEGATE_PORT")
                .unwrap_or_else(|_| "8080".to_string())
                .parse()
                .unwrap_or(8080),
            bind_host,
            bind_host_warning,
            target_openai_url: env::var("OPENAI_API_BASE")
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
            target_anthropic_url: env::var("ANTHROPIC_API_BASE")
                .unwrap_or_else(|_| "https://api.anthropic.com/v1".to_string()),
            // Solo el host: el cliente Gemini pega a `/v1beta/models/{model}:...`
            // y ese path se reenvía sin tocar.
            target_gemini_url: env::var("GEMINI_API_BASE")
                .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string()),
            target_codex_url: env::var("OXIDEGATE_CODEX_API_BASE")
                .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex".to_string()),
            storage_dir,
            force_prompt_cache: env::var("OXIDEGATE_FORCE_CACHE")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
            force_effort,
            force_effort_warning,
        }
    }

    // Método útil para que el optimizador sepa si existe el entorno de OpenCode en la máquina
    pub fn has_opencode_env(&self) -> bool {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let mut path = PathBuf::from(home);
        path.push(".config");
        path.push("opencode");
        path.exists()
    }
}

#[cfg(test)]
mod force_effort_tests {
    use super::*;

    /// El default es NO tocar nada. Es la promesa entera del proxy.
    #[test]
    fn sin_variable_la_palanca_esta_apagada() {
        let (objetivo, aviso) = parse_force_effort(None);

        assert!(objetivo.is_none());
        assert!(aviso.is_none());
    }

    /// Una variable vacía o en blanco es "no la he puesto", no un error.
    /// Mismo criterio que `OXIDEGATE_HOST=`.
    #[test]
    fn una_variable_vacia_no_enciende_ni_avisa() {
        for crudo in ["", "   ", "\t"] {
            let (objetivo, aviso) = parse_force_effort(Some(crudo));
            assert!(objetivo.is_none(), "{crudo:?} no debe encender la palanca");
            assert!(aviso.is_none(), "{crudo:?} no es un error, es una ausencia");
        }
    }

    /// Los cinco niveles del dialecto, y sin sensibilidad a mayúsculas ni a
    /// espacios: quien exporta una variable de entorno no debería pelearse
    /// con eso.
    #[test]
    fn reconoce_los_cinco_niveles_normalizados() {
        for (crudo, esperado) in [
            ("low", "low"),
            ("MEDIUM", "medium"),
            (" high ", "high"),
            ("XHigh", "xhigh"),
            ("max", "max"),
        ] {
            let (objetivo, aviso) = parse_force_effort(Some(crudo));
            assert_eq!(objetivo.as_deref(), Some(esperado), "crudo: {crudo:?}");
            assert!(aviso.is_none());
        }
    }

    /// **Falla CERRADO, y es la decisión que importa de esta función.** Un
    /// typo deja la palanca apagada en vez de mutar cada petición con un valor
    /// que el proveedor no entiende. El aviso nombra el valor y los válidos.
    #[test]
    fn un_valor_desconocido_apaga_la_palanca_y_avisa() {
        for basura in ["lowest", "bajo", "1", "true", "off"] {
            let (objetivo, aviso) = parse_force_effort(Some(basura));

            assert!(
                objetivo.is_none(),
                "{basura:?} NO puede encender la palanca: mutaría cada petición"
            );
            let aviso = aviso.expect("un valor desconocido tiene que avisar");
            assert!(
                aviso.contains(basura),
                "el aviso debe nombrar el valor: {aviso}"
            );
            assert!(
                aviso.contains("APAGADA"),
                "el aviso debe decir qué pasó: {aviso}"
            );
        }
    }
}

#[cfg(test)]
mod bind_host_tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn sin_variable_bindea_en_loopback() {
        let (host, aviso) = parse_bind_host(None);
        assert_eq!(host, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(aviso.is_none());
    }

    #[test]
    fn vacio_se_trata_como_ausente_y_no_avisa() {
        // `OXIDEGATE_HOST=` en un .env es "no lo he puesto", no un error.
        for raw in ["", "   "] {
            let (host, aviso) = parse_bind_host(Some(raw));
            assert_eq!(host, IpAddr::V4(Ipv4Addr::LOCALHOST));
            assert!(aviso.is_none(), "{raw:?} no debería avisar");
        }
    }

    #[test]
    fn todas_las_interfaces_se_acepta() {
        let (host, aviso) = parse_bind_host(Some("0.0.0.0"));
        assert_eq!(host, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert!(aviso.is_none());
    }

    #[test]
    fn acepta_una_ip_concreta_y_tambien_ipv6() {
        let (v4, _) = parse_bind_host(Some("192.168.1.50"));
        assert_eq!(v4, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)));

        let (v6, _) = parse_bind_host(Some("::"));
        assert_eq!(v6, IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    }

    #[test]
    fn localhost_es_un_alias_valido() {
        // La gente lo escribe. Aceptarlo evita un aviso que no ayuda a nadie.
        let (host, aviso) = parse_bind_host(Some("localhost"));
        assert_eq!(host, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(aviso.is_none());
    }

    #[test]
    fn un_valor_ilegible_falla_cerrado_y_avisa() {
        // LA invariante de seguridad de esta función: un typo NUNCA puede
        // acabar abriendo el proxy a la red. Vuelve a loopback y lo dice.
        let (host, aviso) = parse_bind_host(Some("0.0.0.O"));
        assert_eq!(host, IpAddr::V4(Ipv4Addr::LOCALHOST));
        let aviso = aviso.expect("un valor ilegible tiene que avisar");
        assert!(aviso.contains("0.0.0.O"), "el aviso debe citar el valor");
        assert!(aviso.contains("127.0.0.1"), "y decir dónde bindeó de verdad");
    }

    #[test]
    fn loopback_no_genera_aviso_de_exposicion() {
        assert!(exposure_warning(IpAddr::V4(Ipv4Addr::LOCALHOST)).is_none());
        assert!(exposure_warning(IpAddr::V6(Ipv6Addr::LOCALHOST)).is_none());
    }

    #[test]
    fn bindear_fuera_de_loopback_avisa_de_lo_que_queda_expuesto() {
        // /requests publica telemetría y `client` es contenido controlado por
        // el cliente (docs §4.5). Abrir el bind sin decirlo sería una trampa.
        for ip in [
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ] {
            let aviso = exposure_warning(ip).unwrap_or_else(|| panic!("{ip} debe avisar"));
            assert!(aviso.contains("/requests"), "{ip}: debe nombrar el endpoint");
        }
    }
}
