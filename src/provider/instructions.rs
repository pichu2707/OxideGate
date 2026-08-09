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
/// Hoy tiene una sola variante **a propósito**: es la única verificada en el
/// cable. Codex, opencode y `pi` también inyectan el fichero y tienen marca
/// propia documentada (`docs/skills-across-tools.md` §6), pero esa tabla se
/// escribió contra versiones anteriores y una marca es una cadena literal:
/// añadirlas sin recapturar sería inventar la medición que este módulo existe
/// para no inventar. Cada una entra cuando su captura exista.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
// Las tres variantes acaban en `Md` y clippy sugiere quitar el sufijo. NO se
// hace: estos nombres se serializan con `rename_all` y son VALORES PUBLICADOS
// del campo `instructions.format` (`claude_md`, `codex_agents_md`,
// `opencode_agents_md`), que consume `oxidegate-lens`. Renombrarlos por
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
}

/// Bloque de instrucciones encontrado en el body de una petición.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
/// **El orden no es casual.** Claude Code va primero porque su bloque está
/// delimitado por un envoltorio con apertura Y cierre reales
/// (`<system-reminder>`…`</system-reminder>`), mientras que el de opencode solo
/// tiene apertura. Ante un cuerpo donde los dos pudieran aparecer, gana el que
/// se puede delimitar con certeza.
pub fn detect_instructions(texto: &str) -> Option<InstructionsBlock> {
    detect_claude_md(texto)
        .or_else(|| detect_codex(texto))
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
    Some(InstructionsBlock {
        bytes: texto[inicio..cierra].len(),
        format: InstructionsFormat::CodexAgentsMd,
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
    Some(InstructionsBlock {
        bytes: texto[inicio..fin].len(),
        format: InstructionsFormat::OpencodeAgentsMd,
    })
}

/// Claude Code: primer `<system-reminder>` que contenga la cabecera
/// `# claudeMd`.
fn detect_claude_md(texto: &str) -> Option<InstructionsBlock> {
    primer_bloque_con(texto, CLAUDE_ABRE, CLAUDE_CIERRA, CLAUDE_MARCA).map(|(bytes, _)| {
        InstructionsBlock {
            bytes,
            format: InstructionsFormat::ClaudeMd,
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
            ["bytes", "format"]
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
}
