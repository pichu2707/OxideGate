//! Detección del bloque de INSTRUCCIONES del usuario en el body: el fichero
//! que gobierna el comportamiento del agente (`CLAUDE.md`, `AGENTS.md`) tal
//! como lo inyecta cada harness.
//!
//! Es el hermano de [`super::skills`], y el hueco que venía a tapar es grande:
//! medido en `docs/fixed-toll-claude-code.md` §1, el `CLAUDE.md` es el **48%
//! del peaje fijo** de una sesión de Claude Code —el doble que todas las skills
//! juntas— y era el único de los tres bloques sin campo propio. Sus bytes caen
//! hoy dentro de `history_bytes`, mezclados con toda la conversación, y de ahí
//! no se pueden sacar.
//!
//! Importa porque el README publica una palanca YA MEDIDA sobre este bloque
//! —`CLAUDE.md` lean ⇒ −29.509 B/petición, la mayor del catálogo— y el proxy
//! no medía el objeto al que esa palanca se aplica.
//!
//! # Por qué el bloque se delimita por su ENVOLTORIO, no por su cabecera
//!
//! Medido en captura real del 2026-08-07 (Claude Code 2.1.220, cuerpo de
//! 188.180 B, coste cero) —una captura distinta a la del peaje fijo de
//! `docs/fixed-toll-claude-code.md` §1 (2026-07-31, cuerpo de 183.861 B),
//! aunque comparta versión de cliente—: el bloque viaja en `messages[0]`
//! envuelto en `<system-reminder>`, con la cabecera `# claudeMd` dentro.
//!
//! | Corte | Bytes |
//! |---|---:|
//! | `<system-reminder>`…`</system-reminder>` | **33.716** |
//! | desde `# claudeMd` hasta la siguiente cabecera `# ` | **8.254** |
//!
//! El segundo corte se para en `# Agent Teams Lite — Orchestrator Instructions`,
//! que es una cabecera **del propio `CLAUDE.md` del usuario**: mide el 24% y
//! parece un número. El contenido del bloque es markdown arbitrario escrito por
//! una persona, así que **ninguna cabecera puede servir de frontera**. Solo el
//! envoltorio del harness puede.
//!
//! # Por qué no basta con encontrar el envoltorio
//!
//! En el MISMO cuerpo capturado, `<system-reminder>` aparece en otros dos
//! sitios: `$.system[2].text` (9.588 B, **abierto y nunca cerrado**) y
//! `$.tools[0].description` (1.582 B, la descripción de la herramienta `Agent`
//! mencionando la etiqueta). Ninguno lleva `# claudeMd` dentro.
//!
//! Por eso un bloque **sólo cuenta si contiene la marca interna**, exactamente
//! el mismo criterio que hace que una mención de `<available_skills>` no sea un
//! listado ([`super::skills`]). Y por eso el recorrido va **cadena a cadena sin
//! concatenar**: uniendo `system[2]` con `messages[0]` se fabricaría un bloque
//! cerrado que en el cable no existe.
//!
//! # Ausencia honesta
//!
//! [`detect_instructions`] devuelve `None` cuando no reconoce ningún bloque, y
//! eso significa *"no se pudo ver"*, **nunca "el usuario no tiene
//! instrucciones"**. Dos casos reales lo hacen inevitable:
//!
//! - **Claude Code ignora `AGENTS.md`.** Un `None` con un `AGENTS.md` en el
//!   proyecto es CORRECTO, no un fallo del detector (`docs/skills-across-tools.md`
//!   §6): el mismo fichero, cuatro comportamientos.
//! - Las marcas son cadenas en inglés de cada harness. Si cambian, el detector
//!   deja de encontrarlas y debe declarar la ausencia, no fabricar un cero.
//! - **Si tu `CLAUDE.md` menciona literalmente `<system-reminder>`**, el bloque
//!   se salta y sale `None`; si menciona la etiqueta de cierre, la cifra sale
//!   corta. El contenido es texto libre del usuario, así que el recorrido se
//!   queda siempre del lado que mide de MENOS (ver [`super::block_scan`], y
//!   publicado en `docs/telemetry-per-request.md` §4.13).

use super::block_scan::primer_bloque_con;

/// Formato del bloque de instrucciones reconocido en el body.
///
/// La variante importa además de los bytes: dice de qué herramienta viene el
/// tráfico **sin depender del `User-Agent`**, que es contenido controlado por
/// el cliente.
///
/// **Cada variante entró con su propia captura del cable**, nunca desde la
/// tabla de `docs/skills-across-tools.md` §6: una marca es una cadena literal,
/// y añadirla sin recapturar sería inventar la medición que este módulo existe
/// para no inventar. La regla se ganó el sueldo dos veces: la marca documentada
/// de Codex **no existía en el binario**, y el zstd que se le atribuía a `pi`
/// resultó ser del **endpoint de Codex**, no del harness — una medición real a
/// la que le faltaba su condición.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
// Las cinco variantes acaban en `Md` y clippy sugiere quitar el sufijo. NO se
// hace: estos nombres se serializan con `rename_all` y son VALORES PUBLICADOS
// del campo `instructions.format` (`claude_md`, `codex_agents_md`,
// `opencode_agents_md`, `pi_agents_md`, `qwen_agents_md`), que consume
// `oxidegate-lens`. Renombrarlos por
// ergonomía interna rompería el contrato de un consumidor externo — y el sufijo
// no es ruido: nombra el fichero que cada harness inyecta.
#[allow(clippy::enum_variant_names)]
pub enum InstructionsFormat {
    /// Claude Code: `<system-reminder>` en `messages[0]` con la cabecera
    /// `# claudeMd` dentro. Medido: 33.716 B en una máquina real.
    ClaudeMd,
    /// Codex: la cabecera `# AGENTS.md instructions for <ruta absoluta>`
    /// seguida de un envoltorio `<INSTRUCTIONS>`…`</INSTRUCTIONS>`.
    ///
    /// Medido sobre captura real de **Codex 0.142.5** (2026-08-09): con un
    /// `AGENTS.md` de 202 B el bloque son **380 B**, o sea **178 B de
    /// envoltorio** — y **116 de esos 178 son la RUTA ABSOLUTA** del proyecto.
    /// El «+159 B» que circulaba es en realidad `62 B + longitud de la ruta`.
    ///
    /// **La marca documentada para Codex no existe.** La tabla decía
    /// `--- project-doc ---` dentro de `<INSTRUCTIONS>`; `grep` sobre la
    /// captura devuelve cero. Es el caso que justifica la regla de #66: ningún
    /// dialecto entra sin captura propia.
    CodexAgentsMd,
    /// opencode: la marca `Instructions from: <ruta absoluta>` en el prompt de
    /// sistema, con el contenido pegado detrás.
    ///
    /// Medido sobre captura real de **opencode 1.18.15** (2026-08-09): con un
    /// `AGENTS.md` de 202 B el bloque completo son **349 B**, o sea **147 B de
    /// envoltorio** — y **126 de esos 147 son la RUTA ABSOLUTA** del proyecto.
    ///
    /// Eso significa que el «+160 B» que circulaba para opencode **no es una
    /// constante**: es `~21 B + longitud de la ruta`. Un proyecto en `/home/u/p`
    /// paga bastante menos que uno en un directorio profundo, y ese número no
    /// se puede comparar entre máquinas sin decir dónde estaba el proyecto.
    /// Mismo hallazgo que con Codex.
    OpencodeAgentsMd,
    /// `pi`: el bloque `<project_instructions path="<ruta absoluta>">`, dentro
    /// del contenedor `<project_context>` del prompt de sistema.
    ///
    /// Medido sobre captura real de **`pi` 0.80.10** (2026-08-15): con un
    /// `AGENTS.md` de 202 B el bloque son **377 B**, o sea **175 B de
    /// envoltorio** — y **120 de esos 175 son la RUTA ABSOLUTA** del proyecto.
    /// El «+200 B lógicos» que circulaba es en realidad `55 B + longitud de la
    /// ruta`. Tercer dialecto seguido con el envoltorio dominado por la ruta
    /// (Codex 65%, opencode 86%, `pi` 69%).
    ///
    /// **La marca documentada para `pi` SÍ existe**, al contrario que la de
    /// Codex.
    ///
    /// # El zstd depende del PROVEEDOR, no del harness
    ///
    /// Se documentaba que `pi` manda el cuerpo comprimido con zstd y que es
    /// «el único de los cuatro» que lo hace, y de ahí que sus cifras fueran
    /// lógicas con un cable de ~1/3. **Esa medición era real, pero le falta la
    /// condición.** En esta captura —`pi` contra un proveedor
    /// `openai-completions`— el cuerpo viaja en **JSON plano**, así que estos
    /// 377 B son de CABLE.
    ///
    /// El motivo está en el código de `pi`: `zstd` aparece SOLO en
    /// `pi-ai/dist/api/openai-codex-responses.js`, y su propio comentario dice
    /// *«The Codex backend accepts zstd-compressed request bodies on the SSE
    /// responses endpoint (the same endpoint the official Codex client
    /// compresses against)»*. Es propiedad del **endpoint de Codex**, no del
    /// harness — y encima el transporte WebSocket manda JSON sin comprimir
    /// incluso ahí.
    ///
    /// Consecuencia para quien lea `bytes`: son lógicos o de cable **según a
    /// qué backend se enrute `pi`**. Con Codex detrás, `maybe_decompress` ya
    /// deshace el zstd antes de medir (ver [`super::maybe_decompress`]) y la
    /// cifra vuelve a ser lógica; con cualquier otro proveedor, coinciden.
    PiAgentsMd,
    /// Qwen Code: el bloque `--- Context from: AGENTS.md ---`…
    /// `--- End of Context from: AGENTS.md ---`.
    ///
    /// Medido sobre captura real de **Qwen Code 0.21.7** (2026-08-15): con un
    /// `AGENTS.md` de 202 B el bloque son **272 B**, o sea **70 B de
    /// envoltorio** (31 de apertura + 1 + 38 de cierre).
    ///
    /// **Es el único de los cuatro con un envoltorio de verdad constante.** Su
    /// ruta es **relativa**, así que no crece con la profundidad del
    /// directorio, frente al `62 B + ruta` de Codex, el `21 B + ruta` de
    /// opencode y el `55 B + ruta` de `pi`. Su cifra sí se puede comparar entre
    /// máquinas.
    ///
    /// # Por qué la marca lleva la ruta EXACTA
    ///
    /// En la captura real hay **TRES** bloques `Context from:` y el del
    /// proyecto es el **segundo**:
    ///
    /// ```text
    /// --- Context from: ../home/.qwen/AGENTS.md ---      (global)
    /// --- Context from: AGENTS.md ---                    (el del proyecto)
    /// --- Context from: .qwen/output-language.md ---     (config de Qwen)
    /// ```
    ///
    /// Coger el primero da el global. Y como la ruta del global **también acaba
    /// en `AGENTS.md`**, buscar por sufijo falla igual. La única marca que
    /// selecciona el fichero del proyecto es la ruta exacta `AGENTS.md`.
    ///
    /// # Lo que este número NO incluye
    ///
    /// El `AGENTS.md` **global** viaja en su propio bloque (176 B en la
    /// captura) y **no se suma aquí**. Es una diferencia real con
    /// [`Self::ClaudeMd`], donde el `CLAUDE.md` de proyecto y el global viajan
    /// concatenados dentro del MISMO `<system-reminder>` y por tanto sí se
    /// cuentan juntos. Sumar dos bloques disjuntos daría además un
    /// `by_heading` que no corresponde a ningún texto real.
    QwenAgentsMd,
}

