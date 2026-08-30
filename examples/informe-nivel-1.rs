//! El informe del nivel 1 de [#29](https://github.com/pichu2707/OxideGate/issues/29):
//! convierte las corridas de `corredor-nivel-1` en una tabla que se pueda
//! publicar sin mentir. Cierra
//! [#122](https://github.com/pichu2707/OxideGate/issues/122).
//!
//! # Lo que NO publica, y es lo primero que dice #29
//!
//! > Publicar una tabla de «herramienta X cuesta N veces más que Y» a partir de
//! > una ejecución por herramienta. Sería el tipo de cifra que suena medida y
//! > no lo está.
//!
//! Ni siquiera con `n` alto se publica esa frase a secas. Se publica **la
//! distribución**: mediana y rango, nunca solo la media. Dos harnesses con la
//! misma media y rangos distintos no cuestan lo mismo, y una media no lo dice.
//!
//! Y **`resueltas/n` va primero**, porque sin eso el coste no se puede
//! interpretar: una herramienta que resuelve 18 de 20 gastando el doble no es
//! peor que una que resuelve 9 de 20 gastando la mitad.
//!
//! # La columna que solo puede calcular OxideGate
//!
//! **`trabajo real` = bytes totales − peaje fijo.**
//!
//! Separar «lo que cuesta arrancar» de «lo que cuesta trabajar» es la
//! aportación de este proyecto a la pregunta de #29, y nadie que mire solo el
//! total está en posición de hacerla: una herramienta puede tener un peaje
//! enorme y ser eficiente trabajando, o al revés.
//!
//! ## Por qué el peaje NO se toma de `floor-across-tools.md`
//!
//! Aquella tabla mide «tal como está instalado aquí» — con las skills, el MCP y
//! la configuración del usuario. **El corredor corre con el `HOME` aislado**, y
//! ahí no existe nada de eso. Son instalaciones distintas, y el propio §4.2 lo
//! avisa: *«los totales no son comparables entre instalaciones»*.
//!
//! Medido el 2026-08-30, la diferencia no es un matiz:
//!
//! | | peaje publicado (§1) | peaje bajo el aislamiento del corredor |
//! |---|---:|---:|
//! | `opencode` | 117.125 B (v1.18.5) | **31.975 B** (v1.18.25) |
//! | `pi` | no está en la tabla | **5.932 B** |
//!
//! Restar el publicado daría `305.131 − 117.125 = 188.006`, un número que **no
//! parece absurdo** y está inventado. Por eso el peaje se mide con
//! `CORREDOR_MODO=peaje`: mismo aislamiento, misma config, mismo prompt trivial,
//! sin la tarea. La resta es válida **por construcción**, no por suposición.
//!
//! # La contaminación va en la tabla, no en un apéndice
//!
//! `optimizer-tool-search.md` §3 lo tiene medido con grupo de control:
//! enrutar Claude Code por OxideGate le hace **dejar de diferir sus esquemas
//! MCP**, porque `ANTHROPIC_BASE_URL` no es first-party. **El instrumento
//! produce el fenómeno**, así que *Claude Code medido a través del proxy no es
//! Claude Code*. Sale impreso con la tabla, no tres secciones más abajo.
//!
//! # Uso
//!
//! ```sh
//! # 1. El peaje y las corridas, por harness
//! CORREDOR_MODO=peaje CORREDOR_HARNESS=pi CORREDOR_N=5  cargo run --example corredor-nivel-1
//! CORREDOR_HARNESS=pi CORREDOR_N=30                     cargo run --example corredor-nivel-1
//!
//! # 2. El informe
//! cargo run --example informe-nivel-1
//! ```
//!
//! Variables: `CORREDOR_DATOS` (`./datos-corredor.jsonl`), la misma que escribe
//! el corredor.

use serde_json::Value;
use std::collections::BTreeMap;

