//! Deriva el modelo del nivel 1: el mismo modelo, con el razonamiento apagado
//! **dentro del modelo** y no en la petición.
//!
//! # El problema que resuelve, y por qué no vale la vía fácil
//!
//! [#121](https://github.com/pichu2707/OxideGate/issues/121) —el corredor del
//! nivel 1— necesita un modelo local capaz de conducir un harness. `qwen3:14b`
//! emite llamadas perfectamente (30/30) pero **entrega `content` vacío la mayor
//! parte de las veces**: deja el resultado leído en `thinking`. Un harness
//! consume `content`, así que recibe nada.
//!
//! Con el razonamiento apagado eso desaparece: entrega 30/30, y de paso va tres
//! veces más rápido.
//!
//! **La tabla canónica y su procedencia viven en
//! [`docs/modelo-del-nivel-1.md`](../docs/modelo-del-nivel-1.md)**, y este fichero
//! no la repite: una cifra en dos sitios es una cifra que se desincroniza, y las
//! dos copias que había aquí ya diferían entre sí antes de borrar una.
//!
//! La fe de erratas sí lleva las suyas, y debe llevarlas: su regla 2 exige que
//! una corrección venga con la medición. Lo que no debe haber es una tabla
//! *descriptiva* duplicada, que es lo que se quitó de aquí.
//!
//! Ese documento argumenta además **por qué el razonamiento se apaga en el
//! modelo** y no en la petición ni en la config de cada harness: las dos
//! alternativas devuelven al experimento un confundidor que el nivel 1 existe
//! para quitar.
//!
//! Lo que le toca a este fichero es el CÓMO. `PARAMETER think false` **no
//! existe** en el Modelfile de ollama 0.30.10 (`Error: unknown parameter
//! 'think'`), así que se hace parcheando la plantilla.
//!
//! # Las tres ediciones, y por qué aborta si no las encuentra
//!
//! La plantilla de `qwen3` decide el razonamiento con `$.IsThinkSet` y
//! `$.Think`, dos variables que rellena la PETICIÓN. Las ediciones las sacan de
//! la ecuación:
//!
//! 1. `/no_think` incondicional pegado al último mensaje de usuario.
//! 2. El prefill `<think></think>` incondicional en el turno del asistente.
//! 3. El bloque que **reinyecta el `thinking` de turnos anteriores**, apagado.
//!
//! La tercera se dejó fuera del primer intento razonando que «con el
//! razonamiento apagado, `.Thinking` viene vacío y esa rama no llega a
//! renderizar». Es cierto por el camino normal, y aun así **la decisión seguía
//! en manos de la petición**: bastaba con mandar `think` para reencenderla en un
//! modelo que se declara sin razonamiento. Un derivado que se llama `-nothink`
//! no puede tener un `if` que dependa de lo que le manden.
//!
//! **No se parchea por número de línea, se parchea por anclas**, y si un ancla
//! no aparece exactamente una vez, esto **aborta sin crear nada**. Una plantilla
//! que cambió bajo los pies —otra versión de ollama, otro modelo base— tiene que
//! detener la derivación, no producir un modelo silenciosamente distinto del que
//! se midió. Mismo criterio que las guardas de `calibrar.rs` y de
//! `sonda-herramientas.rs`: un instrumento que no suspende lo que sabe que está
//! mal no mide nada.
//!
//! # Uso
//!
//! ```sh
//! cargo run --example derivar-nothink
//! NOTHINK_BASE=qwen3:14b NOTHINK_DESTINO=qwen3:14b-nothink \
//!   cargo run --example derivar-nothink
//! ```
//!
//! Variables:
//!   NOTHINK_BASE     tag del modelo base (default `qwen3:14b`)
//!   NOTHINK_DESTINO  tag del derivado (default `<base>-nothink`)
//!
//! Comprobar el resultado es trabajo de la sonda, no de esto:
//!
//! ```sh
//! SONDA_MODELOS=qwen3:14b-nothink SONDA_N=30 cargo run --example sonda-herramientas
//! ```

use std::process::Command;

/// El `if` que envuelve al `/think` — `/no_think` en el último mensaje de
/// usuario. Se sustituye entero: lo que decidía la petición pasa a ser fijo.
const ANCLA_USUARIO: &str = r#"{{- if and $.IsThinkSet (eq $i $lastUserIdx) }}
   {{- if $.Think -}}
      {{- " "}}/think
   {{- else -}}
      {{- " "}}/no_think
   {{- end -}}
{{- end }}<|im_end|>"#;