/// Tope de filas con cabecera propia que publica [`desglosar`].
///
/// El contenido del bloque es entrada de terceros: nada impide un fichero con
/// diez mil cabeceras, y estas filas viven además en el buffer de 200 de
/// `/requests`, así que el coste se multiplica. Mismo espíritu que
/// `MAX_TOOL_SERVERS`.
///
/// **El desborde SIGUE contándose**: todo lo que pase del cupo colapsa en un
/// único bucket [`InstructionsHeadingKind::Others`], así que las filas siempre
/// suman el total exacto del bloque. Se pierde el desglose fino, nunca un byte.
///
/// 32 sobra de largo: el `CLAUDE.md` real con el que se midió #97 da **21
/// filas** a nivel ≤2.
pub const MAX_INSTRUCTIONS_HEADINGS: usize = 32;

/// Tope de bytes de cada nombre de cabecera publicado. Una cabecera real no se
/// acerca; una de 1 MB sería una entrada hostil, no un caso de uso. Mismo
/// motivo y mismo valor que `MAX_TOOL_NAME_LEN`.
pub const MAX_HEADING_LEN: usize = 128;

/// Qué representa una fila del desglose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionsHeadingKind {
    /// Todo lo que va ANTES de la primera cabecera: el envoltorio del harness
    /// (`<system-reminder>`, `# claudeMd`, la ruta absoluta de Codex…) y la
    /// prosa que el usuario haya puesto antes de titular nada.
    ///
    /// Existe para que la suma cuadre con [`InstructionsBlock::bytes`] sin
    /// tener que restar. Un fichero sin cabeceras da exactamente esta fila y
    /// nada más, que es la lectura correcta: «esto no está dividido».
    Preamble,
    /// Una sección abierta por una cabecera markdown de nivel 1 o 2.
    Heading,
    /// Bucket de desborde: todo lo que quedó pasado [`MAX_INSTRUCTIONS_HEADINGS`].
    Others,
}

/// Una fila del desglose interior del bloque de instrucciones.
///
/// # Qué es esta partición, y qué NO es
///
/// **Las fronteras del BLOQUE las pone el envoltorio del harness** — eso está
/// resuelto en los detectores de este módulo y no se toca. Esto reparte lo que
/// hay DENTRO, y lo reparte por las cabeceras markdown **del usuario**: es
/// «cómo lo organizó quien lo escribió», no una partición definida por el
/// harness ni por el proxy.
///
/// La distinción importa porque `docs/optimizer-claude-md.md` documenta que
/// cortar POR CABECERA para encontrar el FINAL del bloque midió 8.254 B de
/// 33.716 reales. Ese error fue usar el contenido para delimitar; aquí el
/// contenido solo se usa para repartir lo ya delimitado.
///
/// # No se publica el porcentaje
///
/// Un consumidor divide por [`InstructionsBlock::bytes`] y lo tiene. Publicar
/// la fracción ya cocinada añadiría un campo que puede desincronizarse con los
/// bytes sin que nada lo note — mismo criterio por el que `energy_idle_wh` se
/// publica al lado y no restado.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InstructionsHeading {
    /// Qué clase de fila es. Ver [`InstructionsHeadingKind`].
    pub kind: InstructionsHeadingKind,
    /// Nivel de la cabecera markdown (1 o 2). `None` en el preámbulo y en el
    /// bucket de desborde, que no salen de ninguna cabecera concreta.
    pub level: Option<u8>,
    /// Bytes de esta fila, cabecera incluida. La suma de todas las filas es
    /// EXACTAMENTE [`InstructionsBlock::bytes`].
    pub bytes: usize,
    /// Texto de la cabecera, recortado a [`MAX_HEADING_LEN`].
    ///
    /// **`None` salvo que la palanca esté puesta** — ver
    /// [`InstructionsBlock::publicable`]. Un nombre de herramienta ya viajaba
    /// al proveedor porque lo eligió el cliente; una cabecera de `CLAUDE.md`
    /// es texto libre de una persona, y puede llevar nombre de cliente, de
    /// proyecto o de cualquier cosa que no debería salir de la máquina.
    pub heading: Option<String>,
}

