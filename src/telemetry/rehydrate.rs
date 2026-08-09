//! Rehidratación del histórico: releer `telemetry.jsonl` al arrancar.
//!
//! El fichero se escribe desde siempre y hasta ahora nadie lo leía. Los tres
//! consumidores viven en RAM y arrancaban vacíos, así que reiniciar el proxy
//! —tras un `brew upgrade`, tras cerrar el portátil— borraba la única pregunta
//! que el usuario quiere contestar de verdad: *¿estoy gastando más o menos que
//! la semana pasada?*
//!
//! Este módulo lee el fichero una vez, al arrancar, y reconstruye
//! [`StatsRegistry`] y [`SessionRegistry`] antes de aceptar la primera
//! petición.
//!
//! # Qué NO rehidrata, y por qué
//!
//! **`GET /requests` se queda vacío al arrancar, a propósito.** Su contrato es
//! «las últimas [`RECENT_CAPACITY`](super::recent::RECENT_CAPACITY) EN VIVO»;
//! mezclar ahí filas de hace una semana lo rompería. Un agregado sí tolera
//! histórico —esa es su naturaleza—, una ventana de las últimas N no.
//!
//! Por la misma razón `RequestMetric::cache_by_section` no se deserializa
//! nunca: es un campo exclusivo de `/requests`.
//!
//! # La ventana, y por qué está acotada
//!
//! Un `.jsonl` de meses no se puede releer entero en cada arranque sin retrasar
//! el primer request. Se lee solo lo que cae dentro de
//! [`HISTORY_DAYS_DEFAULT`] días, configurable con `OXIDEGATE_HISTORY_DAYS`, y
//! `0` desactiva la rehidratación por completo.
//!
//! # Tolerancia: una fila corrupta no puede tumbar el arranque
//!
//! El fichero es append-only y puede tener una última línea truncada (un corte
//! de luz a mitad de escritura), o filas de una build antigua. Ninguna de las
//! dos debe impedir arrancar: se cuentan y se siguen. El resultado
//! ([`Rehydrated`]) dice cuántas se saltaron, para que el hueco sea visible en
//! vez de silencioso.

use std::path::Path;

use crate::telemetry::logger::RequestMetric;

/// Solo lo necesario para situar una fila en el tiempo.
///
/// Permite decidir la ventana sin pagar el parseo completo de una fila que se
/// va a descartar, y —más importante— sin contar como «ilegible» una fila
/// antigua que la ventana iba a saltar igualmente.
#[derive(serde::Deserialize)]
struct SoloTimestamp {
    timestamp: String,
}
use crate::telemetry::mcp::McpRegistry;
use crate::telemetry::stats::{SessionRegistry, StatsRegistry};

/// Días de histórico que se releen si `OXIDEGATE_HISTORY_DAYS` no dice otra
/// cosa.
///
/// Siete es una semana: el marco natural de la pregunta que este módulo existe
/// para contestar («¿gasto más que la semana pasada?»), y suficientemente corto
/// para que releerlo no se note en el arranque.
pub const HISTORY_DAYS_DEFAULT: u32 = 7;

/// Resultado de una rehidratación, para poder declararlo en vez de suponerlo.
///
/// Los tres contadores se publican: un agregado que no dice de dónde sale es un
/// número sin unidades, y saber que se descartaron filas importa tanto como
/// saber cuántas se leyeron.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Rehydrated {
    /// Filas incorporadas a los agregados.
    pub rows: usize,
    /// Filas que quedaron FUERA de la ventana temporal. No son un error: son
    /// la ventana haciendo su trabajo.
    pub skipped_old: usize,
    /// Filas que no se pudieron parsear. **Un valor distinto de cero merece
    /// mirarse**: o el fichero tiene una línea truncada al final (normal tras
    /// un corte), o alguien cambió el formato sin migrar.
    pub skipped_bad: usize,
    /// Timestamp de la fila más antigua incorporada (RFC 3339), o `None` si no
    /// se incorporó ninguna. Es lo que convierte el agregado en una medición
    /// con unidades: **desde cuándo** mide.
    pub oldest: Option<String>,
}

