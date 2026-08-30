//! Sonda de herramientas: ¿este modelo sabe emitir llamadas, sabe usar lo que
//! le devuelven, y **cuánto depende eso de cómo esté redactado el encargo**?
//!
//! # La pregunta que contesta, y el issue que reventó por no hacerla antes
//!
//! [#121](https://github.com/pichu2707/OxideGate/issues/121) —el corredor del
//! nivel 1— se bloqueó a mitad de camino: el cableado funcionaba, el proxy
//! medía, las herramientas se declaraban y llegaban, y aun así `qwen2.5:7b` no
//! emitió ni una llamada en los seis turnos del harness. Se inventó las
//! respuestas de herramienta y nunca tocó la tarea.
//!
//! Aquel diagnóstico concluyó «este modelo no emite llamadas a herramientas».
//! **Es falso**, y esta sonda lo demuestra: emite 5/5 cuando se le ordena, y usa
//! 5/5 el resultado que se le devuelve. Lo que falla está más arriba.
//!
//! # Lo que esta sonda NO es, y por qué conviene decirlo primero
//!
//! **No es la puerta de #121.** Se intentó que lo fuera y no se sostiene.
//!
//! La versión con un nivel de «iniciativa» —un encargo, sin nombrar la
//! herramienta, aprobado o suspenso— se cayó en cuanto se midió con `n>1`.
//! Misma tarea, cambiando solo la redacción, el uso de herramientas se mueve.
//! Lo único **consistente en todas las ejecuciones y los dos modelos medidos**
//! es la cabeza: `averigua` sale siempre arriba, y siempre al máximo (5/5, 5/5,
//! 30/30). Cuál queda ABAJO cambia — con `qwen3:14b` a n=5 `arregla (seco)`
//! empataba en cabeza, y a n=30 el último fue `constatación`.
//!
//! La dirección está establecida; **el tamaño del efecto y el orden de la cola,
//! NO**. Ver [`N_MINIMO_PUBLICABLE`]: con `n=5` el rango se movía solo entre
//! ejecuciones idénticas, así que cualquier cifra concreta de esta sección sería
//! azar con formato de tabla. No citar magnitudes ni ordenaciones sin `n` alto.
//!
//! Lo que sí queda: la iniciativa no es una propiedad del modelo, es del par
//! **(modelo × redacción)**. Un umbral construido sobre un prompt escrito a
//! mano mide a quien lo escribió. Y lo que hundió #121 fue `pi` con SU system
//! prompt y SUS cuatro herramientas durante seis turnos — eso no se sintetiza
//! en un turno. **La puerta de #121 tiene que ser el harness real.**
//!
//! # Lo que esta sonda SÍ es
//!
//! 1. **Una criba barata, necesaria y no suficiente** (niveles PROTOCOLO y
//!    ENCADENAR): lo que resultó estable y reproducible. Suspenderla cierra la
//!    pregunta gratis; aprobarla no promete nada.
//! 2. **Una medida de la SENSIBILIDAD A LA REDACCIÓN**: la misma tarea escrita
//!    de varias formas, y se publica el **rango**, no un veredicto.
//!
//! # Por qué el rango es un dato para #29, y no una excusa
//!
//! [#29](https://github.com/pichu2707/OxideGate/issues/29) quiere comparar
//! herramientas sobre la MISMA tarea. Si una palabra mueve el uso de
//! herramientas en el mismo modelo y la misma tarea, entonces una parte de lo
//! que distingue a un harness de otro puede ser, simplemente, **cómo redacta
//! sus prompts de sistema**. Eso es una hipótesis medible, y esta sonda existe
//! para darle un número: cuánta varianza cabe atribuir a la redacción antes de
//! empezar a atribuírsela a la herramienta.
//!
//! # Las dos anclas, que son la guarda de este banco
//!
//! La batería lleva dos redacciones que no son encargos sino calibración:
//!
//! - **Techo** — se le NOMBRA la herramienta. Si no emite, ha cambiado algo bajo
//!   los pies (plantilla, versión de ollama, cableado) y nada es comparable.
//! - **Suelo** — un saludo, sin tarea ni fichero. Si emite aquí, el modelo llama
//!   a ciegas y la batería entera deja de discriminar: cualquier rango que
//!   saliera sería ruido con formato de tabla.
//!
//! Si cualquiera de las dos anclas falla, **se aborta sin publicar nada**. Mismo
//! criterio que la guarda de `calibrar.rs` y que el `TAREA.md` del banco: un
//! instrumento que no suspende lo que sabe que está mal no mide nada. La guarda
//! anterior ya paró dos mediciones malas antes de que se publicaran, y la
//! segunda vez le paró los pies al diseño, no al modelo.
//!
//! # Por qué no vale con `calibrar.rs`
//!
//! `calibrar.rs` mide el suelo de RAZONAMIENTO: un turno, sin herramientas, el
//! fichero en la mano. `qwen2.5:7b` lo pasa 4/10 — sabe arreglar el código, y
//! aun así no conduce un harness. Son capacidades distintas y hacen falta las
//! dos; medir una y suponer la otra es lo que bloqueó #121.
//!
//! # El centinela, y el falso positivo que evita
//!
//! El fichero que devuelve la herramienta contiene un valor que el modelo no
//! puede adivinar ([`CENTINELA`]). Si fuera un número plausible —`0.10`, `1.0`—
//! un modelo que ignorase la respuesta y se lo inventara acertaría de vez en
//! cuando, y esas filas se contarían como «encadenó» **mintiendo**.
//!
//! # Uso
//!
//! ```sh
//! cargo run --example sonda-herramientas
//! SONDA_MODELOS=qwen3:14b SONDA_N=10 cargo run --example sonda-herramientas
//! ```
//!
//! Variables:
//!   SONDA_MODELOS  lista separada por comas (default `qwen3:14b,qwen2.5-coder:14b`)
//!   SONDA_N        repeticiones por redacción (default 5)
//!   SONDA_OLLAMA   puerto de ollama (default 11434)
//!
//! Vive en `examples/` por el mismo motivo que `calibrar.rs`, `captura.rs` y
//! `bench.rs`: **Cargo no instala examples**, así que una herramienta de
//! medición no acaba en el PATH de nadie.
use serde_json::{Value, json};

