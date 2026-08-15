//! Calibrador de la tarea sonda: ¿la resuelve un modelo local, y cuántas veces
//! de cuántas?
//!
//! # La pregunta que contesta, y por qué decide un issue entero
//!
//! El nivel 1 de #29 —los cuatro harnesses contra el MISMO modelo local, para
//! que el modelo deje de ser un confundidor— solo existe si el modelo local es
//! capaz de resolver la tarea. Si no lo es, ese experimento **no mide nada**:
//! mide cuánto gasta cada harness dando vueltas antes de rendirse, que produce
//! números con muy buena pinta y ningún significado.
//!
//! Por eso esto se corre ANTES de escribir el corredor, no después.
//!
//! # Mide el SUELO, a propósito
//!
//! **Un solo turno, sin herramientas, con todo el contenido en el prompt.** No
//! hay agente aquí: se le entrega el fichero roto y los tests, y se le pide el
//! fichero corregido.
//!
//! Es deliberadamente más fácil que la tarea real —un harness tendría que
//! encontrar los ficheros él solo— y esa es justo la propiedad que se busca:
//! **si el modelo no puede con el fichero en la mano, ningún harness lo va a
//! salvar.** Un suelo que no se pasa cierra la pregunta sin gastar cuota.
//!
//! Lo contrario NO se sigue: pasar el suelo no garantiza que un harness llegue.
//! Eso lo dirá el corredor.
//!
//! # Por qué NO se fija la temperatura a 0
//!
//! Con muestreo determinista las N repeticiones darían la misma respuesta y el
//! `n>1` que pide #29 sería decorativo. Lo que interesa es la PROPORCIÓN de
//! ejecuciones que resuelven, así que se deja el muestreo por defecto del
//! modelo y se publica la tasa.
//!
//! # Uso
//!
//! ```sh
//! cargo run --example calibrar
//! CALIBRAR_MODELOS=qwen2.5:7b,llama3.2:3b CALIBRAR_N=10 cargo run --example calibrar
//! ```
//!
//! Variables:
//!   CALIBRAR_MODELOS  lista separada por comas (default `llama3.2:3b,qwen2.5:7b`)
//!   CALIBRAR_N        repeticiones por modelo (default 10)
//!   CALIBRAR_OLLAMA   puerto de ollama (default 11434)
//!   CALIBRAR_TAREA    directorio de la tarea (default `tareas/reparar-tarifa`)
//!
//! Vive en `examples/` por el mismo motivo que `captura.rs` y `bench.rs`:
//! **Cargo no instala examples**, así que una herramienta de medición no acaba
//! en el PATH de nadie.
use std::path::{Path, PathBuf};
use std::process::Command;

/// Nombre del fichero que el modelo tiene que arreglar.
const FUENTE: &str = "tarifa.py";
/// Nombre del fichero de tests. Se le entrega al modelo, pero **no se le
/// permite tocarlo**: el veredicto tiene que ser del banco, no del examinado.
const TESTS: &str = "test_tarifa.py";

/// Arma el prompt de un turno con todo lo que el modelo necesita.
///
/// Se le pasa el contenido de los tests **entero**. No es hacer trampa: el
/// objetivo es medir si puede razonar el arreglo, no si sabe buscar ficheros
/// —eso es lo que mide el corredor con harnesses—. Y sin los tests delante, un
/// fallo sería ambiguo entre «no supo arreglarlo» y «no supo qué se le pedía».
///
/// # El formato importa, y costó una medición entera
///
/// La primera versión separaba los ficheros con cabeceras `--- fichero ---`.
/// El modelo **las devolvía dentro del bloque de código**, y `--- tarifa.py ---`
/// no es Python: `SyntaxError`. Esa fila se contaba como «el modelo no supo
/// arreglarlo» cuando el arreglo estaba bien y lo roto era el formato.
///
/// Ahora cada fichero va en **su propio bloque cercado**, que es la forma en la
/// que un modelo espera ver código — y así la respuesta natural es también un
/// bloque, con un solo fichero dentro.
fn construir_prompt(fuente: &str, tests: &str) -> String {
    format!(
        "Este fichero de Python (`{FUENTE}`) tiene errores:\n\
         \n\
         ```python\n\
         {fuente}\n\
         ```\n\
         \n\
         Y por eso fallan sus tests (`{TESTS}`), que NO debes modificar:\n\
         \n\
         ```python\n\
         {tests}\n\
         ```\n\
         \n\
         Devuelve SOLO el contenido corregido de `{FUENTE}`, completo, en un\n\
         único bloque ```python. No incluyas los tests. No expliques nada.\n"
    )
}