/// Repeticiones mínimas para que una fila se publique.
///
/// Mismo umbral que [`N_MINIMO_PUBLICABLE`] de la sonda, y por el mismo motivo
/// medido: con `n=5` el rango se movía solo entre ejecuciones idénticas. Una
/// fila por debajo se imprime marcada como **INDICATIVA**, no se esconde —
/// esconderla sería decidir por quien lee.
const N_MINIMO_PUBLICABLE: usize = 30;

#[derive(Debug, Default)]
struct Muestra {
    version: String,
    modelo: String,
    /// Bytes mandados por repetición, TODAS.
    bytes: Vec<u64>,
    /// Bytes de las repeticiones que **resolvieron**.
    ///
    /// Van aparte porque son dos preguntas distintas y mezclarlas engaña: el
    /// rango de `bytes` incluye repeticiones que murieron en dos turnos sin
    /// tocar la tarea, y su mínimo se lee como «lo barato que sale trabajar»
    /// cuando es «lo barato que sale rendirse». Medido el 2026-08-30, el mínimo
    /// del trabajo real de opencode caía a 38 B por eso.
    ///
    /// Se publican las DOS: #121 exige que lo que no resuelve se cuente y se
    /// publique, y #122 pide el coste del trabajo. Filtrar una escondería la
    /// otra.
    bytes_ok: Vec<u64>,
    /// Turnos (peticiones) por repetición.
    turnos: Vec<u64>,
    resueltas: usize,
    /// Repeticiones que fallaron por el BANCO, no por el modelo. Se cuentan
    /// aparte: no pueden entrar en el denominador de una tasa de capacidad.
    del_banco: usize,
    total: usize,
}

/// Mediana y rango. **Nunca solo la media** — ver el doc del módulo.
fn mediana_y_rango(v: &[u64]) -> Option<(u64, u64, u64)> {
    if v.is_empty() {
        return None;
    }
    let mut o = v.to_vec();
    o.sort_unstable();
    Some((o[o.len() / 2], o[0], o[o.len() - 1]))
}

/// Lee el fichero de datos y agrupa por `(harness, modo)`.
fn agrupar(contenido: &str) -> BTreeMap<(String, String), Muestra> {
    let mut out: BTreeMap<(String, String), Muestra> = BTreeMap::new();
    for linea in contenido.lines() {
        let Ok(v) = serde_json::from_str::<Value>(linea) else {
            continue;
        };
        let harness = v["harness"].as_str().unwrap_or("?").to_string();
        let modo = v["modo"].as_str().unwrap_or("corrida").to_string();
        let m = out.entry((harness, modo)).or_default();
        m.version = v["version"].as_str().unwrap_or("?").to_string();
        m.modelo = v["modelo"].as_str().unwrap_or("?").to_string();
        if let Some(b) = v["bytes"].as_u64() {
            m.bytes.push(b);
        }
        if let Some(t) = v["peticiones"].as_u64() {
            m.turnos.push(t);
        }
        if v["resuelto"].as_bool() == Some(true) {
            m.resueltas += 1;
            if let Some(b) = v["bytes"].as_u64() {
                m.bytes_ok.push(b);
            }
        }
        if v["fallo_del_banco"].as_bool() == Some(true) {
            m.del_banco += 1;
        }
        m.total += 1;
    }
    out
}

/// El peaje de un harness: la **mediana** de su modo peaje.
///
/// `None` si ese harness no tiene peaje medido, y entonces la fila se publica
/// **sin** la columna de trabajo real. Rellenarla con el peaje de otra
/// instalación es exactamente lo que este informe no hace.
fn peaje_de(datos: &BTreeMap<(String, String), Muestra>, harness: &str) -> Option<u64> {
    let m = datos.get(&(harness.to_string(), "peaje".to_string()))?;
    mediana_y_rango(&m.bytes).map(|(med, _, _)| med)
}