/// Lo que ocupa su lugar.
const PARCHE_USUARIO: &str =
    r#"{{- if eq $i $lastUserIdx }}{{- " "}}/no_think{{- end }}<|im_end|>"#;

/// La condición del prefill `<think></think>` en el turno del asistente.
const ANCLA_PREFILL: &str = "{{ if and $.IsThinkSet (not $.Think) -}}";

/// Lo que ocupa su lugar: siempre.
const PARCHE_PREFILL: &str = "{{ if true -}}";

/// La condición que devuelve al prompt el `thinking` de turnos anteriores.
const ANCLA_REINYECCION: &str =
    "{{ if (and $.IsThinkSet (and .Thinking (or $last (gt $i $lastUserIdx)))) -}}";

/// Lo que ocupa su lugar: nunca. Un modelo sin razonamiento no tiene
/// razonamiento previo que reinyectar.
const PARCHE_REINYECCION: &str = "{{ if false -}}";

/// Aplica las tres ediciones, o dice cuál falta.
///
/// Es una función pura sobre la plantilla a propósito: es la parte que puede
/// romperse en silencio, y así se prueba sin ollama delante.
///
/// Exige que cada ancla aparezca **exactamente una vez**. Cero significa que la
/// plantilla cambió; más de una, que el ancla dejó de identificar un sitio
/// concreto. Las dos son motivo de abortar: en los dos casos el modelo derivado
/// no sería el que se midió.
fn parchear(plantilla: &str) -> Result<String, String> {
    for (nombre, ancla) in [
        ("del usuario", ANCLA_USUARIO),
        ("del prefill", ANCLA_PREFILL),
        ("de la reinyección", ANCLA_REINYECCION),
    ] {
        match plantilla.matches(ancla).count() {
            1 => {}
            0 => {
                return Err(format!(
                    "el ancla {nombre} no aparece en la plantilla: cambió bajo los pies"
                ));
            }
            n => {
                return Err(format!(
                    "el ancla {nombre} aparece {n} veces, y debe aparecer 1"
                ));
            }
        }
    }
    Ok(plantilla
        .replace(ANCLA_USUARIO, PARCHE_USUARIO)
        .replace(ANCLA_PREFILL, PARCHE_PREFILL)
        .replace(ANCLA_REINYECCION, PARCHE_REINYECCION))
}

/// La ventana de contexto del derivado, en tokens.
///
/// **No es un ajuste de rendimiento: es una condición para que el nivel 1 mida
/// algo.** `qwen3:14b` declara 40960 de contexto, pero un modelo sin
/// `PARAMETER num_ctx` recibe el defecto de ollama —4096 en 0.30.10— y el
/// servidor **corta el prompt en silencio**: la petición sale `200` y el modelo
/// contesta a lo que le quedó.
///
/// El prompt real de un harness no cabe ahí ni de lejos. Codex manda ~6500
/// tokens (system + 20 KB de declaraciones de herramientas + el encargo), y
/// medido el 2026-08-30 llegaban **4095**: se tiraba el 37%, herramientas
/// incluidas. La primera corrida del corredor dio 0/3 por esto, y ese cero se
/// habría leído como «el modelo no sabe conducir un harness».
///
/// # Por qué en el modelo y no fuera
///
/// Mismo argumento que el razonamiento apagado, y por los mismos tres motivos
/// (ver §3 de `docs/modelo-del-nivel-1.md`):
///
/// | dónde | por qué NO |
/// |---|---|
/// | en la petición | un harness no manda `num_ctx`; inyectarlo desde OxideGate mete al instrumento dentro del experimento |
/// | en el servidor (`OLLAMA_CONTEXT_LENGTH`) | depende de cómo arranque ollama cada quien: un confundidor que viaja sin declarar |
/// | **en el modelo** | constante para los cuatro harnesses, se declara en el informe, y ninguno de los cuatro sabe que existe |
const NUM_CTX: usize = 32_768;