/// Nombre de la única herramienta declarada. Una basta: si un modelo no sabe
/// emitir UNA llamada, no va a saber orquestar cuatro.
const HERRAMIENTA: &str = "leer_fichero";
/// Único argumento de la herramienta.
const ARGUMENTO: &str = "ruta";
/// Fichero que se le pide leer. No existe en ninguna parte: la herramienta es
/// simulada por la sonda, porque lo que se mide es el PROTOCOLO, no el disco.
const FICHERO: &str = "tarifa.py";
/// Constante cuyo valor se le pregunta al modelo en la redacción techo.
const CONSTANTE: &str = "TARIFA_CACHE";
/// El valor que devuelve la herramienta simulada. **Arbitrario a propósito**:
/// ver la sección del centinela en la cabecera del fichero.
const CENTINELA: &str = "7.3125";

/// Declaración de herramientas en el formato que espera `/api/chat` de ollama
/// (el mismo esquema de OpenAI).
fn herramientas() -> Value {
    json!([{
        "type": "function",
        "function": {
            "name": HERRAMIENTA,
            "description": "Lee un fichero del disco y devuelve su contenido completo.",
            "parameters": {
                "type": "object",
                "properties": {
                    ARGUMENTO: {
                        "type": "string",
                        "description": "Ruta del fichero que hay que leer.",
                    }
                },
                "required": [ARGUMENTO],
            }
        }
    }])
}

/// Qué papel juega una redacción dentro de la batería.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Papel {
    /// Ancla superior: se le NOMBRA la herramienta. **Debe** emitir.
    Techo,
    /// Ancla inferior: no hay tarea ni fichero. **No debe** emitir.
    Suelo,
    /// Una redacción del encargo real. Aquí es donde se mide la varianza.
    Encargo,
}

/// Una forma de pedir lo mismo.
struct Redaccion {
    nombre: &'static str,
    texto: String,
    papel: Papel,
}

/// La batería: dos anclas y las redacciones del MISMO encargo.
///
/// # Las reglas que la hacen una medida y no una anécdota
///
/// - Los encargos **nunca nombran la herramienta**. Nombrarla mide obediencia,
///   y la obediencia la aprueba hasta el modelo que #121 descartó.
/// - Los encargos hablan todos del **mismo fichero y la misma avería**. Si
///   variara la tarea, el rango mediría dificultad y no redacción — que es
///   exactamente el confundidor que #29 existe para quitar.
/// - Lo que cambia entre ellos es el **verbo y el marco**: investigar, reparar,
///   o solo constatar el hecho sin pedir nada.
fn bateria() -> Vec<Redaccion> {
    vec![
        Redaccion {
            nombre: "techo (se le nombra)",
            papel: Papel::Techo,
            texto: format!(
                "¿Qué valor tiene la constante `{CONSTANTE}` en el fichero `{FICHERO}`?\n\
                 \n\
                 No puedes saberlo de memoria: usa la herramienta `{HERRAMIENTA}` para\n\
                 leer el fichero y contesta con el valor exacto que encuentres.\n"
            ),
        },
        Redaccion {
            nombre: "averigua",
            papel: Papel::Encargo,
            texto: format!(
                "El fichero `{FICHERO}` tiene un error y por eso fallan sus tests.\n\
                 Averigua cuál es el error.\n"
            ),
        },
        Redaccion {
            nombre: "arreglalo (con contexto)",
            papel: Papel::Encargo,
            texto: format!(
                "El fichero `{FICHERO}` tiene un error y por eso fallan sus tests.\n\
                 Arréglalo.\n"
            ),
        },
        Redaccion {
            nombre: "arregla (seco)",
            papel: Papel::Encargo,
            texto: format!("Arregla el fichero {FICHERO}, que sus tests fallan.\n"),
        },
        Redaccion {
            nombre: "constatacion (sin peticion)",
            papel: Papel::Encargo,
            texto: format!("Los tests de `{FICHERO}` fallan.\n"),
        },
        Redaccion {
            nombre: "suelo (sin tarea)",
            papel: Papel::Suelo,
            texto: "Hola, ¿qué tal estás?\n".to_string(),
        },
    ]
}

/// Lo que devuelve la herramienta simulada.
///
/// La herramienta **no toca el disco**: lo que se mide es el protocolo, no la
/// lectura de ficheros. Simularla mantiene el experimento cerrado —sin estado
/// entre repeticiones y sin nada que preparar o limpiar— y deja el centinela
/// bajo control de la sonda.
///
/// Lleva una segunda constante de relleno para que contestar no sea repetir el
/// único número que había: el modelo tiene que ELEGIR cuál de los dos se le
/// pidió.
fn contenido_del_fichero() -> String {
    format!("{CONSTANTE} = {CENTINELA}\nTARIFA_ENTRADA = 3.0\n")
}

/// Qué hizo el modelo cuando se le dieron herramientas.
///
/// Los casos se separan a propósito: colapsarlos en «no sabe» escondería el
/// diagnóstico. `Pseudollamada` y `SoloTexto` son fallos MUY distintos —el
/// primero dice que el modelo lo intentó y la plantilla no lo emitió, que es
/// reparable; el segundo dice que ni lo intentó.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Veredicto {
    /// `tool_calls` presente, nombre declarado, argumentos con la clave.
    Emitida,
    /// Emitió una llamada, pero a una herramienta que nadie declaró.
    NombreInventado,
    /// El nombre está bien y los argumentos no parsean o les falta la clave.
    ArgumentosRotos,
    /// Sin `tool_calls`, pero escribió la llamada DENTRO del texto. Es el fallo
    /// exacto que #121 midió en `qwen2.5:7b` bajo el harness.
    Pseudollamada,
    /// Sin `tool_calls` y sin rastro de intento: contestó en prosa.
    SoloTexto,
}