/// Reparte los bytes de `bloque` entre sus cabeceras markdown de nivel 1 y 2.
///
/// # Por qué el nivel 3 no corta
///
/// Medido sobre un `CLAUDE.md` real de 33.460 B: a nivel ≤2 salen **21 filas**
/// y las cuatro mayores son el **86,7%**. Bajar a ≤3 da 44 filas y **destruye
/// la señal** — «Model Assignments» pasa de 9.809 B (29,3%) a 1.705 B (5,1%)
/// porque su contenido se reparte entre hijos `###`. La fila que había que ver
/// deja de existir, y con ella la pregunta que el desglose viene a hacer
/// contestable: *¿esto que pago siempre, lo uso siempre?*
///
/// # Los fences
///
/// Una cabecera dentro de un bloque de código **no es una cabecera**: un
/// `CLAUDE.md` que documenta comandos lleva `# comentario` dentro de un fence,
/// y contarlo partiría la sección por un sitio que no existe. Se rastrean los
/// fences de tres backticks, que es la forma que usan los cuatro dialectos.
///
/// # La invariante
///
/// Las filas suman EXACTAMENTE `bloque.len()`. Lo sostiene el recorrido: se
/// avanza por líneas con `split_inclusive`, así que ningún salto de línea se
/// pierde por el camino, y cada corte es el offset donde empieza la siguiente.
fn desglosar(bloque: &str) -> Vec<InstructionsHeading> {
    // (offset donde empieza la línea, nivel, título ya recortado)
    let mut cortes: Vec<(usize, u8, String)> = Vec::new();
    // Offset de la PRIMERA cabecera rechazada por cupo: donde empieza el
    // bucket de desborde. `None` si nunca se agotó.
    let mut desborde: Option<usize> = None;
    let mut offset = 0usize;
    let mut en_fence = false;

    for linea in bloque.split_inclusive('\n') {
        let sin_sangria = linea.trim_end_matches(['\n', '\r']).trim_start();

        if sin_sangria.starts_with("```") {
            en_fence = !en_fence;
        } else if !en_fence {
            if let Some((nivel, titulo)) = cabecera(sin_sangria) {
                if cortes.len() < MAX_INSTRUCTIONS_HEADINGS {
                    cortes.push((offset, nivel, titulo));
                } else {
                    // Cupo agotado. Se anota dónde empieza el sobrante y se
                    // deja de mirar: lo único que falta saber es el offset, y
                    // seguir escaneando solo gastaría tiempo del request.
                    desborde = Some(offset);
                    break;
                }
            }
        }
        offset += linea.len();
    }

    let mut filas = Vec::with_capacity(cortes.len() + 2);
    // Frontera final de la última cabecera admitida: donde empieza el
    // desborde, o el final del bloque si no lo hubo.
    let tope = desborde.unwrap_or(bloque.len());

    // Preámbulo: del inicio a la primera cabecera. Sin cabeceras es el bloque
    // entero — y esa es la lectura correcta de un fichero sin titular.
    let primer_corte = cortes.first().map(|c| c.0).unwrap_or(bloque.len());
    if primer_corte > 0 {
        filas.push(InstructionsHeading {
            kind: InstructionsHeadingKind::Preamble,
            level: None,
            bytes: primer_corte,
            heading: None,
        });
    }

    for (i, (inicio, nivel, titulo)) in cortes.iter().enumerate() {
        let fin = cortes.get(i + 1).map(|c| c.0).unwrap_or(tope);
        filas.push(InstructionsHeading {
            kind: InstructionsHeadingKind::Heading,
            level: Some(*nivel),
            bytes: fin.saturating_sub(*inicio),
            heading: Some(titulo.clone()),
        });
    }

    if let Some(inicio) = desborde {
        filas.push(InstructionsHeading {
            kind: InstructionsHeadingKind::Others,
            level: None,
            bytes: bloque.len().saturating_sub(inicio),
            heading: None,
        });
    }

    filas
}

/// Reconoce una cabecera markdown de nivel 1 o 2 y devuelve `(nivel, título)`.
///
/// Exige el espacio tras las almohadillas: `#hashtag` no es una cabecera, y en
/// un fichero de instrucciones esa forma aparece de verdad.
fn cabecera(linea: &str) -> Option<(u8, String)> {
    let almohadillas = linea.len() - linea.trim_start_matches('#').len();
    if !(1..=2).contains(&almohadillas) {
        return None;
    }
    let resto = &linea[almohadillas..];
    let titulo = resto.strip_prefix(' ')?.trim();
    if titulo.is_empty() {
        return None;
    }
    Some((almohadillas as u8, recortar(titulo, MAX_HEADING_LEN)))
}

/// Recorta `s` a como mucho `max` BYTES sin partir un carácter.
///
/// El body es entrada de terceros: un `&s[..max]` a media secuencia UTF-8 sería
/// un `panic` en el camino crítico de la petición.
fn recortar(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let corte = (0..=max)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    s[..corte].to_string()
}

/// Bloque de instrucciones encontrado en el body de una petición.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InstructionsBlock {
    /// Bytes del bloque COMPLETO, envoltorio incluido. Es lo que se paga en
    /// CADA petición.
    ///
    /// **No es el tamaño del fichero en disco.** El harness añade su
    /// envoltorio, y sobre todo: el fichero de proyecto y el global viajan
    /// concatenados en el mismo bloque. Este número es lo que sube por el
    /// cable, que es lo que se factura.
    ///
    /// **Ni es el número del documento.** `docs/fixed-toll-claude-code.md` §1
    /// publica 33.718 B, pero es de OTRA captura: 2026-07-31, cuerpo completo
    /// de 183.861 B. Esta cifra es de la captura del 2026-08-07 (cuerpo de
    /// 188.180 B), y ahí sí está verificado: sobre ese mismo cuerpo, la parte
    /// de texto ENTERA mide 33.718 B y el bloque delimitado por las marcas
    /// mide 33.716 B —los 2 B de diferencia son el `\n\n` que queda fuera del
    /// cierre. Que la captura antigua dé la misma cifra que la parte de texto
    /// de la nueva es coincidencia, no una relación de 2 B entre los dos
    /// documentos. La cifra de aquí es la reproducible por un tercero, porque
    /// sus fronteras son visibles en el propio body.
    pub bytes: usize,
    /// Qué dialecto se reconoció.
    pub format: InstructionsFormat,
    /// Reparto de [`Self::bytes`] entre las cabeceras markdown del contenido.
    ///
    /// Las filas suman EXACTAMENTE `bytes`, siempre — ver [`desglosar`]. Nunca
    /// está vacío para un bloque reconocido: sin cabeceras sale una sola fila
    /// de preámbulo con el bloque entero.
    ///
    /// Los NOMBRES no viajan salvo que la palanca esté puesta. Ver
    /// [`Self::publicable`].
    pub by_heading: Vec<InstructionsHeading>,
}

impl InstructionsBlock {
    /// Devuelve el bloque listo para publicar, quitando los nombres de las
    /// cabeceras si `con_nombres` es `false`.
    ///
    /// # Por qué se filtra AQUÍ y no en el detector
    ///
    /// El detector es puro y no conoce la configuración. La decisión de qué
    /// sale por el cable la toma quien tiene el `AppConfig` delante —
    /// `Provider::prepare`— y la toma una vez, antes de que el bloque entre en
    /// `Outgoing`. A partir de ese punto ya no hay nombre que se pueda escapar
    /// ni a `telemetry.jsonl` ni al buffer de `/requests`.
    ///
    /// # Por qué el default es sin nombres
    ///
    /// Los tamaños no identifican nada; el texto sí puede. Una cabecera de
    /// `CLAUDE.md` la escribió una persona y puede llevar nombre de cliente o
    /// de proyecto. El precedente de `tool_names` no aplica: aquellos nombres
    /// los eligió el cliente y YA viajaban al proveedor en el mismo body, así
    /// que publicarlos no añadía exposición. Un nombre de sección viaja igual
    /// al proveedor, sí — pero también acaba en un fichero en disco y en un
    /// endpoint HTTP, que es donde alguien lo mira sin haberlo pedido.
    ///
    /// Quitar el nombre **no quita el dato**: los bytes, el nivel y la posición
    /// siguen ahí, y con el fichero delante se sabe qué fila es cuál.
    pub fn publicable(mut self, con_nombres: bool) -> Self {
        if !con_nombres {
            for fila in &mut self.by_heading {
                fila.heading = None;
            }
        }
        self
    }
}

/// Envoltorio del bloque en Claude Code, y la marca que lo distingue de
/// cualquier otro `<system-reminder>` del mismo cuerpo.
const CLAUDE_ABRE: &str = "<system-reminder>";
const CLAUDE_CIERRA: &str = "</system-reminder>";
const CLAUDE_MARCA: &str = "# claudeMd";

/// Marca con la que opencode abre el bloque. Los dos puntos y el espacio van
/// dentro a propósito: el contenido es markdown de una persona y puede
/// mencionar la palabra `Instructions` sin ser una marca.
const OPENCODE_MARCA: &str = "Instructions from: ";

/// Lo único que hay detrás del bloque de opencode, y por tanto su única
/// frontera final: el preámbulo de su bloque de skills.
///
/// **Es prosa del harness, y eso es una fragilidad conocida.** Si opencode
/// cambia esa frase, este detector deja de encontrar el final y publica `None`
/// — que significa «no lo reconozco», no «no hay instrucciones». Falla honesto,
/// no falla mintiendo, que es lo que decide el diseño de abajo.
const OPENCODE_FIN: &str = "Skills provide specialized instructions";