/// Lee `telemetry.jsonl` y reconstruye los dos agregados.
///
/// Los registros se pasan por `&mut` en vez de devolverse construidos para que
/// el llamador decida su compartición (hoy `Arc<RwLock<_>>`): este módulo no
/// sabe nada de locks, igual que el resto de `telemetry`.
///
/// `days == 0` desactiva la rehidratación y devuelve un [`Rehydrated`] en cero
/// sin tocar el disco. No es un caso de error: es la opción de quien quiere el
/// comportamiento anterior.
///
/// Que el fichero no exista tampoco es un error —es un primer arranque— y se
/// devuelve el mismo cero.
pub fn rehydrate(
    path: &Path,
    days: u32,
    now: chrono::DateTime<chrono::Utc>,
    stats: &mut StatsRegistry,
    sessions: &mut SessionRegistry,
    mcp: &mut McpRegistry,
) -> Rehydrated {
    let mut out = Rehydrated::default();
    if days == 0 {
        return out;
    }
    let Ok(contenido) = std::fs::read_to_string(path) else {
        return out;
    };
    let corte = now - chrono::Duration::days(days as i64);

    for linea in contenido.lines() {
        if linea.trim().is_empty() {
            continue;
        }
        // LA VENTANA SE COMPRUEBA ANTES DEL PARSEO COMPLETO, y el orden no es
        // una optimización: es lo que hace que `skipped_bad` signifique algo.
        //
        // Al revés, una fila de hace seis meses escrita por una build antigua
        // se contaba como ilegible aunque la ventana la fuera a descartar de
        // todos modos, y el contador se llenaba de ruido —medido: 2.387 de
        // 2.960— hasta tapar las filas ilegibles que sí importan, las que
        // caen dentro de la ventana. Un aviso que siempre grita no avisa.
        let Ok(cab) = serde_json::from_str::<SoloTimestamp>(linea) else {
            out.skipped_bad += 1;
            continue;
        };
        let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&cab.timestamp) else {
            out.skipped_bad += 1;
            continue;
        };
        if ts.with_timezone(&chrono::Utc) < corte {
            out.skipped_old += 1;
            continue;
        }
        let Ok(m) = serde_json::from_str::<RequestMetric>(linea) else {
            out.skipped_bad += 1;
            continue;
        };

        if out
            .oldest
            .as_deref()
            .is_none_or(|viejo| m.timestamp.as_str() < viejo)
        {
            out.oldest = Some(m.timestamp.clone());
        }
        stats.ingest(&m);
        sessions.ingest(&m);
        // El cruce MCP se rehidrata como los otros dos agregados, y por un
        // motivo mas fuerte: exige 50 peticiones concluyentes antes de opinar,
        // asi que sin histórico arrancaria en blanco tras cada reinicio y no
        // llegaria nunca al umbral en un uso normal.
        mcp.ingest(&m);
        out.rows += 1;
    }
    out
}