/// Qué hizo el modelo con el resultado de la herramienta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encadenado {
    /// Contestó con el centinela: leyó lo que se le devolvió.
    Uso,
    /// El centinela está en `thinking` y `content` viene VACÍO: leyó el
    /// resultado y no entregó respuesta.
    ///
    /// # El caso que se contaba como `Ignoro` mintiendo
    ///
    /// `qwen3:14b` hace esto **18/30** por `/api/chat` y **13/30** por `/v1`
    /// (medido el 2026-08-25 a `n=30`). Una versión anterior de esta función
    /// solo miraba `content` y le apuntaba «ignoró el resultado» a un modelo que
    /// lo había leído perfectamente — un fallo del instrumento cargado a la
    /// cuenta del modelo, igual que los que #120 dejó documentados en
    /// `calibrar.rs`.
    ///
    /// No se cuenta como `Uso` porque **un harness consume `content`**: si viene
    /// vacío, el agente se queda sin nada aunque el modelo lo supiera. Son dos
    /// hechos distintos y se publican por separado.
    ///
    /// # No es incurable, y creerlo costó ocho días
    ///
    /// Este doc-comment publicaba **5/5**, que era `n=5` y se leía como
    /// «siempre» (E-011). De ahí se dedujo que `qwen3:14b` no servía para el
    /// nivel 1, y con esa deducción #121 se quedó sin candidato local.
    ///
    /// Es una propiedad del modelo **con el razonamiento encendido**. Apagado,
    /// el mismo modelo entrega 30/30. La cura no es un campo en la petición
    /// —que ningún harness manda— sino **otro tag**, con el razonamiento
    /// apagado dentro del modelo.
    PensoSinContestar,
    /// Contestó sin el centinela por ninguna parte: ignoró el resultado.
    Ignoro,
    /// Volvió a llamar a la herramienta en vez de contestar. No es usar el
    /// resultado, pero tampoco es inventárselo: se cuenta aparte.
    VolvioALlamar,
}

/// Lee los argumentos de una llamada, vengan como vengan.
///
/// # Las dos formas, y por qué se aceptan las dos
///
/// El `/api/chat` nativo de ollama devuelve `arguments` como **objeto**; el
/// endpoint compatible con OpenAI —que es el que atraviesa OxideGate— lo
/// devuelve como **cadena JSON**. Comprobado el 2026-08-16 contra el mismo
/// modelo. Aceptar solo una haría que el veredicto dependiera de por dónde se
/// le habló al modelo y no del modelo, que es justo el confundidor que #29
/// existe para quitar.
///
/// Devuelve `None` cuando no hay objeto que leer. **No se adivina**: una cadena
/// que no parsea es un fallo, no una invitación a inventarse la ruta.
fn argumentos(bruto: &Value) -> Option<serde_json::Map<String, Value>> {
    if let Some(objeto) = bruto.as_object() {
        return Some(objeto.clone());
    }
    let texto = bruto.as_str()?;
    serde_json::from_str::<Value>(texto)
        .ok()?
        .as_object()
        .cloned()
}

/// Clasifica el mensaje del asistente de un primer turno.
fn clasificar(mensaje: &Value) -> Veredicto {
    let llamada = mensaje["tool_calls"].as_array().and_then(|l| l.first());

    let Some(llamada) = llamada else {
        // Sin `tool_calls`. Que lo haya INTENTADO por el texto o no es el dato
        // que separa un fallo reparable de uno que no lo es.
        let texto = mensaje["content"].as_str().unwrap_or("");
        return if parece_pseudollamada(texto) {
            Veredicto::Pseudollamada
        } else {
            Veredicto::SoloTexto
        };
    };

    if llamada["function"]["name"].as_str().unwrap_or("") != HERRAMIENTA {
        return Veredicto::NombreInventado;
    }

    // Se exige que la clave exista y sea una cadena, pero NO que valga
    // exactamente `FICHERO`: un modelo que pida `./tarifa.py` ha entendido el
    // protocolo, y suspenderlo por normalizar rutas mediría otra cosa.
    match argumentos(&llamada["function"]["arguments"]) {
        Some(args) if args.get(ARGUMENTO).and_then(Value::as_str).is_some() => Veredicto::Emitida,
        _ => Veredicto::ArgumentosRotos,
    }
}

/// ¿El texto lleva escrita una llamada que debería haber ido por su canal?
///
/// Se exige **estructura**, no vocabulario: un modelo que EXPLICA en prosa que
/// «llamaría a la función con esos argumentos» no está haciendo una
/// pseudo-llamada, y contarlo como tal inflaría el caso reparable a costa del
/// irreparable.
fn parece_pseudollamada(texto: &str) -> bool {
    // Marcadores de plantilla. Que aparezcan en el CONTENIDO significa que el
    // modelo produjo el formato y la plantilla no lo convirtió en `tool_calls`.
    const MARCAS: [&str; 4] = ["<tool_call", "<function_call", "<tool_response", "<|tool"];
    if MARCAS.iter().any(|m| texto.contains(m)) {
        return true;
    }

    // Un objeto JSON con las dos claves de una llamada. Se exigen las COMILLAS
    // a propósito: sin ellas, cualquier frase que dijera «argumentos» contaría
    // como intento y el informe recomendaría arreglar una plantilla por un
    // modelo que nunca lo intentó.
    texto.contains("\"name\"") && texto.contains("\"arguments\"")
}

/// Clasifica el mensaje del asistente del turno de encadenado, tras devolverle
/// el resultado de la herramienta.
///
/// # Por qué `thinking` solo se mira cuando `content` está vacío
///
/// Un modelo razonador publica dos campos, y **el harness solo consume
/// `content`**. Si hay respuesta entregada, ella manda: contestar ignorando un
/// valor que uno mismo acaba de leer es `Ignoro`, por mucho que el razonamiento
/// lo mencionara. `thinking` solo desempata cuando no hubo entrega — y entonces
/// distingue «leyó y no contestó» de «no se enteró», que son cosas distintas.
fn clasificar_encadenado(mensaje: &Value) -> Encadenado {
    if mensaje["tool_calls"]
        .as_array()
        .is_some_and(|l| !l.is_empty())
    {
        return Encadenado::VolvioALlamar;
    }

    let contenido = mensaje["content"].as_str().unwrap_or("");
    if !contenido.trim().is_empty() {
        return if contenido.contains(CENTINELA) {
            Encadenado::Uso
        } else {
            Encadenado::Ignoro
        };
    }

    let pensamiento = mensaje["thinking"].as_str().unwrap_or("");
    if pensamiento.contains(CENTINELA) {
        Encadenado::PensoSinContestar
    } else {
        Encadenado::Ignoro
    }
}

/// Repeticiones por debajo de las cuales el rango NO es publicable.
///
/// # El número que salió de una medición, no de una intuición
///
/// Con `n=5`, la misma batería corrida dos veces sobre `qwen2.5:7b` sin cambiar
/// nada dio esto (2026-08-16):
///
/// | redacción | 1.ª | 2.ª |
/// |---|---|---|
/// | averigua | 5/5 | 5/5 |
/// | arréglalo | 4/5 | 3/5 |
/// | **arregla (seco)** | **1/5** | **3/5** |
/// | constatación | 4/5 | 3/5 |
/// | **rango** | **1-5/5** | **3-5/5** |
///
/// Con tasas cerca del 50%, el error de muestreo de cinco tiradas es **del mismo
/// tamaño que el efecto**: el rango publicado se movía solo. Y como la sonda
/// corre en local y gratis, subir `n` no cuesta nada más que tiempo de tarjeta —
/// no hay ninguna excusa para publicar un número inestable.
const N_MINIMO_PUBLICABLE: usize = 30;