/// El Modelfile del derivado. `FROM` hereda pesos y parámetros del base; lo que
/// cambia es la plantilla y la ventana de contexto ([`NUM_CTX`]).
///
/// Aborta si la plantilla lleva `"""`, que cerraría el literal antes de tiempo y
/// produciría un Modelfile que dice algo distinto de lo que se pretendía.
fn modelfile(base: &str, plantilla: &str) -> Result<String, String> {
    if plantilla.contains(r#"""""#) {
        return Err("la plantilla contiene `\"\"\"` y rompería el literal del Modelfile".into());
    }
    Ok(format!(
        "FROM {base}\nPARAMETER num_ctx {NUM_CTX}\nTEMPLATE \"\"\"{plantilla}\"\"\"\n"
    ))
}

/// El tag del derivado cuando nadie lo dice. `qwen3:14b` → `qwen3:14b-nothink`.
fn destino_por_defecto(base: &str) -> String {
    format!("{base}-nothink")
}

fn var(nombre: &str, defecto: &str) -> String {
    std::env::var(nombre).unwrap_or_else(|_| defecto.to_string())
}

fn ollama(args: &[&str]) -> Result<String, String> {
    let salida = Command::new("ollama")
        .args(args)
        .output()
        .map_err(|e| format!("no se pudo ejecutar `ollama {}`: {e}", args.join(" ")))?;
    if !salida.status.success() {
        return Err(format!(
            "`ollama {}` falló: {}",
            args.join(" "),
            String::from_utf8_lossy(&salida.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&salida.stdout).into_owned())
}

fn derivar() -> Result<String, String> {
    let base = var("NOTHINK_BASE", "qwen3:14b");
    let destino = var("NOTHINK_DESTINO", &destino_por_defecto(&base));

    println!("base    : {base}");
    println!("destino : {destino}");

    let plantilla = ollama(&["show", "--template", &base])?;
    let parcheada = parchear(&plantilla)?;
    println!(
        "plantilla parcheada: 3 ediciones sobre {} bytes",
        plantilla.len()
    );

    // `create_new` en vez de `write`: en un host compartido, un nombre
    // predecible en el temporal es un symlink plantado esperando a que alguien
    // lo siga. Y el fichero se borra TAMBIÉN si `ollama create` falla — si no,
    // el mensaje de aborto dice «no se ha creado nada» mientras deja un
    // Modelfile suelto contando otra cosa.
    let ruta = std::env::temp_dir().join(format!(
        "Modelfile.{}.{}",
        destino.replace(':', "_"),
        std::process::id()
    ));
    let contenido = modelfile(&base, &parcheada)?;
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&ruta)
            .map_err(|e| format!("no se pudo crear {}: {e}", ruta.display()))?;
        f.write_all(contenido.as_bytes())
            .map_err(|e| format!("no se pudo escribir {}: {e}", ruta.display()))?;
    }

    let creado = ollama(&["create", &destino, "-f", &ruta.to_string_lossy()]);
    let _ = std::fs::remove_file(&ruta);
    creado?;
    Ok(destino)
}