/// Busca el bloque de instrucciones en un texto plano del body.
///
/// Devuelve `None` si no reconoce ningún dialecto, o si lo que encuentra es un
/// envoltorio sin la marca interna.
///
/// **El orden no es casual: mandan los que cierran de verdad.** Claude Code
/// (`<system-reminder>`…`</system-reminder>`), Codex (`<INSTRUCTIONS>`…
/// `</INSTRUCTIONS>`), `pi` (`<project_instructions>`…`</project_instructions>`)
/// y Qwen (`--- Context from: AGENTS.md ---`…`--- End of Context… ---`)
/// delimitan su bloque con apertura Y cierre reales. **opencode va el ÚLTIMO**
/// porque es el único que solo tiene apertura y su final hay que deducirlo de
/// una frase en prosa. Ante un cuerpo donde varios pudieran aparecer, gana el
/// que se puede delimitar con certeza.
pub fn detect_instructions(texto: &str) -> Option<InstructionsBlock> {
    detect_claude_md(texto)
        .or_else(|| detect_codex(texto))
        .or_else(|| detect_pi(texto))
        .or_else(|| detect_qwen(texto))
        .or_else(|| detect_opencode(texto))
}

/// Cabecera con la que Codex identifica el bloque. Va FUERA del envoltorio, y
/// lleva el `# ` y el ` for ` a propósito: el prompt de sistema del propio
/// Codex menciona «AGENTS.md instructions» sin ser el bloque —verificado en la
/// captura: *«…take precedence over AGENTS.md instructions.»*—.
const CODEX_CABECERA: &str = "# AGENTS.md instructions for ";
const CODEX_ABRE: &str = "<INSTRUCTIONS>";
const CODEX_CIERRA: &str = "</INSTRUCTIONS>";

/// Codex: cabecera identificadora seguida del envoltorio `<INSTRUCTIONS>`.
///
/// Se mide **desde la cabecera**, no desde `<INSTRUCTIONS>`: la ruta absoluta
/// vive ahí y son **116 de los 178 B de envoltorio** en la captura real.
/// Cortar en el envoltorio dejaría fuera el 65% de lo que cuesta.
///
/// A diferencia de opencode, Codex **sí cierra** el bloque, así que la frontera
/// final es real y no hay que adivinarla. Sin `</INSTRUCTIONS>` no es su forma
/// y se devuelve `None` en vez de inventar un final.
fn detect_codex(texto: &str) -> Option<InstructionsBlock> {
    let inicio = texto.find(CODEX_CABECERA)?;
    let abre = texto[inicio..].find(CODEX_ABRE)? + inicio;
    let cierra = texto[abre..].find(CODEX_CIERRA)? + abre + CODEX_CIERRA.len();
    let bloque = &texto[inicio..cierra];
    Some(InstructionsBlock {
        bytes: bloque.len(),
        format: InstructionsFormat::CodexAgentsMd,
        by_heading: desglosar(bloque),
    })
}

/// Apertura del bloque de `pi`. Lleva el `path="` a propósito: el prompt de
/// sistema de `pi` y su documentación hablan de sus propias etiquetas, y una
/// mención suelta del nombre no puede abrir un bloque.
const PI_ABRE: &str = "<project_instructions path=\"";
const PI_CIERRA: &str = "</project_instructions>";

/// `pi`: el bloque `<project_instructions path="…">`, con la ruta absoluta en
/// la apertura y un cierre real.
///
/// # Dónde se corta, y por qué no en el contenedor
///
/// El bloque vive dentro de `<project_context>`, que en la captura real añade
/// **86 B más** (463 B frente a 377 B). Ese contenedor **queda fuera**: es
/// genérico —su nombre no dice «instructions»— y su preámbulo es prosa del
/// harness, no algo atribuible al fichero del usuario. La frontera honesta es
/// el bloque que nombra la ruta.
///
/// Se mide **desde `<project_instructions`**, no desde el `>` de la apertura:
/// la ruta absoluta vive ahí y son **120 de los 175 B de envoltorio**. Cortar
/// después dejaría fuera el 69% de lo que cuesta.
///
/// Como Codex —y a diferencia de opencode— `pi` **sí cierra**, así que la
/// frontera final es real y no hay que adivinarla. Sin `</project_instructions>`
/// no es su forma y se devuelve `None` en vez de inventar un final.
///
/// # Un solo bloque
///
/// `pi` descubre `AGENTS.md` **y** `CLAUDE.md`, así que cabía esperar dos
/// bloques dentro del contenedor. Verificado en el cable con los dos ficheros
/// presentes: emite **uno solo**, el de `AGENTS.md`. Por eso se mide el primero
/// y no se suma una lista.
fn detect_pi(texto: &str) -> Option<InstructionsBlock> {
    let inicio = texto.find(PI_ABRE)?;
    let cierra = texto[inicio..].find(PI_CIERRA)? + inicio + PI_CIERRA.len();
    let bloque = &texto[inicio..cierra];
    Some(InstructionsBlock {
        bytes: bloque.len(),
        format: InstructionsFormat::PiAgentsMd,
        by_heading: desglosar(bloque),
    })
}

/// Marca de Qwen Code. Lleva la ruta **exacta** `AGENTS.md`, no un sufijo: en
/// la captura real conviven tres bloques `Context from:` y el del `AGENTS.md`
/// global —que también acaba en `AGENTS.md`— va ANTES que el del proyecto.
const QWEN_ABRE: &str = "--- Context from: AGENTS.md ---";
const QWEN_CIERRA: &str = "--- End of Context from: AGENTS.md ---";

/// Qwen Code: el bloque `--- Context from: AGENTS.md ---` con su cierre real.
///
/// Envoltorio **fijo de 70 B** y ruta relativa: el único de los cuatro cuya
/// cifra no depende de dónde tengas el proyecto.
///
/// Abre y cierra de verdad, así que la frontera final no se adivina. Sin
/// `--- End of Context from: AGENTS.md ---` no es su forma y se devuelve `None`.
///
/// **No suma el `AGENTS.md` global**, que viaja en su propio bloque. Ver
/// [`InstructionsFormat::QwenAgentsMd`] para por qué, y para la diferencia con
/// `claude_md`.
fn detect_qwen(texto: &str) -> Option<InstructionsBlock> {
    let inicio = texto.find(QWEN_ABRE)?;
    let cierra = texto[inicio..].find(QWEN_CIERRA)? + inicio + QWEN_CIERRA.len();
    let bloque = &texto[inicio..cierra];
    Some(InstructionsBlock {
        bytes: bloque.len(),
        format: InstructionsFormat::QwenAgentsMd,
        by_heading: desglosar(bloque),
    })
}

/// opencode: `Instructions from: <ruta>` y el contenido pegado detrás, hasta
/// donde empieza el preámbulo de skills.
///
/// # La frontera, que es todo el problema
///
/// Verificado sobre captura real de opencode 1.18.15: el harness **abre** el
/// bloque con la marca y **no lo cierra**. Lo único que hay detrás es su
/// bloque de skills:
///
/// ```text
/// </env>
/// Instructions from: /ruta/absoluta/AGENTS.md
/// …contenido del AGENTS.md…
///
/// Skills provide specialized instructions and workflows for specific tasks.
/// ```
///
/// Es la tercera vez que aparece esta forma —también en los hooks de Claude
/// Code— y la respuesta es la misma: **si no se encuentra la frontera, `None`.**
/// Correr hasta el final del texto se tragaría el listado de skills entero, que
/// es exactamente el error que `docs/fixed-toll-claude-code.md` §4 documenta
/// tras cometerlo dos veces. Un `null` es honesto; un número inflado no.
fn detect_opencode(texto: &str) -> Option<InstructionsBlock> {
    let inicio = texto.find(OPENCODE_MARCA)?;
    let fin = texto[inicio..].find(OPENCODE_FIN)? + inicio;
    let bloque = &texto[inicio..fin];
    Some(InstructionsBlock {
        bytes: bloque.len(),
        format: InstructionsFormat::OpencodeAgentsMd,
        by_heading: desglosar(bloque),
    })
}

/// Claude Code: primer `<system-reminder>` que contenga la cabecera
/// `# claudeMd`.
fn detect_claude_md(texto: &str) -> Option<InstructionsBlock> {
    primer_bloque_con(texto, CLAUDE_ABRE, CLAUDE_CIERRA, CLAUDE_MARCA).map(|(bloque, _)| {
        InstructionsBlock {
            bytes: bloque.len(),
            format: InstructionsFormat::ClaudeMd,
            by_heading: desglosar(bloque),
        }
    })
}