/// El rango de tasas entre las redacciones del encargo: `(mínimo, máximo)`.
///
/// **Es la medida que publica esta sonda.** No hay umbral ni aprobado: el rango
/// dice cuánta de la variación observada cabe atribuir a la redacción antes de
/// atribuírsela a la herramienta o al modelo.
///
/// Sin encargos devuelve `None` en vez de un cero que parecería «no hay
/// varianza», mismo criterio que el resto del proyecto con los nulos.
fn sensibilidad(tasas: &[usize]) -> Option<(usize, usize)> {
    let min = *tasas.iter().min()?;
    let max = *tasas.iter().max()?;
    Some((min, max))
}

fn var(nombre: &str, defecto: &str) -> String {
    std::env::var(nombre).unwrap_or_else(|_| defecto.to_string())
}

/// Un turno contra `/api/chat` de ollama. Devuelve el mensaje del asistente.
///
/// # Por qué no se fija ni la temperatura ni el modo de razonamiento
///
/// Mismo criterio que `calibrar.rs`: con el muestreo por defecto, la PROPORCIÓN
/// sobre `n` repeticiones es el dato.
///
/// Y `think` se deja **como venga de fábrica en el tag que se le pase**, porque
/// eso es exactamente lo que recibiría un harness: ninguno manda ese campo. La
/// sonda mide el modelo tal y como se lo van a encontrar.
///
/// El razonamiento se apaga —cuando hay que apagarlo— **en otro tag**, no aquí:
/// `SONDA_MODELOS=qwen3:14b-nothink`. Así el modo de razonamiento viaja dentro
/// del modelo, es constante para los cuatro harnesses del nivel 1 y se declara
/// como el confundidor que es. Fijarlo en la petición metería a quien mide
/// dentro de lo medido; fijarlo en la config de cada harness devolvería el
/// confundidor que el nivel 1 existe para quitar.
///
/// La justificación anterior decía que apagarlo «cambia lo que se mide, porque
/// un modelo razonador con el razonamiento apagado no es el modelo que luego
/// conduciría el harness». El dato la invierte: con el razonamiento encendido,
/// ese modelo **no conduce ningún harness** —entrega respuesta 17/30—. El que lo
/// conduciría es justamente el de no-think.
async fn pedir(
    cliente: &reqwest::Client,
    puerto: &str,
    modelo: &str,
    mensajes: &[Value],
) -> Option<Value> {
    let url = format!("http://127.0.0.1:{puerto}/api/chat");
    let cuerpo = json!({
        "model": modelo,
        "stream": false,
        "messages": mensajes,
        "tools": herramientas(),
    });
    let resp = cliente.post(&url).json(&cuerpo).send().await.ok()?;
    let v: Value = resp.json().await.ok()?;
    let mensaje = v.get("message")?;
    if mensaje.is_null() {
        return None;
    }
    Some(mensaje.clone())
}

/// Recuento de una redacción. Cada casilla se cuenta por separado: sumarlas en
/// «falló» perdería el diagnóstico, que es lo único que esta sonda aporta sobre
/// un simple sí/no.
#[derive(Default)]
struct Cuenta {
    emitida: usize,
    nombre_inventado: usize,
    argumentos_rotos: usize,
    pseudollamada: usize,
    solo_texto: usize,
    /// La petición no llegó o no se pudo leer. **No es un fallo del modelo** y
    /// por eso no se mezcla con los demás: es «no se pudo medir».
    sin_respuesta: usize,
}

impl Cuenta {
    fn anotar(&mut self, v: Veredicto) {
        match v {
            Veredicto::Emitida => self.emitida += 1,
            Veredicto::NombreInventado => self.nombre_inventado += 1,
            Veredicto::ArgumentosRotos => self.argumentos_rotos += 1,
            Veredicto::Pseudollamada => self.pseudollamada += 1,
            Veredicto::SoloTexto => self.solo_texto += 1,
        }
    }
}

/// Lo medido de un modelo: una cuenta por redacción, más el encadenado.
struct Medida {
    modelo: String,
    /// Alineado con [`bateria`], índice a índice.
    por_redaccion: Vec<Cuenta>,
    uso: usize,
    penso: usize,
    ignoro: usize,
    volvio: usize,
}

impl Medida {
    fn cuenta_de(&self, papel: Papel) -> Option<&Cuenta> {
        bateria()
            .iter()
            .position(|r| r.papel == papel)
            .and_then(|i| self.por_redaccion.get(i))
    }

    /// Tasas de emisión de las redacciones del encargo, en orden de batería.
    fn tasas_de_encargo(&self) -> Vec<usize> {
        bateria()
            .iter()
            .enumerate()
            .filter(|(_, r)| r.papel == Papel::Encargo)
            .filter_map(|(i, _)| self.por_redaccion.get(i).map(|c| c.emitida))
            .collect()
    }
}

/// Corre la batería entera de un modelo, `n` repeticiones por redacción.
///
/// El encadenado se mide **solo sobre el techo**, y a propósito: encadenar e
/// iniciar son capacidades distintas, y este proyecto mide una variable cada
/// vez. Colgándolo del techo, un modelo que apenas emita por su cuenta aún
/// puede demostrar si sabe usar un resultado — eso distingue «no arranca» de
/// «no sabe seguir».
async fn examinar(cliente: &reqwest::Client, puerto: &str, modelo: &str, n: usize) -> Medida {
    let redacciones = bateria();
    let mut por_redaccion: Vec<Cuenta> = Vec::with_capacity(redacciones.len());
    let (mut uso, mut penso, mut ignoro, mut volvio) = (0usize, 0usize, 0usize, 0usize);

    for r in &redacciones {
        let mut c = Cuenta::default();
        for i in 1..=n {
            let mut mensajes = vec![json!({ "role": "user", "content": r.texto })];

            let Some(primero) = pedir(cliente, puerto, modelo, &mensajes).await else {
                c.sin_respuesta += 1;
                println!("  {modelo} · {} [{i}/{n}] SinRespuesta", r.nombre);
                continue;
            };

            let veredicto = clasificar(&primero);
            c.anotar(veredicto);

            // El encadenado solo cuelga del techo.
            let mut cola = String::new();
            if r.papel == Papel::Techo && veredicto == Veredicto::Emitida {
                mensajes.push(primero);
                mensajes.push(json!({
                    "role": "tool",
                    "tool_name": HERRAMIENTA,
                    "content": contenido_del_fichero(),
                }));
                match pedir(cliente, puerto, modelo, &mensajes).await {
                    None => {
                        c.sin_respuesta += 1;
                        cola = " → SinRespuesta".to_string();
                    }
                    Some(segundo) => {
                        let e = clasificar_encadenado(&segundo);
                        match e {
                            Encadenado::Uso => uso += 1,
                            Encadenado::PensoSinContestar => penso += 1,
                            Encadenado::Ignoro => ignoro += 1,
                            Encadenado::VolvioALlamar => volvio += 1,
                        }
                        cola = format!(" → {e:?}");
                    }
                }
            }

            println!("  {modelo} · {} [{i}/{n}] {veredicto:?}{cola}", r.nombre);
        }
        por_redaccion.push(c);
    }

    Medida {
        modelo: modelo.to_string(),
        por_redaccion,
        uso,
        penso,
        ignoro,
        volvio,
    }
}