fn main() {
    match derivar() {
        Ok(destino) => {
            println!("\ncreado: {destino}");
            println!("comprobarlo NO es trabajo de esto — la sonda es quien dictamina:");
            println!("  SONDA_MODELOS={destino} SONDA_N=30 cargo run --example sonda-herramientas");
        }
        Err(e) => {
            eprintln!("\nABORTADO: {e}");
            eprintln!("No se ha creado ningún modelo. Un derivado que no sea el que se midió");
            eprintln!("es peor que no tener derivado: mide otra cosa y no lo dice.");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La plantilla real de `qwen3:14b` en ollama 0.30.10, **tal cual la
    /// devuelve `ollama show --template`**.
    ///
    /// # Por qué es un fichero y no un `format!` con las anclas dentro
    ///
    /// Porque la versión anterior se construía A PARTIR de las propias anclas, y
    /// eso hacía los tests **incapaces de fallar**: comprobaban que el parche
    /// quitaba las anclas de un texto fabricado con esas anclas y nada más.
    ///
    /// La plantilla real tenía **tres** `$.IsThinkSet` y el fixture reproducía
    /// dos, así que el test que afirma «no queda ninguna decisión en manos de la
    /// petición» pasaba mientras el modelo derivado seguía teniendo una. Un
    /// fixture escrito a mano por quien escribió el parche no prueba nada sobre
    /// el mundo: prueba que el autor es coherente consigo mismo.
    ///
    /// Si ollama cambia la plantilla, este fichero deja de coincidir con la
    /// realidad y hay que **volver a capturarlo y volver a medir** — que es
    /// exactamente lo que la guarda de `parchear` quiere forzar.
    fn plantilla_real() -> String {
        include_str!("fixtures/plantilla-qwen3-14b.txt").to_string()
    }

    /// El fixture es la plantilla de verdad, no un resumen: si esto falla, se
    /// recapturó mal o se editó a mano.
    #[test]
    fn el_fixture_es_la_plantilla_completa() {
        let t = plantilla_real();
        assert_eq!(
            t.matches("$.IsThinkSet").count(),
            3,
            "la plantilla real de qwen3:14b tiene TRES $.IsThinkSet"
        );
        assert!(
            t.contains("<|im_start|>"),
            "esto no parece una plantilla de qwen"
        );
    }

    #[test]
    fn parchea_las_dos_zonas() {
        let r = parchear(&plantilla_real()).expect("la plantilla real tiene las dos anclas");
        assert!(
            r.contains(PARCHE_USUARIO),
            "falta el /no_think incondicional"
        );
        assert!(r.contains(PARCHE_PREFILL), "falta el prefill incondicional");
    }

    /// **La propiedad que importa**: después del parche no queda ni una decisión
    /// en manos de la petición. Si quedara, un harness podría reencender el
    /// razonamiento sin saberlo y el nivel 1 mediría dos modelos distintos.
    #[test]
    fn no_queda_ninguna_decision_en_manos_de_la_peticion() {
        let r = parchear(&plantilla_real()).unwrap();
        assert!(
            !r.contains("$.IsThinkSet"),
            "la plantilla parcheada aún consulta $.IsThinkSet: {r}"
        );
        assert!(
            !r.contains("/think\n") && !r.contains("}}/think"),
            "aún puede emitir /think: {r}"
        );
    }

    /// Una plantilla de otro modelo —o de otra versión de ollama— no se parchea
    /// «como se pueda»: se rechaza. Es la guarda entera de este example.
    #[test]
    fn una_plantilla_sin_las_anclas_aborta() {
        let e = parchear("{{ .Content }}").expect_err("debe rechazar una plantilla ajena");
        assert!(
            e.contains("cambió bajo los pies"),
            "mensaje poco claro: {e}"
        );
    }

    /// Un ancla duplicada tampoco vale: dejaría de señalar un sitio concreto.
    #[test]
    fn un_ancla_repetida_aborta() {
        let doble = format!("{}\n{}", plantilla_real(), ANCLA_PREFILL);
        let e = parchear(&doble).expect_err("debe rechazar un ancla ambigua");
        assert!(e.contains("aparece 2 veces"), "mensaje poco claro: {e}");
    }

    #[test]
    fn el_modelfile_hereda_del_base() {
        let m = modelfile("qwen3:14b", "hola").unwrap();
        assert!(m.starts_with("FROM qwen3:14b\n"));
        assert!(m.contains("TEMPLATE \"\"\"hola\"\"\""));
        assert!(m.contains(&format!("PARAMETER num_ctx {NUM_CTX}")));
    }

    /// El defecto de ollama (4096) corta el prompt de cualquier harness real.
    /// Si alguien baja esta constante por ahorrar memoria, el corredor vuelve a
    /// medir sobre un estimulo truncado sin que nadie se entere.
    #[test]
    fn la_ventana_cubre_el_prompt_de_un_harness_real() {
        assert!(NUM_CTX > 4_096, "es el defecto de ollama, el que truncaba");
        assert!(NUM_CTX > 6_500, "Codex manda ~6500 tokens medidos");
        // Y no puede pasarse del contexto que el modelo base declara (40960).
        assert!(NUM_CTX <= 40_960);
    }

    /// Una plantilla con `"""` cerraría el literal antes de tiempo: el Modelfile
    /// resultante diría algo distinto de lo que se pretendía, y `ollama create`
    /// podría aceptarlo igual.
    #[test]
    fn una_plantilla_con_triple_comilla_aborta() {
        let e = modelfile("x", "a \"\"\" b").expect_err("debe rechazarla");
        assert!(e.contains("rompería el literal"), "mensaje poco claro: {e}");
    }

    #[test]
    fn el_destino_por_defecto_conserva_el_tag_del_base() {
        assert_eq!(destino_por_defecto("qwen3:14b"), "qwen3:14b-nothink");
    }
}