/// ¿El rango de `a` queda ENTERO por debajo del de `b`?
///
/// Devuelve `(max_de_a, min_de_b)` cuando no hay solape.
///
/// Es la única forma honesta de decir «X cuesta menos que Y» con `n` finito.
/// Comparar medianas no basta: dos medianas distintas con rangos que se pisan
/// no distinguen nada, y esa es justo la cifra que #29 prohíbe publicar.
fn sin_solape(a: &[u64], b: &[u64]) -> Option<(u64, u64)> {
    let (_, _, max_a) = mediana_y_rango(a)?;
    let (_, min_b, _) = mediana_y_rango(b)?;
    (max_a < min_b).then_some((max_a, min_b))
}

fn fmt_rango(v: &[u64]) -> String {
    match mediana_y_rango(v) {
        Some((med, lo, hi)) if lo == hi => format!("{med}"),
        Some((med, lo, hi)) => format!("{med} ({lo}-{hi})"),
        None => "—".into(),
    }
}

fn main() {
    let ruta = std::env::var("CORREDOR_DATOS").unwrap_or_else(|_| "./datos-corredor.jsonl".into());
    let contenido = std::fs::read_to_string(&ruta).unwrap_or_default();
    if contenido.trim().is_empty() {
        eprintln!("ABORTA: no hay datos en {ruta}.");
        eprintln!("  Corre primero el corredor; el informe no inventa cifras.");
        std::process::exit(1);
    }

    let datos = agrupar(&contenido);
    let corridas: Vec<(&String, &Muestra)> = datos
        .iter()
        .filter(|((_, modo), _)| modo == "corrida")
        .map(|((h, _), m)| (h, m))
        .collect();

    if corridas.is_empty() {
        eprintln!("ABORTA: hay datos, pero ninguna CORRIDA (solo peajes).");
        std::process::exit(1);
    }

    println!("informe del nivel 1 de #29 — la misma tarea, harnesses distintos\n");

    println!(
        "{:<20} {:>10} {:>18} {:>12} {:>10} {:>16}",
        "harness", "resueltas", "bytes/rep", "turnos", "peaje", "trabajo real"
    );
    println!("{}", "─".repeat(92));

    for (harness, m) in &corridas {
        let peaje = peaje_de(&datos, harness);
        // El trabajo real se calcula por REPETICION y luego se resume, no al
        // reves: restar el peaje de la mediana daria la mediana de otra cosa.
        let trabajo: Vec<u64> = match peaje {
            Some(p) => m.bytes.iter().map(|b| b.saturating_sub(p)).collect(),
            None => Vec::new(),
        };
        println!(
            "{:<20} {:>10} {:>18} {:>12} {:>10} {:>16}",
            format!(
                "{harness} {}",
                m.version.split_whitespace().last().unwrap_or("")
            ),
            format!("{}/{}", m.resueltas, m.total),
            fmt_rango(&m.bytes),
            fmt_rango(&m.turnos),
            peaje.map_or("—".into(), |p| p.to_string()),
            if trabajo.is_empty() {
                "—".into()
            } else {
                fmt_rango(&trabajo)
            },
        );
    }

    println!("\nmediana (min-max). Un solo numero = el rango era cero.");

    // Y las MISMAS cifras sobre las repeticiones que SI resolvieron. Sin esta
    // segunda fila, el minimo de la de arriba se lee como «lo barato que sale
    // trabajar» cuando puede ser «lo barato que sale rendirse».
    println!("\nsolo las repeticiones que RESOLVIERON:\n");
    println!(
        "{:<20} {:>10} {:>18} {:>12} {:>10} {:>16}",
        "harness", "resueltas", "bytes/rep", "turnos", "peaje", "trabajo real"
    );
    println!("{}", "─".repeat(92));
    for (harness, m) in &corridas {
        let peaje = peaje_de(&datos, harness);
        let trabajo: Vec<u64> = match peaje {
            Some(p) => m.bytes_ok.iter().map(|b| b.saturating_sub(p)).collect(),
            None => Vec::new(),
        };
        println!(
            "{:<20} {:>10} {:>18} {:>12} {:>10} {:>16}",
            format!(
                "{harness} {}",
                m.version.split_whitespace().last().unwrap_or("")
            ),
            format!("{}/{}", m.resueltas, m.total),
            fmt_rango(&m.bytes_ok),
            "—",
            peaje.map_or("—".into(), |p| p.to_string()),
            if trabajo.is_empty() {
                "—".into()
            } else {
                fmt_rango(&trabajo)
            },
        );
    }
    println!();

    // ---- Cuando los rangos NO se solapan ----
    //
    // Es la unica forma honesta de decir «X cuesta mas que Y» con n finito.
    // Comparar medianas no basta: dos medianas distintas con rangos que se
    // pisan no distinguen nada, y esa es justo la cifra que #29 prohibe
    // publicar. Si el mas caro de uno es mas barato que el mas barato del otro,
    // no hay solape que interpretar.
    if corridas.len() == 2 {
        let trabajo_de = |m: &Muestra, h: &str| -> Vec<u64> {
            match peaje_de(&datos, h) {
                Some(p) => m.bytes_ok.iter().map(|b| b.saturating_sub(p)).collect(),
                None => Vec::new(),
            }
        };
        let (ha, ma) = corridas[0];
        let (hb, mb) = corridas[1];
        let ta = trabajo_de(ma, ha);
        let tb = trabajo_de(mb, hb);

        match (sin_solape(&ta, &tb), sin_solape(&tb, &ta)) {
            (Some((max_a, min_b)), _) => println!(
                "  RANGOS SIN SOLAPE: el trabajo real mas CARO de `{ha}` ({max_a}) es menor que\n\
                 \x20 el mas BARATO de `{hb}` ({min_b}), sobre las repeticiones que resolvieron\n\
                 \x20 (n={} y n={}). Eso es mas fuerte que comparar medianas: no hay solape que\n\
                 \x20 interpretar.\n",
                ma.resueltas, mb.resueltas
            ),
            (_, Some((max_b, min_a))) => println!(
                "  RANGOS SIN SOLAPE: el trabajo real mas CARO de `{hb}` ({max_b}) es menor que\n\
                 \x20 el mas BARATO de `{ha}` ({min_a}), sobre las repeticiones que resolvieron\n\
                 \x20 (n={} y n={}). Eso es mas fuerte que comparar medianas: no hay solape que\n\
                 \x20 interpretar.\n",
                mb.resueltas, ma.resueltas
            ),
            _ => println!(
                "  LOS RANGOS SE SOLAPAN: con estos datos NO se puede decir que uno cueste mas\n\
                 \x20 que el otro. Una mediana con rangos que se pisan es justo la cifra que #29\n\
                 \x20 prohibe publicar.\n"
            ),
        }
    }

    // ---- Los avisos, que van AQUI y no en un apendice ----

    for (harness, m) in &corridas {
        if m.total < N_MINIMO_PUBLICABLE {
            println!(
                "  INDICATIVA: `{harness}` tiene n={} < {N_MINIMO_PUBLICABLE}. Su rango se mueve\n\
                 \x20 solo entre ejecuciones identicas: no citarlo.",
                m.total
            );
        }
        if m.del_banco > 0 {
            println!(
                "  AVISO: {}/{} repeticiones de `{harness}` son fallos del BANCO, no del\n\
                 \x20 modelo. Su tasa no es una tasa de capacidad hasta que eso este en cero.",
                m.del_banco, m.total
            );
        }
        if peaje_de(&datos, harness).is_none() {
            println!(
                "  SIN PEAJE: `{harness}` no tiene peaje medido, asi que no se publica su\n\
                 \x20 trabajo real. Correr: CORREDOR_MODO=peaje CORREDOR_HARNESS={harness}\n\
                 \x20 Rellenarlo con el peaje de otra instalacion seria inventarlo."
            );
        }
    }

    println!(
        "\n  EL PEAJE ES EL DEL AISLAMIENTO DEL CORREDOR, no el de\n\
         \x20 floor-across-tools.md §1: aquel se mide con la config instalada del\n\
         \x20 usuario -skills, MCP- y aqui el HOME va aislado. Son instalaciones\n\
         \x20 distintas y sus totales NO se restan entre si (§4.2). Medido: el peaje\n\
         \x20 publicado de opencode es 3,7 veces el que paga dentro del corredor."
    );

    println!(
        "\n  CONTAMINACION DECLARADA: enrutar Claude Code por OxideGate le hace dejar\n\
         \x20 de diferir sus esquemas MCP, porque ANTHROPIC_BASE_URL no es\n\
         \x20 first-party (optimizer-tool-search.md §3, con grupo de control). El\n\
         \x20 instrumento PRODUCE el fenomeno: Claude Code medido a traves del proxy\n\
         \x20 no es Claude Code. No esta en esta tabla, y si algun dia entra, entra\n\
         \x20 con este aviso pegado."
    );

    println!(
        "\n  ESTO NO RECOMIENDA NADA. Aqui se publica el dato; elegir es de quien\n\
         \x20 paga (#122). Y los tokens NO son comparables entre proveedores:\n\
         \x20 se compara por BYTES MANDADOS, que es la variable controlada."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datos_de(lineas: &[&str]) -> BTreeMap<(String, String), Muestra> {
        agrupar(&lineas.join("\n"))
    }

    #[test]
    fn la_mediana_y_el_rango_describen_la_muestra() {
        assert_eq!(mediana_y_rango(&[5, 1, 9, 3, 7]), Some((5, 1, 9)));
        assert_eq!(mediana_y_rango(&[]), None);
    }

    /// El caso que justifica publicar el rango: misma media, muestras distintas.
    #[test]
    fn dos_muestras_con_la_misma_media_se_distinguen_por_el_rango() {
        assert_eq!(fmt_rango(&[50, 50, 50]), "50");
        assert_eq!(fmt_rango(&[10, 50, 90]), "50 (10-90)");
    }

    #[test]
    fn agrupa_por_harness_y_modo_sin_mezclarlos() {
        let d = datos_de(&[
            r#"{"harness":"pi","modo":"peaje","bytes":5932,"peticiones":1,"resuelto":true}"#,
            r#"{"harness":"pi","modo":"corrida","bytes":78954,"peticiones":8,"resuelto":true}"#,
            r#"{"harness":"opencode","modo":"corrida","bytes":305131,"peticiones":10,"resuelto":true}"#,
        ]);
        assert_eq!(
            d.len(),
            3,
            "peaje y corrida del mismo harness NO se mezclan"
        );
        assert_eq!(peaje_de(&d, "pi"), Some(5932));
    }

    /// Sin peaje medido NO se rellena con el de otra instalacion: se publica
    /// la fila sin esa columna. Es el fallo entero que bloqueaba #122.
    #[test]
    fn un_harness_sin_peaje_no_hereda_el_de_otro() {
        let d = datos_de(&[
            r#"{"harness":"pi","modo":"peaje","bytes":5932,"peticiones":1}"#,
            r#"{"harness":"opencode","modo":"corrida","bytes":305131,"peticiones":10}"#,
        ]);
        assert_eq!(peaje_de(&d, "opencode"), None, "no hereda el peaje de pi");
    }

    /// El trabajo real se calcula por repeticion y LUEGO se resume. Restar el
    /// peaje de la mediana da la mediana de otra cosa en cuanto la muestra no
    /// es simetrica.
    #[test]
    fn el_trabajo_real_se_resta_por_repeticion_no_sobre_la_mediana() {
        let bytes = [100u64, 100, 400];
        let peaje = 50u64;
        let por_rep: Vec<u64> = bytes.iter().map(|b| b - peaje).collect();
        assert_eq!(mediana_y_rango(&por_rep).unwrap(), (50, 50, 350));
        // Y el rango sobrevive: restar sobre la mediana lo habria aplanado.
        assert_ne!(mediana_y_rango(&por_rep).unwrap().2, 350 - 50);
    }

    /// Un peaje mayor que los bytes de una repeticion NO puede dar un numero
    /// negativo envuelto: `saturating_sub` deja 0, que se lee como «no hubo
    /// trabajo por encima del peaje».
    #[test]
    fn un_peaje_mayor_que_la_corrida_no_desborda() {
        assert_eq!(100u64.saturating_sub(500), 0);
    }

    /// El minimo del rango de TODAS puede venir de una repeticion que se
    /// rindio en dos turnos. Leerlo como «lo barato que sale trabajar» es el
    /// engano que la segunda tabla existe para evitar.
    #[test]
    fn las_resueltas_se_miden_aparte_de_las_que_se_rindieron() {
        let d = datos_de(&[
            r#"{"harness":"oc","modo":"corrida","bytes":40,"peticiones":2,"resuelto":false}"#,
            r#"{"harness":"oc","modo":"corrida","bytes":300,"peticiones":10,"resuelto":true}"#,
            r#"{"harness":"oc","modo":"corrida","bytes":320,"peticiones":11,"resuelto":true}"#,
        ]);
        let m = &d[&("oc".into(), "corrida".into())];
        assert_eq!(
            mediana_y_rango(&m.bytes).unwrap().1,
            40,
            "el minimo de TODAS"
        );
        assert_eq!(
            mediana_y_rango(&m.bytes_ok).unwrap().1,
            300,
            "el minimo de las que RESOLVIERON no lo arrastra"
        );
        assert_eq!(m.bytes_ok.len(), 2);
    }

    #[test]
    fn sin_solape_solo_afirma_cuando_los_rangos_no_se_pisan() {
        // El caso real: el mas caro de pi (150450) < el mas barato de opencode.
        assert_eq!(sin_solape(&[100, 150], &[160, 300]), Some((150, 160)));
        // Se pisan por un solo byte: no se afirma nada.
        assert_eq!(sin_solape(&[100, 160], &[160, 300]), None);
        // Y no es simetrico: hay que preguntar en los dos sentidos.
        assert_eq!(sin_solape(&[160, 300], &[100, 150]), None);
        // Una muestra vacia no permite concluir.
        assert_eq!(sin_solape(&[], &[1, 2]), None);
    }

    /// Dos medianas MUY distintas con rangos que se pisan no distinguen nada.
    /// Es la cifra que #29 prohibe publicar, y este test la deja marcada.
    #[test]
    fn medianas_distintas_con_rangos_que_se_pisan_no_afirman_nada() {
        let a = [10u64, 50, 200];
        let b = [40u64, 150, 300];
        assert_ne!(
            mediana_y_rango(&a).unwrap().0,
            mediana_y_rango(&b).unwrap().0
        );
        assert_eq!(sin_solape(&a, &b), None);
        assert_eq!(sin_solape(&b, &a), None);
    }

    #[test]
    fn los_fallos_del_banco_se_cuentan_aparte() {
        let d = datos_de(&[
            r#"{"harness":"pi","modo":"corrida","bytes":1,"peticiones":1,"resuelto":false,"fallo_del_banco":true}"#,
            r#"{"harness":"pi","modo":"corrida","bytes":2,"peticiones":1,"resuelto":true}"#,
        ]);
        let m = &d[&("pi".into(), "corrida".into())];
        assert_eq!(m.total, 2);
        assert_eq!(m.resueltas, 1);
        assert_eq!(m.del_banco, 1, "no se colapsa con «no resuelto»");
    }

    #[test]
    fn una_linea_corrupta_no_tumba_el_informe() {
        let d = datos_de(&[
            "no soy json",
            r#"{"harness":"pi","modo":"corrida","bytes":10,"peticiones":1}"#,
        ]);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn el_umbral_publicable_es_el_mismo_que_el_de_la_sonda() {
        // La sonda lo fijo en 30 midiendo, no eligiendo: con n=5 el rango se
        // movia solo entre ejecuciones identicas.
        assert_eq!(N_MINIMO_PUBLICABLE, 30);
    }
}