/// Busca el bloque de instrucciones en un body completo, sea cual sea el
/// dialecto.
///
/// Se recorre el JSON entero en vez de aplicar una regla por proveedor: cada
/// harness lo pone en un sitio distinto del body, y el recorrido no tiene por
/// qué saberlo.
///
/// Se examina **cadena a cadena, sin concatenar**: unir textos podría formar
/// un bloque a caballo entre dos campos que en el body no existe — y en este
/// cuerpo concreto pasaría, porque `$.system[2].text` deja un
/// `<system-reminder>` abierto sin cerrar.
pub fn detect_instructions_in_body(body: &serde_json::Value) -> Option<InstructionsBlock> {
    match body {
        serde_json::Value::String(s) => detect_instructions(s),
        serde_json::Value::Array(xs) => xs.iter().find_map(detect_instructions_in_body),
        serde_json::Value::Object(o) => o.values().find_map(detect_instructions_in_body),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduce la FORMA medida en la captura real: envoltorio, cabecera
    /// `# claudeMd`, contenido de usuario que incluye una cabecera `# ` de
    /// nivel 1 (la trampa), y las secciones hermanas del final.
    fn bloque_como_el_real() -> String {
        format!(
            "{CLAUDE_ABRE}\n\
             As you answer the user's questions, you can use the following context:\n\
             {CLAUDE_MARCA}\n\
             Codebase and user instructions are shown below.\n\n\
             ## Rules\n\n- Never add AI attribution to commits.\n\n\
             # Agent Teams Lite — Orchestrator Instructions\n\n\
             You are a COORDINATOR, not an executor.\n\
             # currentDate\n\
             Today's date is 2026-08-07.\n\
             {CLAUDE_CIERRA}"
        )
    }

    /// GUARDA DE FORMA de `InstructionsBlock` (`instructions` en `GET /requests`).
    ///
    /// `FIELDS` declara `instructions`, así que el NOMBRE del objeto está
    /// cubierto por el test recursivo de `/version`. Sus dos claves internas no
    /// lo estarían: el snapshot de contrato solo congela el primer nivel de la
    /// fila. Mismo razonamiento que en `skills`.
    #[test]
    fn la_forma_de_instructions_no_cambia_sin_querer() {
        let v = serde_json::to_value(InstructionsBlock {
            bytes: 33_716,
            format: InstructionsFormat::ClaudeMd,
            by_heading: Vec::new(),
        })
        .expect("serializa");

        let claves: std::collections::BTreeSet<&str> = v
            .as_object()
            .expect("objeto")
            .keys()
            .map(String::as_str)
            .collect();

        assert_eq!(
            claves,
            ["bytes", "format", "by_heading"]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "cambió la forma de instructions. Si es ADITIVO, actualiza esta \
             lista. Si RENOMBRA, QUITA o cambia el tipo de una clave, sube \
             además CONTRACT_VERSION en middleware::version y anótalo en \
             docs/telemetry-per-request.md §8"
        );
    }

    // --- Codex: cabecera + `<INSTRUCTIONS>`…`</INSTRUCTIONS>` ---

    /// Reproduce la disposición REAL capturada de Codex 0.142.5: la cabecera
    /// identificadora FUERA del envoltorio, y el contenido dentro.
    fn parte_de_codex(contenido: &str) -> String {
        format!(
            "# AGENTS.md instructions for /home/quien/proyecto\n\n\
             <INSTRUCTIONS>\n{contenido}\n</INSTRUCTIONS>"
        )
    }

    #[test]
    fn reconoce_el_bloque_de_codex() {
        let texto = parte_de_codex("# Instrucciones\n\nResponde en una linea.");

        let b = detect_instructions(&texto).expect("debe reconocer el bloque");

        assert_eq!(b.format, InstructionsFormat::CodexAgentsMd);
        assert_eq!(
            b.bytes,
            texto.len(),
            "mide de la CABECERA al cierre: la cabecera también se paga"
        );
    }

    /// La cabecera va FUERA del envoltorio, así que el bloque no puede medirse
    /// solo de `<INSTRUCTIONS>` a `</INSTRUCTIONS>`: se dejaría fuera la ruta
    /// absoluta, que en la captura real son 116 de los 178 B de envoltorio.
    #[test]
    fn la_cabecera_de_codex_entra_en_la_cuenta() {
        let texto = parte_de_codex("x");
        let solo_envoltorio = texto.find("<INSTRUCTIONS>").expect("hay envoltorio");

        let b = detect_instructions(&texto).expect("reconoce");

        assert!(
            b.bytes > texto.len() - solo_envoltorio,
            "sin la cabecera se perdería la ruta: {} vs {}",
            b.bytes,
            texto.len() - solo_envoltorio
        );
    }

    /// **El prompt de sistema de Codex MENCIONA `AGENTS.md instructions`** sin
    /// ser el bloque —verificado en la captura real: «…take precedence over
    /// AGENTS.md instructions.»—. La marca lleva el `# ` y el ` for ` a
    /// propósito para no morder ahí.
    #[test]
    fn la_mencion_del_prompt_de_sistema_no_abre_un_bloque() {
        let texto = "Direct instructions take precedence over AGENTS.md instructions.\n\
                     The contents of the AGENTS.md file at the root of the repo…";

        assert!(detect_instructions(texto).is_none());
    }

    /// Sin cierre no hay bloque. Codex SÍ cierra —a diferencia de opencode—
    /// así que un cuerpo sin `</INSTRUCTIONS>` no es su forma y no se adivina.
    #[test]
    fn sin_cierre_el_bloque_de_codex_no_se_adivina() {
        let texto = "# AGENTS.md instructions for /x\n\n<INSTRUCTIONS>\ncontenido sin cerrar";

        assert!(detect_instructions(texto).is_none());
    }

    // --- opencode: `Instructions from: <ruta>` ---

    /// Reproduce la disposición REAL capturada de opencode 1.18.15: la marca
    /// tras `</env>`, el contenido pegado, y detrás el preámbulo de skills sin
    /// ninguna marca de cierre en medio.
    fn parte_de_opencode(contenido: &str) -> String {
        format!(
            "...prompt de sistema...\n<env>\n  Platform: linux\n</env>\n\
             Instructions from: /home/quien/proyecto/AGENTS.md\n{contenido}\n\n\
             Skills provide specialized instructions and workflows for specific tasks.\n\
             Use the skill tool to load a skill.\n<available_skills>\n</available_skills>"
        )
    }

    #[test]
    fn reconoce_el_bloque_de_opencode() {
        let contenido = "# Instrucciones\n\nResponde en una linea.";
        let texto = parte_de_opencode(contenido);

        let b = detect_instructions(&texto).expect("debe reconocer el bloque");

        assert_eq!(b.format, InstructionsFormat::OpencodeAgentsMd);
        // De la marca al inicio del preámbulo de skills: cabecera + ruta +
        // contenido + el `\n\n` que los separa, ni un byte más.
        let esperado = "Instructions from: /home/quien/proyecto/AGENTS.md\n".len()
            + contenido.len()
            + "\n\n".len();
        assert_eq!(b.bytes, esperado);
    }

    /// **FALLA CERRADO.** opencode abre el bloque con una marca y NO lo cierra:
    /// lo único que hay detrás es el preámbulo de skills. Sin esa frontera,
    /// correr hasta el final se tragaría el listado entero de skills — el mismo
    /// error que ya se cometió dos veces en este proyecto y que documenta
    /// `fixed-toll-claude-code.md` §4.
    #[test]
    fn sin_la_frontera_de_skills_no_se_adivina_el_final() {
        let texto = "Instructions from: /x/AGENTS.md\n# algo\n\ny nada más detrás";

        assert!(
            detect_instructions(texto).is_none(),
            "sin frontera, `null` — que significa «no lo reconozco», no «no hay instrucciones»"
        );
    }

    /// El contenido es markdown de una persona y puede mencionar cualquier
    /// cosa, incluida la palabra `Instructions`. La marca lleva sus dos
    /// puntos y el espacio a propósito.
    #[test]
    fn una_mencion_en_el_contenido_no_abre_un_bloque() {
        let texto =
            parte_de_opencode("Aqui hablo de Instructions y de AGENTS.md sin ser una marca.");

        let b = detect_instructions(&texto).expect("reconoce el bloque de verdad");

        assert!(
            b.bytes < texto.len(),
            "no puede tragarse la parte entera: {} de {}",
            b.bytes,
            texto.len()
        );
    }

    // --- pi: `<project_instructions path="…">` dentro de `<project_context>` ---

    /// Reproduce la disposición REAL capturada de `pi` 0.80.10: el contenedor
    /// `<project_context>` con su preámbulo en prosa, el bloque con la ruta
    /// absoluta EN LA APERTURA, y el `cwd` detrás del contenedor.
    fn parte_de_pi(contenido: &str) -> String {
        format!(
            "...prompt de sistema de pi...\n\n\
             <project_context>\n\n\
             Project-specific instructions and guidelines:\n\n\
             <project_instructions path=\"/home/quien/proyecto/AGENTS.md\">\n\
             {contenido}\n\
             </project_instructions>\n\n\
             </project_context>\n\n\
             Current working directory: /home/quien/proyecto"
        )
    }

    #[test]
    fn reconoce_el_bloque_de_pi() {
        let contenido = "# Instrucciones\n\nResponde en una linea.";
        let texto = parte_de_pi(contenido);

        let b = detect_instructions(&texto).expect("debe reconocer el bloque");

        assert_eq!(b.format, InstructionsFormat::PiAgentsMd);
        // De la apertura CON la ruta al cierre real, ni un byte más.
        let esperado = "<project_instructions path=\"/home/quien/proyecto/AGENTS.md\">\n".len()
            + contenido.len()
            + "\n</project_instructions>".len();
        assert_eq!(b.bytes, esperado);
    }

    /// La ruta absoluta vive DENTRO de la apertura, y es lo que domina el
    /// envoltorio: en la captura real son **120 de los 175 B**, un 69%. Medir
    /// desde `>` en vez de desde `<project_instructions` tiraría ese 69% —
    /// mismo error que ya se evitó en Codex (116 de 178) y opencode (126 de 147).
    #[test]
    fn la_ruta_de_pi_entra_en_la_cuenta() {
        const RUTA: &str = "/home/quien/proyecto/AGENTS.md";
        let contenido = "x";
        let texto = parte_de_pi(contenido);

        let b = detect_instructions(&texto).expect("reconoce");

        // Lo que se paga de más por tener el fichero, con este contenido.
        let envoltorio = b.bytes - contenido.len();
        assert_eq!(
            envoltorio - RUTA.len(),
            55,
            "el envoltorio de pi es `55 B FIJOS + la ruta`, no una constante: \
             en la captura real la ruta eran 120 de 175 B (69%)"
        );
    }

    /// **El contenedor `<project_context>` queda FUERA a propósito.** En la
    /// captura real añade 86 B más (bloque 463 B frente a 377 B), pero es un
    /// contenedor genérico —su nombre no dice «instructions»— y su preámbulo en
    /// prosa no es atribuible al fichero del usuario. La frontera honesta es el
    /// bloque que SÍ nombra la ruta, que además abre y cierra de verdad.
    #[test]
    fn el_contenedor_de_pi_queda_fuera_de_la_cuenta() {
        let texto = parte_de_pi("# algo");
        let contenedor = texto.find("<project_context>").expect("hay contenedor");
        let fin_contenedor =
            texto.find("</project_context>").expect("cierra") + "</project_context>".len();

        let b = detect_instructions(&texto).expect("reconoce");

        assert!(
            b.bytes < fin_contenedor - contenedor,
            "no puede tragarse el contenedor entero: {} vs {}",
            b.bytes,
            fin_contenedor - contenedor
        );
    }

    /// Sin cierre no hay bloque. `pi` SÍ cierra —como Codex y a diferencia de
    /// opencode— así que un cuerpo sin `</project_instructions>` no es su forma
    /// y no se adivina el final.
    #[test]
    fn sin_cierre_el_bloque_de_pi_no_se_adivina() {
        let texto = "<project_context>\n<project_instructions path=\"/x/AGENTS.md\">\nsin cerrar";

        assert!(detect_instructions(texto).is_none());
    }

    /// La marca lleva el atributo `path="` a propósito. El contenido es markdown
    /// de una persona —y la documentación de `pi` habla de `custom-provider.md`
    /// y de sus propias etiquetas—, así que una mención suelta del nombre no
    /// puede abrir un bloque.
    #[test]
    fn una_mencion_de_project_instructions_no_abre_un_bloque() {
        let texto = "El harness envuelve el fichero en project_instructions, \
                     dentro de <project_context>, y no lo comprime.";

        assert!(detect_instructions(texto).is_none());
    }

    // --- Qwen Code: `--- Context from: AGENTS.md ---` ---

    /// Reproduce la disposición REAL capturada de Qwen Code 0.21.7: **TRES**
    /// bloques `Context from:`, con el del proyecto en MEDIO. El primero es el
    /// `AGENTS.md` global —cuya ruta también acaba en `AGENTS.md`— y el último
    /// es un fichero de configuración del propio Qwen.
    ///
    /// `contenido` entra **VERBATIM**: en el cable es el fichero tal cual, y un
    /// fichero de texto acaba en `\n`. Ese salto es lo que separa el contenido
    /// del cierre — **no lo pone el harness**. Por eso el envoltorio son 70 B
    /// (31 de apertura + el `\n` de después + 38 de cierre) y no 71.
    fn parte_de_qwen(contenido: &str) -> String {
        format!(
            "...prompt de sistema de qwen...\n\n\
             --- Context from: ../home/.qwen/AGENTS.md ---\n\
             # Contexto global\n\
             --- End of Context from: ../home/.qwen/AGENTS.md ---\n\n\
             --- Context from: AGENTS.md ---\n\
             {contenido}\
             --- End of Context from: AGENTS.md ---\n\n\
             --- Context from: .qwen/output-language.md ---\n\
             Responde siempre en espanol neutro.\n\
             --- End of Context from: .qwen/output-language.md ---\n\n\
             ---\n\n# auto memory\n"
        )
    }

    #[test]
    fn reconoce_el_bloque_de_qwen() {
        let contenido = "# Instrucciones\n\nResponde en una linea.\n";
        let texto = parte_de_qwen(contenido);

        let b = detect_instructions(&texto).expect("debe reconocer el bloque");

        assert_eq!(b.format, InstructionsFormat::QwenAgentsMd);
        let esperado = "--- Context from: AGENTS.md ---\n".len()
            + contenido.len()
            + "--- End of Context from: AGENTS.md ---".len();
        assert_eq!(b.bytes, esperado);
    }

    /// **El test que decide el detector.** En la captura real hay TRES bloques
    /// `Context from:` y el del proyecto es el SEGUNDO. Coger el primero da el
    /// `AGENTS.md` global; y como la ruta de ese global **también acaba en
    /// `AGENTS.md`**, buscar por sufijo falla igual. La marca tiene que ser la
    /// ruta EXACTA `AGENTS.md`, que es la del fichero en el directorio actual.
    #[test]
    fn el_agents_md_global_de_qwen_no_gana_al_del_proyecto() {
        let contenido = "contenido del proyecto\n";
        let texto = parte_de_qwen(contenido);

        let b = detect_instructions(&texto).expect("reconoce");

        let bloque = &texto[texto
            .find("--- Context from: AGENTS.md ---")
            .expect("existe")..];
        assert!(
            bloque[..b.bytes].contains(contenido),
            "midió el bloque equivocado: no contiene el contenido del proyecto"
        );
        assert!(
            !bloque[..b.bytes].contains("Contexto global"),
            "se tragó el bloque global"
        );
    }

    /// Qwen es el ÚNICO de los cuatro cuyo envoltorio es de verdad constante:
    /// su ruta es **relativa**, así que no crece con la profundidad del
    /// directorio. 70 B medidos en la captura real (31 de apertura + 1 + 38 de
    /// cierre), frente a los `55/62/21 B + ruta` de `pi`, Codex y opencode.
    #[test]
    fn el_envoltorio_de_qwen_es_de_verdad_fijo() {
        let contenido = "x\n";
        let texto = parte_de_qwen(contenido);

        let b = detect_instructions(&texto).expect("reconoce");

        assert_eq!(
            b.bytes - contenido.len(),
            70,
            "70 B FIJOS: la ruta es relativa y no infla el envoltorio"
        );
    }

    /// Sin cierre no hay bloque. Qwen SÍ cierra, así que un cuerpo sin
    /// `--- End of Context from: AGENTS.md ---` no es su forma y no se adivina.
    #[test]
    fn sin_cierre_el_bloque_de_qwen_no_se_adivina() {
        let texto = "--- Context from: AGENTS.md ---\ncontenido sin cerrar\n\n# otra cosa";

        assert!(detect_instructions(texto).is_none());
    }

    /// Claude Code gana cuando los dos dialectos podrían aparecer: su bloque
    /// está delimitado por un envoltorio con cierre real, así que es el más
    /// fiable de los dos. El orden de `detect_instructions` no es casual.
    #[test]
    fn el_dialecto_con_cierre_real_tiene_precedencia() {
        let texto = format!("{}\n{}", bloque_como_el_real(), parte_de_opencode("# otro"));

        let b = detect_instructions(&texto).expect("reconoce alguno");

        assert_eq!(b.format, InstructionsFormat::ClaudeMd);
    }

    /// El bloque se mide de marca a marca, envoltorio incluido.
    #[test]
    fn reconoce_el_bloque_de_claude_code() {
        let texto = format!("ruido antes\n{}\n\ny ruido después", bloque_como_el_real());

        let b = detect_instructions(&texto).expect("debe reconocer el bloque");

        assert_eq!(b.format, InstructionsFormat::ClaudeMd);
        assert_eq!(
            b.bytes,
            bloque_como_el_real().len(),
            "mide de <system-reminder> a </system-reminder>, ni un byte más"
        );
    }

    /// **El test que justifica el diseño entero.** Cortar en la siguiente
    /// cabecera `# ` medía 8.254 B de 33.716 reales en la captura: se paraba en
    /// una cabecera del `CLAUDE.md` DEL USUARIO. El contenido del bloque es
    /// markdown de una persona, así que ninguna cabecera puede ser frontera.
    #[test]
    fn no_corta_en_una_cabecera_del_contenido_del_usuario() {
        let bloque = bloque_como_el_real();
        let b = detect_instructions(&bloque).expect("debe reconocer el bloque");

        let hasta_la_cabecera = bloque
            .find("\n# Agent Teams Lite")
            .expect("la trampa está en el fixture");

        assert!(
            b.bytes > hasta_la_cabecera,
            "se cortó en una cabecera del usuario: {} B de {} reales",
            b.bytes,
            bloque.len()
        );
    }

    /// Los otros dos `<system-reminder>` del MISMO cuerpo capturado: uno
    /// abierto y nunca cerrado (`$.system[2].text`), otro dentro de la
    /// descripción de una herramienta (`$.tools[0].description`). Sin la marca
    /// interna, ninguno es el bloque de instrucciones.
    #[test]
    fn un_envoltorio_sin_la_marca_no_es_el_bloque() {
        let sin_cerrar =
            format!("{CLAUDE_ABRE}\nEste recordatorio nunca se cierra y no lleva la marca.");
        let mencion = format!(
            "El texto puede aparecer envuelto en un {CLAUDE_ABRE} … {CLAUDE_CIERRA} cualquiera."
        );

        assert!(detect_instructions(&sin_cerrar).is_none());
        assert!(detect_instructions(&mencion).is_none());
    }

    /// Mención primero, bloque real después: hay que saltarse la mención y
    /// quedarse con el bueno, no rendirse en el primer intento.
    #[test]
    fn encuentra_el_bloque_real_aunque_haya_menciones_antes() {
        let texto = format!(
            "Habla del {CLAUDE_ABRE} sin marca dentro {CLAUDE_CIERRA}.\nY más abajo:\n{}",
            bloque_como_el_real()
        );

        let b = detect_instructions(&texto).expect("debe saltarse la mención");

        assert_eq!(b.bytes, bloque_como_el_real().len());
    }

    /// Sin ninguna marca, `None` significa "no se pudo ver", nunca "el usuario
    /// no tiene instrucciones". Es el contrato del resto de la telemetría, y
    /// aquí además es el caso REAL de Claude Code con un `AGENTS.md`: lo ignora.
    #[test]
    fn sin_marcas_devuelve_none_y_no_un_cero_fabricado() {
        assert!(detect_instructions("un body cualquiera sin instrucciones").is_none());
        assert!(detect_instructions("").is_none());
        assert!(
            detect_instructions("# AGENTS.md\n\nResponde en español.").is_none(),
            "Claude Code no manda AGENTS.md: None es la respuesta correcta, no un 0"
        );
    }

    /// El bloque se encuentra esté donde esté, y los tipos que no son texto no
    /// rompen el recorrido.
    #[test]
    fn lo_encuentra_en_cualquier_rama_del_body() {
        let body = serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": bloque_como_el_real()},
                    {"type": "text", "text": "Responde solo: ok"}
                ]}
            ],
            "nada": null, "n": 3.5
        });

        let b = detect_instructions_in_body(&body).expect("debe encontrarlo en messages[0]");

        assert_eq!(b.format, InstructionsFormat::ClaudeMd);
        assert_eq!(b.bytes, bloque_como_el_real().len());
    }

    /// Un body sin bloque no fabrica uno.
    #[test]
    fn un_body_sin_bloque_devuelve_none() {
        let body = serde_json::json!({
            "model": "x",
            "messages": [{"role": "user", "content": "hola"}]
        });

        assert!(detect_instructions_in_body(&body).is_none());
    }

    /// **Medido en el cuerpo real**: `$.system[2].text` deja un
    /// `<system-reminder>` ABIERTO y sin cerrar, y `messages[0]` trae el bloque
    /// entero. Concatenar las dos cadenas fabricaría un bloque cerrado que en
    /// el cable no existe, y mediría de más. Por eso se examinan por separado.
    #[test]
    fn no_concatena_cadenas_para_fabricar_un_bloque() {
        let body = serde_json::json!({
            "a": format!("{CLAUDE_ABRE}\nrecordatorio abierto con {CLAUDE_MARCA} mencionado"),
            "b": format!("y aquí cierra otro campo distinto {CLAUDE_CIERRA}")
        });

        assert!(detect_instructions_in_body(&body).is_none());
    }

    /// **Límite conocido, fijado a propósito.** El contenido del bloque es
    /// texto libre del usuario: si su `CLAUDE.md` escribe literalmente la
    /// etiqueta de cierre, el recorrido para ahí y la cifra sale CORTA.
    ///
    /// Se prefiere esto a coger el último cierre de la cadena, que ante dos
    /// recordatorios seguidos se tragaría el hueco entre ellos y mediría de
    /// MÁS. Medir de menos en un caso raro y declararlo es honesto; medir de
    /// más en silencio, no. Documentado en `docs/telemetry-per-request.md`.
    #[test]
    fn un_cierre_escrito_por_el_usuario_trunca_la_medida_y_esta_declarado() {
        let texto = format!(
            "{CLAUDE_ABRE}\n{CLAUDE_MARCA}\n\
             Mi CLAUDE.md habla de la etiqueta {CLAUDE_CIERRA} literalmente.\n\
             Y aquí seguiría el resto del fichero.\n{CLAUDE_CIERRA}"
        );

        let b = detect_instructions(&texto).expect("reconoce el bloque igualmente");

        assert!(
            b.bytes < texto.len(),
            "límite conocido: el cierre del usuario trunca la medida"
        );
    }

    // ---- Desglose por cabecera (#97) ----

    /// LA INVARIANTE. Las filas del desglose suman EXACTAMENTE los bytes del
    /// bloque, siempre. Es la misma garantía que da `group_tools_by_server`, y
    /// existe por el mismo motivo: un desglose que no cuadra con su total
    /// invita a restarlos, y esa resta sería una medición inventada.
    ///
    /// El preámbulo es lo que hace que cuadre: se lleva el envoltorio del
    /// harness y todo lo que haya antes de la primera cabecera.
    #[test]
    fn el_desglose_suma_exactamente_los_bytes_del_bloque() {
        let texto = bloque_como_el_real();
        let b = detect_instructions(&texto).expect("reconoce el bloque");

        let suma: usize = b.by_heading.iter().map(|s| s.bytes).sum();
        assert_eq!(
            suma, b.bytes,
            "el desglose tiene que sumar el bloque entero, sin perder ni un byte"
        );
    }

    /// Un fichero SIN cabeceras da UNA fila, y se puede leer como tal: no es
    /// un desglose vacío ni un error, es «esto no está dividido».
    ///
    /// Se prueba sobre el dialecto de **opencode** a propósito: es el único
    /// cuya marca (`Instructions from: `) no es una cabecera markdown. En
    /// Claude Code este caso NO EXISTE — ver
    /// [`la_marca_del_harness_cuenta_como_cabecera`].
    #[test]
    fn sin_cabeceras_sale_una_sola_fila_de_preambulo() {
        let texto = format!(
            "</env>\n{OPENCODE_MARCA}/home/u/p/AGENTS.md\n\
             Responde siempre en español.\n\n{OPENCODE_FIN} y lo que siga."
        );
        let b = detect_instructions(&texto).expect("reconoce");

        assert_eq!(b.by_heading.len(), 1);
        assert_eq!(b.by_heading[0].kind, InstructionsHeadingKind::Preamble);
        assert_eq!(b.by_heading[0].bytes, b.bytes, "se lleva el bloque entero");
    }

    /// HALLAZGO (#97). La marca del envoltorio de Claude Code, `# claudeMd`,
    /// **es una cabecera markdown de nivel 1** — y el desglose la cuenta como
    /// tal, porque por las reglas de markdown lo es.
    ///
    /// No se filtra, y es deliberado: filtrarla exigiría que el desglose
    /// supiera qué líneas puso el harness, que es EXACTAMENTE la dependencia
    /// del contenido que este módulo evita en la frontera del bloque. Se
    /// publica el nivel para que un consumidor pueda decidir por su cuenta, y
    /// se documenta que las primeras filas suelen ser andamiaje.
    ///
    /// Le pasa igual a Codex, cuya marca es `# AGENTS.md instructions for …`.
    #[test]
    fn la_marca_del_harness_cuenta_como_cabecera() {
        let texto = format!("{CLAUDE_ABRE}\n{CLAUDE_MARCA}\ncontenido\n{CLAUDE_CIERRA}");
        let b = detect_instructions(&texto)
            .expect("reconoce")
            .publicable(true);

        assert_eq!(
            b.by_heading
                .iter()
                .filter_map(|s| s.heading.as_deref())
                .collect::<Vec<_>>(),
            vec!["claudeMd"],
            "la marca del harness sale como fila, con su nombre"
        );
        let suma: usize = b.by_heading.iter().map(|s| s.bytes).sum();
        assert_eq!(suma, b.bytes, "y la invariante aguanta igual");
    }

    /// Una cabecera DENTRO de un bloque de código no es una cabecera. Es el
    /// caso más probable de todos: un `CLAUDE.md` que documenta comandos lleva
    /// `# comentario` dentro de un fence, y contarlo partiría la sección por
    /// un sitio que no existe.
    #[test]
    fn las_cabeceras_dentro_de_un_fence_no_cortan() {
        let texto = format!(
            "{CLAUDE_ABRE}\n{CLAUDE_MARCA}\n\
             ## Uno\n\
             ```sh\n\
             # esto es un comentario de shell, no una cabecera\n\
             ## y esto tampoco\n\
             ```\n\
             ## Dos\n\
             {CLAUDE_CIERRA}"
        );
        let b = detect_instructions(&texto).expect("reconoce");

        let b = b.publicable(true);
        let nombres: Vec<&str> = b
            .by_heading
            .iter()
            .filter_map(|s| s.heading.as_deref())
            .collect();
        // `claudeMd` es la marca del harness, que tambien es cabecera.
        assert_eq!(
            nombres,
            vec!["claudeMd", "Uno", "Dos"],
            "lo de dentro del fence no puede aparecer"
        );
    }

    /// El nivel 3 NO corta. Medido sobre el fichero real: bajar a `###` lleva
    /// las filas de 21 a 44 y DESTRUYE la señal — «Model Assignments» pasa de
    /// 9.809 B (29,3%) a 1.705 B (5,1%) porque su contenido se reparte entre
    /// hijos. El bloque que hay que ver desaparece como fila.
    #[test]
    fn el_nivel_3_no_corta_seccion() {
        let texto = format!(
            "{CLAUDE_ABRE}\n{CLAUDE_MARCA}\n\
             ## Padre\ntexto\n\
             ### Hijo\nmas texto\n\
             ## Otro\nfin\n\
             {CLAUDE_CIERRA}"
        );
        let b = detect_instructions(&texto).expect("reconoce");

        let b = b.publicable(true);
        let nombres: Vec<&str> = b
            .by_heading
            .iter()
            .filter_map(|s| s.heading.as_deref())
            .collect();
        assert_eq!(
            nombres,
            vec!["claudeMd", "Padre", "Otro"],
            "`### Hijo` va DENTRO de `## Padre`, no es fila propia"
        );
        let padre = b
            .by_heading
            .iter()
            .find(|s| s.heading.as_deref() == Some("Padre"))
            .expect("hay padre");
        assert!(
            padre.bytes > "## Padre\ntexto\n".len(),
            "el padre se queda los bytes del hijo"
        );
    }

    /// POR DEFECTO los nombres NO viajan. El contenido es texto libre de una
    /// persona y puede llevar nombres de cliente o de proyecto; el campo va a
    /// `telemetry.jsonl` en claro y al buffer de `/requests`.
    #[test]
    fn por_defecto_los_nombres_no_viajan() {
        let texto = bloque_como_el_real();
        let b = detect_instructions(&texto)
            .expect("reconoce")
            .publicable(false);

        assert!(
            b.by_heading.iter().all(|s| s.heading.is_none()),
            "sin la palanca no sale ni un nombre"
        );
        assert!(
            b.by_heading.iter().any(|s| s.bytes > 0),
            "pero los tamaños siguen ahí: quitar el nombre no quita el dato"
        );
    }

    /// Con la palanca puesta sí salen, y esa es toda la diferencia.
    #[test]
    fn con_la_palanca_los_nombres_viajan() {
        let texto = bloque_como_el_real();
        let b = detect_instructions(&texto)
            .expect("reconoce")
            .publicable(true);

        let nombres: Vec<&str> = b
            .by_heading
            .iter()
            .filter_map(|s| s.heading.as_deref())
            .collect();
        assert!(
            nombres.contains(&"Rules"),
            "esperaba la cabecera `## Rules`, salieron {nombres:?}"
        );
    }

    /// El cupo colapsa el sobrante en UN bucket, y ese bucket SIGUE contando
    /// sus bytes: se pierde el desglose fino, nunca un byte. Igual que
    /// `(others)` en `group_tools_by_server`.
    #[test]
    fn el_cupo_colapsa_en_others_sin_perder_un_byte() {
        let mut cuerpo = String::new();
        for i in 0..(MAX_INSTRUCTIONS_HEADINGS + 10) {
            cuerpo.push_str(&format!("## Seccion {i}\ncontenido de la seccion\n"));
        }
        let texto = format!("{CLAUDE_ABRE}\n{CLAUDE_MARCA}\n{cuerpo}{CLAUDE_CIERRA}");
        let b = detect_instructions(&texto).expect("reconoce");

        let suma: usize = b.by_heading.iter().map(|s| s.bytes).sum();
        assert_eq!(suma, b.bytes, "el cupo no puede perder bytes");
        assert_eq!(
            b.by_heading
                .iter()
                .filter(|s| s.kind == InstructionsHeadingKind::Others)
                .count(),
            1,
            "todo el sobrante en UN bucket"
        );
        assert!(
            b.by_heading.len() <= MAX_INSTRUCTIONS_HEADINGS + 2,
            "cupo + preambulo + others, y ni una fila mas"
        );
    }

    /// Un nombre absurdamente largo se recorta, y el recorte NO puede partir
    /// un carácter multibyte: el body es entrada de terceros y un `panic` en
    /// el camino crítico de la petición sería peor que cualquier dato feo.
    #[test]
    fn un_nombre_largo_se_trunca_sin_partir_un_caracter() {
        let largo = "ñ".repeat(MAX_HEADING_LEN * 2);
        let texto =
            format!("{CLAUDE_ABRE}\n{CLAUDE_MARCA}\n## {largo}\ncontenido\n{CLAUDE_CIERRA}");
        let b = detect_instructions(&texto)
            .expect("reconoce")
            .publicable(true);

        let nombre = b
            .by_heading
            .iter()
            .filter_map(|s| s.heading.as_deref())
            .find(|n| n.starts_with('ñ'))
            .expect("hay una cabecera larga");
        assert!(nombre.len() <= MAX_HEADING_LEN, "recortado a bytes");
        assert!(
            nombre.chars().all(|c| c == 'ñ'),
            "y sin dejar medio caracter suelto"
        );
    }
}