/// Días de ventana pedidos por entorno, con el defecto de
/// [`HISTORY_DAYS_DEFAULT`].
///
/// Un valor no numérico NO cae al defecto en silencio: se devuelve junto a un
/// aviso para que el llamador lo registre. Tragarse una variable mal escrita y
/// comportarse distinto de lo que el usuario pidió es el tipo de fallo
/// silencioso que este proyecto persigue en el resto del código.
pub fn history_days_from_env(raw: Option<&str>) -> (u32, Option<String>) {
    match raw {
        None => (HISTORY_DAYS_DEFAULT, None),
        Some(v) => match v.trim().parse::<u32>() {
            Ok(d) => (d, None),
            Err(_) => (
                HISTORY_DAYS_DEFAULT,
                Some(format!(
                    "OXIDEGATE_HISTORY_DAYS='{v}' no es un número: se usan \
                     {HISTORY_DAYS_DEFAULT} días"
                )),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fila(ts: &str, upstream: &str, model: &str, input: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","route":"/v1/messages","upstream":"{upstream}",
               "model":"{model}","prompt_hash":"h","stream":true,"status":200,
               "total_ms":100.0,"input_tokens":{input}}}"#
        )
        .replace('\n', "")
    }

    /// Fichero temporal propio, con el mismo patrón que ya usa
    /// `telemetry::metered`: este crate no tiene ninguna dev-dependency y no
    /// merece la pena estrenar una por seis tests. El nombre lleva pid y un
    /// contador para que dos tests en paralelo no se pisen.
    struct Temporal(std::path::PathBuf);

    impl Temporal {
        fn nuevo(lineas: &[String]) -> Temporal {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "oxi-rehydrate-{}-{}.jsonl",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let mut f = std::fs::File::create(&p).expect("crear");
            for l in lineas {
                writeln!(f, "{l}").expect("escribe");
            }
            f.flush().expect("flush");
            Temporal(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Temporal {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn escribe(lineas: &[String]) -> Temporal {
        Temporal::nuevo(lineas)
    }

    fn ahora() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-07-31T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    /// El caso central: filas dentro de la ventana entran en los dos agregados.
    #[test]
    fn rehidrata_las_filas_dentro_de_la_ventana() {
        let f = escribe(&[
            fila("2026-07-30T10:00:00Z", "anthropic", "claude-opus-4-8", 100),
            fila("2026-07-31T10:00:00Z", "anthropic", "claude-opus-4-8", 200),
        ]);
        let (mut s, mut ses, mut mcp) = (
            StatsRegistry::default(),
            SessionRegistry::default(),
            McpRegistry::default(),
        );

        let r = rehydrate(f.path(), 7, ahora(), &mut s, &mut ses, &mut mcp);

        assert_eq!(r.rows, 2);
        assert_eq!(r.skipped_old, 0);
        assert_eq!(r.skipped_bad, 0);
        assert_eq!(r.oldest.as_deref(), Some("2026-07-30T10:00:00Z"));
        assert_eq!(s.snapshot(None).0.len(), 1, "un solo (upstream, model)");
    }

    /// Lo que queda fuera de la ventana se cuenta como VIEJO, no como error:
    /// son dos huecos distintos y colapsarlos escondería un fichero corrupto
    /// detrás de una ventana corta.
    #[test]
    fn lo_anterior_a_la_ventana_se_cuenta_aparte_de_lo_corrupto() {
        let f = escribe(&[
            fila("2026-01-01T10:00:00Z", "anthropic", "m", 1),
            fila("2026-07-31T10:00:00Z", "anthropic", "m", 2),
            "{esto no es json".to_string(),
        ]);
        let (mut s, mut ses, mut mcp) = (
            StatsRegistry::default(),
            SessionRegistry::default(),
            McpRegistry::default(),
        );

        let r = rehydrate(f.path(), 7, ahora(), &mut s, &mut ses, &mut mcp);

        assert_eq!(r.rows, 1);
        assert_eq!(r.skipped_old, 1);
        assert_eq!(r.skipped_bad, 1);
    }

    /// Una línea truncada al final —lo que deja un corte a mitad de escritura—
    /// no puede impedir arrancar: se cuenta y se sigue.
    #[test]
    fn una_linea_truncada_al_final_no_tumba_el_arranque() {
        let f = escribe(&[
            fila("2026-07-31T10:00:00Z", "anthropic", "m", 1),
            r#"{"timestamp":"2026-07-31T11:00:00Z","route":"/v1/mes"#.to_string(),
        ]);
        let (mut s, mut ses, mut mcp) = (
            StatsRegistry::default(),
            SessionRegistry::default(),
            McpRegistry::default(),
        );

        let r = rehydrate(f.path(), 7, ahora(), &mut s, &mut ses, &mut mcp);

        assert_eq!(r.rows, 1);
        assert_eq!(r.skipped_bad, 1);
    }

    /// Una fila de una build ANTIGUA (sin los campos que se añadieron después)
    /// tiene que entrar igual. Es el caso que motiva los `serde(default)` por
    /// campo de `RequestMetric`.
    #[test]
    fn una_fila_sin_los_campos_nuevos_se_rehidrata_igual() {
        let f = escribe(&[r#"{"timestamp":"2026-07-31T10:00:00Z","route":"/v1/messages","upstream":"anthropic","prompt_hash":"h","stream":false,"status":200,"total_ms":50.0}"#
            .to_string()]);
        let (mut s, mut ses, mut mcp) = (
            StatsRegistry::default(),
            SessionRegistry::default(),
            McpRegistry::default(),
        );

        let r = rehydrate(f.path(), 7, ahora(), &mut s, &mut ses, &mut mcp);

        assert_eq!(r.rows, 1, "una fila vieja no puede quedarse fuera");
        assert_eq!(r.skipped_bad, 0);
    }

    /// REGRESIÓN MEDIDA. Una fila antigua con `tools_by_server` pero SIN
    /// `tool_names` dentro tiene que entrar igual.
    ///
    /// El `serde(default)` de `RequestMetric` no basta: la tolerancia hay que
    /// declararla también en los campos INTERNOS de los tipos anidados. Sin
    /// esto, 2.387 de 2.960 filas reales fallaban con
    /// `missing field 'tool_names'` — el 80% del histórico, y justo el campo
    /// que el issue #42 nombraba como ejemplo.
    #[test]
    fn una_fila_con_tools_by_server_sin_tool_names_se_rehidrata_igual() {
        let f = escribe(&[r#"{"timestamp":"2026-07-31T10:00:00Z","route":"/v1/messages","upstream":"anthropic","prompt_hash":"h","stream":true,"status":200,"total_ms":10.0,"tools_by_server":[{"server":"(native)","kind":"native","tools":3,"bytes":100}]}"#.to_string()]);
        let (mut s, mut ses, mut mcp) = (
            StatsRegistry::default(),
            SessionRegistry::default(),
            McpRegistry::default(),
        );

        let r = rehydrate(f.path(), 7, ahora(), &mut s, &mut ses, &mut mcp);

        assert_eq!(r.rows, 1, "sin tool_names ni deferred_tools debe entrar");
        assert_eq!(r.skipped_bad, 0);
    }

    /// Una fila ANTIGUA e ilegible no puede contarse como ilegible: la ventana
    /// la iba a descartar de todos modos, y contarla llena el aviso de ruido
    /// hasta tapar las que sí importan.
    #[test]
    fn una_fila_vieja_e_ilegible_cuenta_como_vieja_y_no_como_corrupta() {
        let f = escribe(&[
            // vieja Y rota: fuera de ventana, no debe alarmar
            r#"{"timestamp":"2026-01-01T10:00:00Z","route":"/v1/m","upstream":"a"}"#.to_string(),
            fila("2026-07-31T10:00:00Z", "anthropic", "m", 1),
        ]);
        let (mut s, mut ses, mut mcp) = (
            StatsRegistry::default(),
            SessionRegistry::default(),
            McpRegistry::default(),
        );

        let r = rehydrate(f.path(), 7, ahora(), &mut s, &mut ses, &mut mcp);

        assert_eq!(r.rows, 1);
        assert_eq!(r.skipped_old, 1, "la vieja cuenta como vieja");
        assert_eq!(r.skipped_bad, 0, "y NO como corrupta");
    }

    /// `days = 0` es la opción explícita de «no rehidrates»: ni siquiera abre
    /// el fichero.
    #[test]
    fn cero_dias_desactiva_la_rehidratacion() {
        let f = escribe(&[fila("2026-07-31T10:00:00Z", "anthropic", "m", 1)]);
        let (mut s, mut ses, mut mcp) = (
            StatsRegistry::default(),
            SessionRegistry::default(),
            McpRegistry::default(),
        );

        let r = rehydrate(f.path(), 0, ahora(), &mut s, &mut ses, &mut mcp);

        assert_eq!(r, Rehydrated::default());
        assert!(s.snapshot(None).0.is_empty());
    }

    /// Un primer arranque no tiene fichero, y eso no es un error.
    #[test]
    fn sin_fichero_no_es_un_error() {
        let (mut s, mut ses, mut mcp) = (
            StatsRegistry::default(),
            SessionRegistry::default(),
            McpRegistry::default(),
        );

        let r = rehydrate(
            Path::new("/no/existe/telemetry.jsonl"),
            7,
            ahora(),
            &mut s,
            &mut ses,
            &mut mcp,
        );

        assert_eq!(r, Rehydrated::default());
    }

    /// Una variable mal escrita NO cae al defecto en silencio: devuelve aviso.
    #[test]
    fn una_ventana_no_numerica_avisa_en_vez_de_caer_al_defecto_en_silencio() {
        assert_eq!(history_days_from_env(None), (HISTORY_DAYS_DEFAULT, None));
        assert_eq!(history_days_from_env(Some("30")), (30, None));
        assert_eq!(history_days_from_env(Some("0")), (0, None));

        let (dias, aviso) = history_days_from_env(Some("siete"));
        assert_eq!(dias, HISTORY_DAYS_DEFAULT);
        assert!(aviso.expect("debe avisar").contains("siete"));
    }
}