/// Saca el código del primer bloque cercado de la respuesta.
///
/// Un modelo pequeño casi nunca contesta solo con el código: mete un párrafo
/// antes, o un «aquí tienes» después. Escribir la respuesta cruda como
/// `.py` haría fallar el test por un `SyntaxError` que **no es del modelo sino
/// de la extracción**, y esa fila contaría como «no resuelto» mintiendo.
///
/// Si no hay ningún bloque cercado se devuelve `None` en vez de adivinar.
/// `None` aquí significa «no supe extraer», que se cuenta aparte de «resolvió
/// mal» — mismo criterio que el resto del proyecto con los nulos.
fn extraer_codigo(respuesta: &str) -> Option<String> {
    let inicio = respuesta.find("```")?;
    let tras_las_comillas = &respuesta[inicio + 3..];
    // La primera línea tras ``` es el lenguaje (```python), o vacía.
    let salto = tras_las_comillas.find('\n')?;
    let cuerpo = &tras_las_comillas[salto + 1..];
    let fin = cuerpo.find("```")?;
    let codigo = cuerpo[..fin].trim_end();
    if codigo.trim().is_empty() {
        return None;
    }
    Some(codigo.to_string())
}

/// Corta el código en el punto donde empieza el SEGUNDO fichero, si lo hay.
///
/// # El fallo de medición que esto arregla
///
/// Pedido «devuelve `tarifa.py` completo», `qwen2.5:7b` devolvió **los dos
/// ficheros pegados dentro del mismo bloque** en 4 de 6 respuestas (95-103
/// líneas, frente a 23-25 de una respuesta correcta).
///
/// Escribir eso entero como `tarifa.py` produce un fichero que se importa a sí
/// mismo y falla — y esa fila se contaba como **`NoResuelto`, culpando al
/// modelo de un fallo del instrumento**. Se comprobó con una respuesta cuyo
/// `coste_usd` era byte a byte idéntico al de otra que sí pasó.
///
/// La frontera es `from <modulo> import`: **un módulo no puede importarse a sí
/// mismo**, así que esa línea solo puede pertenecer al fichero de tests. Lo que
/// queda pegado por delante —el docstring de los tests y algún `import`— es
/// Python válido e inerte, así que no altera el veredicto.
///
/// Devuelve también SI hubo que recortar, para que el informe lo publique: un
/// arreglo silencioso escondería con qué frecuencia el modelo ignora el
/// formato pedido, que es un dato del experimento.
fn recortar_al_fichero(codigo: &str, modulo: &str) -> (String, bool) {
    let marca = format!("from {modulo} import");
    let (recortado, corte) = match codigo.find(&marca) {
        None => (codigo, false),
        Some(pos) => (&codigo[..pos], true),
    };

    // Segunda defensa: el modelo puede devolver cabeceras `--- fichero ---` de
    // un prompt anterior o inventadas. No son Python y revientan el fichero con
    // un `SyntaxError` que se contaría contra el modelo. Se quitan por línea.
    let mut sobraban_marcadores = false;
    let limpio: Vec<&str> = recortado
        .lines()
        .filter(|l| {
            let t = l.trim();
            let es_marcador = t.starts_with("---") && t.ends_with("---") && t.len() > 5;
            if es_marcador {
                sobraban_marcadores = true;
            }
            !es_marcador
        })
        .collect();

    (
        limpio.join("\n").trim_end().to_string(),
        corte || sobraban_marcadores,
    )
}