/// ¿El suelo queda lo bastante por debajo de los encargos como para que la
/// batería discrimine?
///
/// # Por qué NO se exige `suelo == 0`
///
/// Esa era la regla original, y **hacía que medir mejor empeorase el veredicto**:
/// `qwen2.5:7b` da 0/5 ante un saludo y 1/30 (medido el 2026-08-16, con la ruta
/// inventada `/path/to/your/file.txt`). Con el criterio binario aprobaba a `n=5`
/// y suspendía a `n=30`, siendo el mismo modelo. Un ancla cuyo veredicto depende
/// de cuánto midas no ancla nada.
///
/// El ancla existe para probar que la batería **discrimina**, no para exigir
/// pureza. Un 3% de llamadas a ciegas contra encargos al 67-100% discrimina de
/// sobra; un modelo que llame igual al saludo que al encargo, no.
///
/// El margen es **el doble**: el suelo tiene que quedar por debajo de la mitad
/// del encargo más flojo. Sin encargos no hay con qué comparar, y entonces solo
/// vale un suelo limpio.
fn suelo_discrimina(suelo: usize, encargos: &[usize]) -> bool {
    let Some(minimo) = encargos.iter().min().copied() else {
        return suelo == 0;
    };
    suelo * 2 < minimo
}

/// Comprueba las dos anclas de un modelo. `Err` lleva el motivo del aborto.
///
/// Se saca de `main` para que sea legible de un vistazo: es la parte que decide
/// si lo demás se publica o no.
fn anclas_validas(m: &Medida, n: usize) -> Result<(), String> {
    match m.cuenta_de(Papel::Techo) {
        None => return Err("la batería no tiene ancla de techo".to_string()),
        Some(t) if t.emitida == 0 => {
            return Err(format!(
                "{}: el TECHO no emitió ni una vez de {n}, y se le nombra la herramienta.\n\
                 Ha cambiado algo bajo los pies (plantilla, versión de ollama, cableado):\n\
                 sin techo no hay escala, y el rango de abajo no sería comparable con nada.",
                m.modelo
            ));
        }
        Some(_) => {}
    }

    let encargos = m.tasas_de_encargo();
    match m.cuenta_de(Papel::Suelo) {
        None => Err("la batería no tiene ancla de suelo".to_string()),
        Some(s) if !suelo_discrimina(s.emitida, &encargos) => Err(format!(
            "{}: el SUELO emitió {}/{n} llamadas ante un saludo, sin tarea ni fichero,\n\
             y el encargo más flojo dio {}/{n}. El suelo no queda lo bastante por\n\
             debajo: este modelo llama demasiado a ciegas para que la batería\n\
             discrimine, y su rango sería ruido con formato de tabla.",
            m.modelo,
            s.emitida,
            encargos.iter().min().copied().unwrap_or(0)
        )),
        Some(_) => Ok(()),
    }
}