/// Veredicto de una ejecución. Los tres casos se cuentan por separado a
/// propósito: colapsarlos en «no resuelto» escondería si el problema es del
/// modelo o del banco.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Veredicto {
    /// Los tests pasan.
    Resuelto,
    /// Los tests fallan.
    NoResuelto,
    /// No se pudo extraer código de la respuesta.
    SinCodigo,
}

fn var(nombre: &str, defecto: &str) -> String {
    std::env::var(nombre).unwrap_or_else(|_| defecto.to_string())
}

/// Deja una copia limpia de la tarea en `destino`. **Cada ejecución parte del
/// mismo estado**: sin esto, la segunda repetición heredaría el arreglo de la
/// primera y la tasa saldría inflada.
fn preparar(origen: &Path, destino: &Path) -> std::io::Result<()> {
    if destino.exists() {
        std::fs::remove_dir_all(destino)?;
    }
    std::fs::create_dir_all(destino)?;
    for f in [FUENTE, TESTS] {
        std::fs::copy(origen.join(f), destino.join(f))?;
    }
    Ok(())
}

/// Corre los tests y dice si pasan. El veredicto es el **código de salida**,
/// no lo que diga por pantalla.
fn tests_pasan(dir: &Path) -> bool {
    Command::new("python3")
        .arg(TESTS)
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn pedir(
    cliente: &reqwest::Client,
    puerto: &str,
    modelo: &str,
    prompt: &str,
) -> Option<String> {
    let url = format!("http://127.0.0.1:{puerto}/api/chat");
    let cuerpo = serde_json::json!({
        "model": modelo,
        "stream": false,
        "messages": [{ "role": "user", "content": prompt }],
    });
    let resp = cliente.post(&url).json(&cuerpo).send().await.ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    v["message"]["content"].as_str().map(|s| s.to_string())
}

#[tokio::main]
async fn main() {
    let tarea = PathBuf::from(var("CALIBRAR_TAREA", "tareas/reparar-tarifa"));
    let puerto = var("CALIBRAR_OLLAMA", "11434");
    let n: usize = var("CALIBRAR_N", "10").parse().unwrap_or(10);
    let modelos: Vec<String> = var("CALIBRAR_MODELOS", "llama3.2:3b,qwen2.5:7b")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let fuente = match std::fs::read_to_string(tarea.join(FUENTE)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "calibrar: no se pudo leer {}: {e}",
                tarea.join(FUENTE).display()
            );
            std::process::exit(1);
        }
    };
    let tests = match std::fs::read_to_string(tarea.join(TESTS)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "calibrar: no se pudo leer {}: {e}",
                tarea.join(TESTS).display()
            );
            std::process::exit(1);
        }
    };

    // GUARDA DEL BANCO. Si el estado inicial pasa, la tarea no mide nada y
    // cualquier tasa que saliera de aquí sería falsa. Se aborta en vez de
    // publicar números.
    let control = std::env::temp_dir().join("calibrar-control");
    if preparar(&tarea, &control).is_ok() && tests_pasan(&control) {
        eprintln!("calibrar: ABORTADO — el estado inicial de la tarea YA PASA.");
        eprintln!("          El banco está roto: una tarea que no falla no mide nada.");
        std::process::exit(1);
    }
    println!("guarda: el estado inicial falla, como debe.\n");

    let prompt = construir_prompt(&fuente, &tests);
    let cliente = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .expect("cliente construible");

    println!("tarea   : {}", tarea.display());
    println!("modelos : {}", modelos.join(", "));
    println!("n       : {n}  (muestreo por defecto, no temperatura 0)\n");

    let mut resumen: Vec<(String, usize, usize, usize, usize)> = Vec::new();

    let modulo = FUENTE.trim_end_matches(".py").to_string();

    for modelo in &modelos {
        let (mut ok, mut mal, mut sin, mut mezclados) = (0usize, 0usize, 0usize, 0usize);
        for i in 1..=n {
            let dir = std::env::temp_dir().join(format!("calibrar-{}", i));
            if let Err(e) = preparar(&tarea, &dir) {
                eprintln!("  {modelo} [{i}/{n}] no se pudo preparar: {e}");
                continue;
            }

            let veredicto = match pedir(&cliente, &puerto, modelo, &prompt).await {
                None => Veredicto::SinCodigo,
                Some(respuesta) => match extraer_codigo(&respuesta) {
                    None => Veredicto::SinCodigo,
                    Some(codigo) => {
                        let (codigo, recortado) = recortar_al_fichero(&codigo, &modulo);
                        if recortado {
                            mezclados += 1;
                        }
                        if std::fs::write(dir.join(FUENTE), codigo).is_err() {
                            Veredicto::SinCodigo
                        } else if tests_pasan(&dir) {
                            Veredicto::Resuelto
                        } else {
                            Veredicto::NoResuelto
                        }
                    }
                },
            };

            match veredicto {
                Veredicto::Resuelto => ok += 1,
                Veredicto::NoResuelto => mal += 1,
                Veredicto::SinCodigo => sin += 1,
            }
            println!("  {modelo} [{i}/{n}] {veredicto:?}");
            let _ = std::fs::remove_dir_all(&dir);
        }
        println!();
        resumen.push((modelo.clone(), ok, mal, sin, mezclados));
    }

    println!("=== calibración ===");
    println!(
        "{:<18} {:>9} {:>10} {:>11} {:>11}",
        "modelo", "resuelto", "fallado", "sin código", "mezclados"
    );
    for (modelo, ok, mal, sin, mezclados) in &resumen {
        println!(
            "{modelo:<18} {:>7}/{n} {mal:>10} {sin:>11} {mezclados:>11}",
            ok
        );
    }
    println!();
    println!("`mezclados` = respuestas que pegaron los DOS ficheros en un mismo bloque y");
    println!("hubo que recortar. No es un fallo de razonamiento, pero sí un dato: mide");
    println!("con qué frecuencia el modelo ignora el formato que se le pidió.");
    println!();
    println!("Una tasa de 0/{n} en TODOS los modelos significa que el nivel 1 de #29");
    println!("no existe con este hardware — y eso es un resultado, no un fallo.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extrae_el_codigo_de_un_bloque_con_lenguaje() {
        let respuesta = "Aquí tienes:\n```python\ndef f():\n    return 1\n```\nEspero que sirva.";

        let codigo = extraer_codigo(respuesta).expect("hay bloque");

        assert_eq!(codigo, "def f():\n    return 1");
    }

    /// La prosa de alrededor es lo normal en un modelo pequeño, y escribirla
    /// al `.py` daría un `SyntaxError` que se contaría como «no resuelto»
    /// mintiendo: el fallo sería de la extracción, no del modelo.
    #[test]
    fn la_prosa_de_alrededor_no_entra_en_el_fichero() {
        let respuesta = "El error es la division.\n```\nx = 1\n```\nY ya está.";

        let codigo = extraer_codigo(respuesta).expect("hay bloque");

        assert!(!codigo.contains("division"));
        assert!(!codigo.contains("Y ya"));
        assert_eq!(codigo, "x = 1");
    }

    /// **Falla honesto.** Sin bloque cercado no se adivina: `None` significa
    /// «no supe extraer» y se cuenta aparte de «resolvió mal», igual que el
    /// resto del proyecto separa «no se pudo medir» de «cero».
    #[test]
    fn sin_bloque_cercado_devuelve_none_en_vez_de_adivinar() {
        assert!(extraer_codigo("El error esta en la division por mil.").is_none());
        assert!(extraer_codigo("").is_none());
    }

    /// Un bloque abierto y nunca cerrado —respuesta truncada por el tope de
    /// tokens— no es código extraíble.
    #[test]
    fn un_bloque_sin_cerrar_no_es_codigo() {
        assert!(extraer_codigo("```python\ndef f():\n    return 1").is_none());
    }

    #[test]
    fn un_bloque_vacio_no_es_codigo() {
        assert!(extraer_codigo("```python\n\n```").is_none());
    }

    /// El prompt tiene que llevar los DOS ficheros: sin los tests delante, un
    /// fallo sería ambiguo entre «no supo arreglarlo» y «no supo qué se pedía».
    /// **El fallo de medición que casi se publica.** El modelo devuelve los dos
    /// ficheros pegados; escribirlos juntos como fuente da un módulo que se
    /// importa a sí mismo, falla, y se contabiliza como si el modelo no hubiera
    /// sabido arreglarlo.
    #[test]
    fn recorta_el_segundo_fichero_pegado_en_el_mismo_bloque() {
        let pegado = "def coste_usd():\n    return 1\n\n\
                      \"\"\"Comprobaciones.\"\"\"\n\n\
                      import sys\n\n\
                      from tarifa import coste_usd\n\n\
                      def main():\n    pass\n";

        let (codigo, recortado) = recortar_al_fichero(pegado, "tarifa");

        assert!(recortado, "tiene que detectar el segundo fichero");
        assert!(codigo.contains("def coste_usd"));
        assert!(!codigo.contains("def main"));
        assert!(!codigo.contains("from tarifa import"));
    }

    /// **El `SyntaxError` que se contaba contra el modelo.** Devolvía dentro del
    /// bloque las cabeceras `--- fichero ---` del prompt; `--- tarifa.py ---` no
    /// es Python, el fichero no cargaba, y la fila salía como «no resuelto»
    /// aunque el arreglo fuera correcto.
    #[test]
    fn las_cabeceras_de_fichero_no_llegan_al_py() {
        let con_cabeceras =
            "--- tarifa.py ---\nTARIFA_CACHE = 0.10\n\ndef coste_usd():\n    return 1\n";

        let (codigo, tocado) = recortar_al_fichero(con_cabeceras, "tarifa");

        assert!(tocado, "tiene que avisar de que hubo que limpiar");
        assert!(!codigo.contains("--- tarifa.py ---"));
        assert!(codigo.starts_with("TARIFA_CACHE"));
        assert!(codigo.contains("def coste_usd"));
    }

    /// Un comentario o una línea de guiones decorativa NO es una cabecera de
    /// fichero: quitarla cambiaría el código del modelo por iniciativa propia.
    #[test]
    fn una_linea_de_guiones_corta_no_se_confunde_con_una_cabecera() {
        let codigo_con_guiones = "x = 1\n# ---\ny = 2\n";

        let (codigo, tocado) = recortar_al_fichero(codigo_con_guiones, "tarifa");

        assert!(!tocado);
        assert!(codigo.contains("# ---"));
    }

    /// Una respuesta correcta —un solo fichero— no se toca. El recorte no puede
    /// morder cuando no hay nada que recortar.
    #[test]
    fn una_respuesta_de_un_solo_fichero_no_se_recorta() {
        let limpio = "TARIFA_CACHE = 0.10\n\ndef coste_usd():\n    return 1\n";

        let (codigo, recortado) = recortar_al_fichero(limpio, "tarifa");

        assert!(!recortado);
        // Se compara sin el salto final: la función normaliza el espacio de
        // cola, y eso no es «recortar código» — en Python da igual.
        assert_eq!(codigo, limpio.trim_end());
    }

    /// La frontera es `from <modulo> import` porque **un módulo no puede
    /// importarse a sí mismo**: esa línea solo puede ser del fichero de tests.
    /// Importar OTRO módulo es legítimo y no debe cortar nada.
    #[test]
    fn importar_otro_modulo_no_es_frontera() {
        let con_import =
            "import math\nfrom decimal import Decimal\n\ndef coste_usd():\n    return 1\n";

        let (_, recortado) = recortar_al_fichero(con_import, "tarifa");

        assert!(!recortado);
    }

    #[test]
    fn el_prompt_lleva_la_fuente_y_los_tests() {
        let p = construir_prompt("CONTENIDO_FUENTE", "CONTENIDO_TESTS");

        assert!(p.contains("CONTENIDO_FUENTE"));
        assert!(p.contains("CONTENIDO_TESTS"));
        assert!(p.contains(FUENTE));
        assert!(p.contains(TESTS));
    }
}