#[tokio::main]
async fn main() {
    let puerto = var("SONDA_OLLAMA", "11434");
    let n: usize = var("SONDA_N", "5").parse().unwrap_or(5);
    let modelos: Vec<String> = var("SONDA_MODELOS", "qwen3:14b,qwen2.5-coder:14b")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let cliente = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .expect("cliente construible");

    let redacciones = bateria();
    println!("redacciones: {}   n: {n}\n", redacciones.len());

    let mut medidas: Vec<Medida> = Vec::new();
    for modelo in &modelos {
        let m = examinar(&cliente, &puerto, modelo, n).await;
        println!();
        // GUARDA. Las anclas se comprueban por modelo y ANTES de publicar su
        // fila: un modelo con las anclas rotas no tiene escala, y colarlo en la
        // tabla contaminaría la comparación con los demás.
        match anclas_validas(&m, n) {
            Err(motivo) => println!("DESCARTADO — {motivo}\n"),
            Ok(()) => medidas.push(m),
        }
    }

    if medidas.is_empty() {
        eprintln!("sonda: ningún modelo pasó las anclas. No se publica ninguna medida.");
        std::process::exit(1);
    }

    println!("=== criba: ¿emite cuando se le nombra la herramienta, y usa el resultado? ===");
    println!(
        "{:<22} {:>9} {:>7} {:>10} {:>11} {:>9} {:>15} {:>8} {:>16}",
        "modelo",
        "techo",
        "pseudo",
        "solo texto",
        "nombre inv.",
        "usó",
        "pensó sin decir",
        "ignoró",
        "volvió a llamar"
    );
    for m in &medidas {
        let t = m.cuenta_de(Papel::Techo).expect("ancla comprobada");
        let base = m.uso + m.penso + m.ignoro + m.volvio;
        println!(
            "{:<22} {:>7}/{n} {:>7} {:>10} {:>11} {:>7}/{base} {:>15} {:>8} {:>16}",
            m.modelo,
            t.emitida,
            t.pseudollamada,
            t.solo_texto,
            t.nombre_inventado,
            m.uso,
            m.penso,
            m.ignoro,
            m.volvio
        );
    }

    println!("\n=== sensibilidad a la redacción: la MISMA tarea, escrita distinto ===");
    if n < N_MINIMO_PUBLICABLE {
        println!(
            "AVISO: n={n} < {N_MINIMO_PUBLICABLE}. El rango de abajo es INDICATIVO, NO publicable."
        );
        println!(
            "Medido: con n=5, la misma bateria corrida dos veces movio `arregla (seco)` de 1/5"
        );
        println!(
            "a 3/5 y el rango de 1-5 a 3-5, sin cambiar nada. Corre con SONDA_N={N_MINIMO_PUBLICABLE}+ antes de citarlo."
        );
    }
    print!("{:<22}", "modelo");
    for r in redacciones.iter().filter(|r| r.papel == Papel::Encargo) {
        print!(" {:>26}", r.nombre);
    }
    println!(" {:>10}", "rango");
    for m in &medidas {
        print!("{:<22}", m.modelo);
        let tasas = m.tasas_de_encargo();
        for t in &tasas {
            print!(" {:>24}/{n}", t);
        }
        match sensibilidad(&tasas) {
            None => println!(" {:>10}", "—"),
            Some((min, max)) => println!(" {:>10}", format!("{min}-{max}/{n}")),
        }
    }

    println!();
    println!("EL RANGO ES EL RESULTADO, no un aprobado. Dice cuánta de la variación en el");
    println!("uso de herramientas cabe atribuir a CÓMO se pide la tarea, antes de empezar a");
    println!("atribuírsela al modelo o a la herramienta.");
    println!();
    println!("Medido con n=30 sobre qwen2.5:7b y qwen3:14b: `averigua` sale 30/30 en LOS DOS");
    println!("—al máximo—, y `constatación` queda abajo o empatada abajo en los dos. El resto");
    println!("se apiña entre medias y su orden NO es estable: la cola no se ordena.");
    println!();
    println!(
        "Con n < {N_MINIMO_PUBLICABLE} el rango se mueve solo entre ejecuciones identicas: no citarlo."
    );
    println!();
    println!("`pseudo` = escribió la llamada en el TEXTO en vez de emitirla. Es el único de");
    println!("los fallos que puede ser de la plantilla y no del modelo — merece mirarse");
    println!("antes de descartar un candidato.");
    println!();
    println!("ESTO NO ES LA PUERTA DE #121. Es una criba necesaria y no suficiente: aprobar");
    println!("la columna `techo` no promete que el modelo aguante seis turnos de harness con");
    println!("cuatro herramientas y un system prompt ajeno. Eso lo dirá el corredor.");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Una llamada bien formada, tal y como la devuelve `/api/chat` de ollama:
    /// `arguments` es un OBJETO, no una cadena.
    #[test]
    fn una_llamada_bien_formada_es_emitida() {
        let mensaje = json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "function": { "name": HERRAMIENTA, "arguments": { ARGUMENTO: FICHERO } }
            }]
        });

        assert_eq!(clasificar(&mensaje), Veredicto::Emitida);
    }

    /// **La incompatibilidad que costaría una medición entera.** El `/api/chat`
    /// nativo de ollama devuelve `arguments` como objeto, pero su endpoint
    /// compatible con OpenAI —y el que atraviesa OxideGate— lo devuelve como
    /// **cadena JSON**. Aceptar solo una de las dos formas haría que un modelo
    /// perfectamente capaz saliera como `ArgumentosRotos` según por dónde se
    /// le hablara.
    #[test]
    fn los_argumentos_como_cadena_json_tambien_valen() {
        let mensaje = json!({
            "role": "assistant",
            "tool_calls": [{
                "function": { "name": HERRAMIENTA, "arguments": "{\"ruta\": \"tarifa.py\"}" }
            }]
        });

        assert_eq!(clasificar(&mensaje), Veredicto::Emitida);
    }

    /// **El fallo exacto de #121 bajo el harness.** `tool_calls: null` y una
    /// pseudo-llamada en el texto, con un nombre sacado de la descripción de la
    /// herramienta. Se separa de `SoloTexto` porque el diagnóstico es distinto:
    /// aquí el modelo SÍ intentó el protocolo y la plantilla no lo emitió.
    #[test]
    fn la_llamada_escrita_en_el_texto_es_pseudollamada_no_solo_texto() {
        let mensaje = json!({
            "role": "assistant",
            "content": "brtc {\"name\": \"Ejecutauncomando\", \"arguments\": {\"cmd\": \"ls\"}}",
            "tool_calls": Value::Null
        });

        assert_eq!(clasificar(&mensaje), Veredicto::Pseudollamada);
    }

    /// La otra mitad de #121: el modelo alucinó **respuestas** de herramienta,
    /// inventando ficheros que no existían. También es protocolo escrito en la
    /// prosa, y también significa que el canal no se usó.
    #[test]
    fn una_respuesta_de_herramienta_alucinada_tambien_es_pseudollamada() {
        let mensaje = json!({
            "role": "assistant",
            "content": "<tool_response>\ndata.csv, main.py\n</tool_response>",
        });

        assert_eq!(clasificar(&mensaje), Veredicto::Pseudollamada);
    }

    /// **El falso positivo que hincharía el caso reparable.** Explicar en prosa
    /// lo que uno haría no es intentar el protocolo. Si esto contara como
    /// `Pseudollamada`, el informe diría «el modelo lo intenta, arregla la
    /// plantilla» sobre un modelo que no lo intentó nunca.
    #[test]
    fn la_prosa_que_menciona_argumentos_no_es_pseudollamada() {
        let mensaje = json!({
            "role": "assistant",
            "content": "Para eso llamaria a la funcion de lectura pasandole \
                        como argumentos la ruta del fichero.",
        });

        assert_eq!(clasificar(&mensaje), Veredicto::SoloTexto);
    }

    /// Un array de `tool_calls` vacío es lo mismo que no haberlo emitido. Si
    /// contara como llamada, un modelo mudo pasaría la criba.
    #[test]
    fn un_array_de_llamadas_vacio_no_es_una_llamada() {
        let mensaje = json!({ "role": "assistant", "content": "", "tool_calls": [] });

        assert_eq!(clasificar(&mensaje), Veredicto::SoloTexto);
    }

    /// Emitir por el canal correcto pero a una herramienta inventada no es
    /// saber usar herramientas: el harness recibiría un nombre que no existe.
    #[test]
    fn una_herramienta_que_nadie_declaro_es_nombre_inventado() {
        let mensaje = json!({
            "role": "assistant",
            "tool_calls": [{
                "function": { "name": "buscar_en_google", "arguments": { "q": "hola" } }
            }]
        });

        assert_eq!(clasificar(&mensaje), Veredicto::NombreInventado);
    }

    /// El nombre correcto con el argumento equivocado deja al harness sin la
    /// ruta: la llamada es sintácticamente válida e inútil.
    #[test]
    fn el_nombre_correcto_sin_el_argumento_pedido_son_argumentos_rotos() {
        let mensaje = json!({
            "role": "assistant",
            "tool_calls": [{
                "function": { "name": HERRAMIENTA, "arguments": { "fichero": FICHERO } }
            }]
        });

        assert_eq!(clasificar(&mensaje), Veredicto::ArgumentosRotos);
    }

    /// Una cadena que dice ser JSON y no lo es tampoco sirve. **Falla honesto**:
    /// no se adivina la ruta a partir del texto suelto.
    #[test]
    fn una_cadena_de_argumentos_que_no_parsea_son_argumentos_rotos() {
        let mensaje = json!({
            "role": "assistant",
            "tool_calls": [{
                "function": { "name": HERRAMIENTA, "arguments": "ruta=tarifa.py" }
            }]
        });

        assert_eq!(clasificar(&mensaje), Veredicto::ArgumentosRotos);
    }

    /// El encadenado pasa cuando la respuesta lleva el centinela, que solo se
    /// puede saber habiéndolo leído del resultado.
    #[test]
    fn una_respuesta_con_el_centinela_es_uso() {
        let mensaje = json!({
            "role": "assistant",
            "content": "La constante TARIFA_CACHE vale 7.3125.",
        });

        assert_eq!(clasificar_encadenado(&mensaje), Encadenado::Uso);
    }

    /// **El falso positivo que el centinela existe para evitar.** Un valor
    /// plausible inventado no es haber leído el resultado. Con un centinela
    /// adivinable, esta fila se contaría como «encadenó» mintiendo.
    #[test]
    fn una_respuesta_con_un_valor_inventado_es_ignoro() {
        let mensaje = json!({
            "role": "assistant",
            "content": "La constante TARIFA_CACHE vale 0.10, como es habitual.",
        });

        assert_eq!(clasificar_encadenado(&mensaje), Encadenado::Ignoro);
    }

    /// **El fallo del instrumento que le costó un 0/5 a `qwen3:14b`.** El
    /// modelo razonador deja el valor leído en `thinking` y entrega `content`
    /// vacío. Mirando solo `content`, esto se contaba como «ignoró el
    /// resultado» — culpando al modelo de algo que sí había hecho.
    #[test]
    fn el_centinela_en_thinking_con_content_vacio_no_es_ignoro() {
        let mensaje = json!({
            "role": "assistant",
            "content": "",
            "thinking": "El fichero dice TARIFA_CACHE = 7.3125, asi que ese es el valor.",
        });

        assert_eq!(
            clasificar_encadenado(&mensaje),
            Encadenado::PensoSinContestar
        );
    }

    /// Pero tampoco es `Uso`: **un harness consume `content`**. Si viene vacío,
    /// el agente se queda sin nada aunque el modelo lo supiera. Colapsarlo en
    /// `Uso` prometería una capacidad que el harness no llega a recibir.
    #[test]
    fn pensar_sin_contestar_no_cuenta_como_uso() {
        let mensaje = json!({
            "role": "assistant",
            "content": "   \n",
            "thinking": "vale 7.3125",
        });

        assert_ne!(clasificar_encadenado(&mensaje), Encadenado::Uso);
    }

    /// Si hubo respuesta entregada, ella manda: contestar ignorando un valor
    /// que uno mismo acaba de leer sigue siendo `Ignoro`, por mucho que el
    /// razonamiento lo mencionara. Lo que le llega al harness es el `content`.
    #[test]
    fn con_content_entregado_el_thinking_no_rescata_la_respuesta() {
        let mensaje = json!({
            "role": "assistant",
            "content": "La constante vale 0.10.",
            "thinking": "el fichero pone 7.3125",
        });

        assert_eq!(clasificar_encadenado(&mensaje), Encadenado::Ignoro);
    }

    /// Sin centinela en ninguno de los dos campos no hay nada que rescatar.
    #[test]
    fn sin_centinela_en_ningun_campo_es_ignoro() {
        let mensaje = json!({ "role": "assistant", "content": "", "thinking": "no lo se" });

        assert_eq!(clasificar_encadenado(&mensaje), Encadenado::Ignoro);
    }

    /// Volver a llamar no es usar el resultado, pero tampoco es inventárselo:
    /// es un bucle. Colapsarlo en `Ignoro` culparía al modelo de alucinar
    /// cuando lo que hace es no saber parar.
    #[test]
    fn volver_a_llamar_a_la_herramienta_se_cuenta_aparte() {
        let mensaje = json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "function": { "name": HERRAMIENTA, "arguments": { ARGUMENTO: FICHERO } }
            }]
        });

        assert_eq!(clasificar_encadenado(&mensaje), Encadenado::VolvioALlamar);
    }

    /// La herramienta declarada tiene que llevar el nombre y el argumento que
    /// la sonda va a exigir después. Si se desincronizaran, TODOS los modelos
    /// saldrían suspendidos por un fallo del instrumento.
    #[test]
    fn la_declaracion_lleva_el_nombre_y_el_argumento_que_se_exigen() {
        let h = herramientas();
        let texto = h.to_string();

        assert!(texto.contains(HERRAMIENTA));
        assert!(texto.contains(ARGUMENTO));
        assert_eq!(h[0]["type"], "function");
        assert_eq!(h[0]["function"]["name"], HERRAMIENTA);
    }

    /// Sin las dos anclas no hay escala: el techo da el máximo alcanzable y el
    /// suelo demuestra que el modelo no llama a ciegas. Un rango sin ellas no
    /// se puede interpretar.
    #[test]
    fn la_bateria_tiene_exactamente_un_techo_y_un_suelo() {
        let b = bateria();

        assert_eq!(b.iter().filter(|r| r.papel == Papel::Techo).count(), 1);
        assert_eq!(b.iter().filter(|r| r.papel == Papel::Suelo).count(), 1);
        assert!(
            b.iter().filter(|r| r.papel == Papel::Encargo).count() >= 2,
            "con menos de dos encargos no hay rango que medir"
        );
    }

    /// **El test que sostiene la medida entera.** Si un encargo nombrara la
    /// herramienta, esa fila mediría obediencia en vez de redacción, y el rango
    /// mezclaría dos cosas distintas. Solo el techo puede nombrarla, porque el
    /// techo ES la medida de la obediencia.
    #[test]
    fn solo_el_techo_nombra_la_herramienta() {
        for r in bateria().iter().filter(|r| r.papel != Papel::Techo) {
            assert!(
                !r.texto.contains(HERRAMIENTA),
                "la redaccion `{}` nombra la herramienta: mediria obediencia",
                r.nombre
            );
            assert!(
                !r.texto.to_lowercase().contains("herramienta"),
                "la redaccion `{}` menciona herramientas",
                r.nombre
            );
        }
    }

    /// **El confundidor que #29 existe para quitar.** Todos los encargos tienen
    /// que hablar del MISMO fichero y la MISMA avería: si variara la tarea, el
    /// rango mediría dificultad en vez de redacción.
    #[test]
    fn todos_los_encargos_hablan_del_mismo_fichero() {
        for r in bateria().iter().filter(|r| r.papel == Papel::Encargo) {
            assert!(
                r.texto.contains(FICHERO),
                "la redaccion `{}` no habla de {FICHERO}: seria otra tarea",
                r.nombre
            );
        }
    }

    /// El suelo no puede dar ninguna excusa para leer nada. Si nombrara el
    /// fichero dejaría de ser un suelo y el ancla inferior se caería.
    #[test]
    fn el_suelo_no_da_ninguna_razon_para_leer_un_fichero() {
        let suelo = bateria()
            .into_iter()
            .find(|r| r.papel == Papel::Suelo)
            .expect("hay suelo");

        assert!(!suelo.texto.contains(FICHERO));
        assert!(!suelo.texto.to_lowercase().contains("test"));
    }

    /// Ninguna redacción puede llevar la respuesta dentro: con el valor en el
    /// prompt no haría falta leer nada para contestarlo.
    #[test]
    fn ninguna_redaccion_revela_el_centinela() {
        for r in bateria() {
            assert!(!r.texto.contains(CENTINELA), "`{}` lo revela", r.nombre);
        }
    }

    /// **La lección que costó publicar un rango falso.** `n=5` daba un rango que
    /// se movía solo entre ejecuciones. El umbral tiene que quedar por encima de
    /// ese valor, o el aviso no protege de nada.
    #[test]
    fn el_umbral_publicable_deja_fuera_el_n_que_resulto_inestable() {
        assert!(
            N_MINIMO_PUBLICABLE > 5,
            "con n=5 el rango se movio de 1-5 a 3-5 sin cambiar nada"
        );
    }

    #[test]
    fn el_rango_va_del_minimo_al_maximo() {
        assert_eq!(sensibilidad(&[2, 5, 4, 3]), Some((2, 5)));
    }

    /// Sin varianza el rango es un punto, y eso es un resultado legítimo: dice
    /// que la redacción no movió nada en ese modelo.
    #[test]
    fn sin_varianza_el_rango_es_un_punto() {
        assert_eq!(sensibilidad(&[3, 3, 3]), Some((3, 3)));
    }

    /// **Falla honesto.** Sin encargos no hay rango, y devolver `(0, 0)` diría
    /// «no hay varianza» sobre algo que no se midió.
    #[test]
    fn sin_encargos_no_hay_rango_en_vez_de_un_cero_que_mentiria() {
        assert_eq!(sensibilidad(&[]), None);
    }

    fn medida_con(techo: usize, suelo: usize) -> Medida {
        let por_redaccion = bateria()
            .iter()
            .map(|r| {
                let mut c = Cuenta::default();
                c.emitida = match r.papel {
                    Papel::Techo => techo,
                    Papel::Suelo => suelo,
                    Papel::Encargo => 3,
                };
                c
            })
            .collect();
        Medida {
            modelo: "prueba".to_string(),
            por_redaccion,
            uso: 0,
            penso: 0,
            ignoro: 0,
            volvio: 0,
        }
    }

    #[test]
    fn con_las_dos_anclas_en_su_sitio_la_medida_vale() {
        assert!(anclas_validas(&medida_con(5, 0), 5).is_ok());
    }

    /// Sin techo no hay escala: el rango de abajo no sería comparable con nada.
    #[test]
    fn un_techo_que_no_emite_invalida_la_medida() {
        assert!(anclas_validas(&medida_con(0, 0), 5).is_err());
    }

    /// **El ancla que evita publicar ruido con formato de tabla.** Un modelo que
    /// llama a la herramienta ante un saludo casi tanto como ante el encargo no
    /// discrimina, y entonces sus tasas por redacción no distinguen nada.
    #[test]
    fn un_suelo_a_la_altura_del_encargo_invalida_la_medida() {
        // `medida_con` pone 3 en cada encargo: un suelo de 3 es el mismo nivel.
        assert!(anclas_validas(&medida_con(5, 3), 5).is_err());
    }

    /// **El fallo que hacía que medir mejor empeorase el veredicto.** Con la
    /// regla original (`suelo == 0`), `qwen2.5:7b` aprobaba a n=5 (0/5 ante un
    /// saludo) y suspendía a n=30 (1/30). Mismo modelo, distinto veredicto según
    /// cuánto midieras. Un ancla así no ancla nada.
    #[test]
    fn una_llamada_a_ciegas_suelta_no_invalida_un_modelo_que_discrimina() {
        let suelo = 1;
        let encargos = [20, 26, 25, 30];

        assert!(
            suelo_discrimina(suelo, &encargos),
            "1/30 contra encargos de 20-30/30 discrimina de sobra"
        );
    }

    /// El margen es el doble: justo en la mitad NO pasa. Sin margen estricto, un
    /// modelo a medio camino entre el saludo y el encargo se colaría.
    #[test]
    fn el_suelo_tiene_que_quedar_por_debajo_de_la_mitad_del_encargo_mas_flojo() {
        assert!(suelo_discrimina(9, &[20, 30]));
        assert!(!suelo_discrimina(10, &[20, 30]), "la mitad exacta no basta");
        assert!(!suelo_discrimina(15, &[20, 30]));
    }

    /// **Falla honesto.** Un modelo que no llama NUNCA —ni al encargo ni al
    /// saludo— no tiene rango que publicar: no hay nada que discriminar.
    #[test]
    fn un_modelo_que_no_llama_nunca_no_discrimina() {
        assert!(!suelo_discrimina(0, &[0, 0]));
    }

    /// Sin encargos no hay con qué comparar, así que solo vale un suelo limpio.
    #[test]
    fn sin_encargos_solo_vale_un_suelo_limpio() {
        assert!(suelo_discrimina(0, &[]));
        assert!(!suelo_discrimina(1, &[]));
    }
}
