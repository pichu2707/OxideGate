//! Detección del listado de capacidades invocables en el body, sea cual sea
//! la herramienta.
//!
//! Se le llama "skills" porque es como lo llama el propio harness —*"the
//! following skills are available for use with the Skill tool"*— pero el
//! bloque enumera **todo lo invocable**: skills de usuario, de plugin,
//! integradas y slash commands. Medido: no hay marca que los distinga.
//!
//! `SKILL.md` es una convención compartida —cuatro de las cinco herramientas
//! medidas la usan— pero **cada una manda el listado a su manera**: sitio
//! distinto del body, formato distinto y precio por skill distinto (de 138 B
//! a 390 B, ver `docs/skills-across-tools.md`). Este módulo reconoce las tres
//! formas medidas en tráfico real y se calla ante lo que no conoce.
//!
//! # Por qué no basta con buscar una marca
//!
//! Los propios ficheros de instrucciones del usuario **mencionan** las marcas.
//! Medido en tráfico real de opencode: `<available_skills>` aparece cinco
//! veces en el body y sólo UNA es el listado; las otras son el `AGENTS.md` del
//! usuario hablando del bloque entre comillas. Un detector que se conforme con
//! encontrar la cadena cuenta una mención como si fuera un listado.
//!
//! Por eso un bloque **sólo cuenta si contiene entradas**. Sin entradas no es
//! un listado: es alguien hablando de uno.
//!
//! # Ausencia honesta
//!
//! [`detect_skills`] devuelve `None` cuando no reconoce ningún listado, y eso
//! significa *"no se pudo ver"*, **nunca "hay cero skills"**. Las marcas son
//! cadenas en inglés de cada herramienta: si cambian, el detector deja de
//! encontrarlas y debe declararlo ausente, no fabricar un cero. Mismo contrato
//! que el resto de la telemetría.
//!
//! Hay un segundo camino a la ausencia, y es el precio de no sobremedir nunca:
//! si el texto del usuario menciona la etiqueta de APERTURA **dentro** de un
//! listado real, ese listado se salta (ver [`super::block_scan`]); si menciona
//! la de cierre, la cifra sale corta. Los dos límites miden de MENOS, y están
//! publicados en `docs/telemetry-per-request.md` §4.8.

/// Formato del listado de skills reconocido en el body.
///
/// La variante importa además de los bytes: dice de qué herramienta viene el
/// tráfico sin depender del `User-Agent`, que es contenido controlado por el
/// cliente.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillsFormat {
    /// Lista plana `- nombre: descripción` tras la cabecera de Claude Code.
    /// Medido: 138 B por skill, el más barato de los tres — no manda rutas.
    FlatList,
    /// XML `<available_skills><skill><name>…</name>…</skill></available_skills>`.
    /// Lo usan Gemini CLI (288 B/skill) y opencode (323 B/skill).
    AvailableSkillsXml,
    /// Bloque `<skills_instructions>` con entradas `(file: <ruta>)`.
    /// Lo usa Codex: 390 B por skill, el más caro de los medidos.
    SkillsInstructions,
}

/// Listado de skills encontrado en el body de una petición.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillsBlock {
    /// Cuántas entradas declara el listado.
    ///
    /// **NO es el número de directorios en `~/.claude/skills/`.** El bloque
    /// enumera todo lo que el modelo puede invocar con la herramienta `Skill`,
    /// y eso incluye skills de usuario, de plugin, integradas del harness y
    /// **slash commands** invocables por el modelo.
    ///
    /// Medido: un comando aparece en el MISMO bloque, con el MISMO formato
    /// `- nombre: descripción` y **sin ninguna marca** que lo distinga de una
    /// skill. Separarlos aquí exigiría inventar la distinción, que es lo que
    /// este proyecto no hace. Para el modelo son todos lo mismo; la diferencia
    /// vive en el disco del usuario, no en el cable.
    ///
    /// Y a la inversa: una skill con `disable-model-invocation: true` NO se
    /// lista, así que no cuenta aquí ni cuesta un byte (ver
    /// `docs/optimizer-skills.md` §6).
    pub declared: usize,
    /// Bytes del bloque completo, cabecera incluida. Es lo que se paga en
    /// CADA petición, se invoque una skill o no.
    pub listing_bytes: usize,
    /// Qué forma se reconoció.
    pub format: SkillsFormat,
}

/// Cabecera de la lista plana de Claude Code.
///
/// `pub(super)` porque el detector de hooks la usa como frontera FINAL de su
/// bloque: el harness abre la salida de hooks con una marca y no la cierra, y
/// lo único que hay detrás es este listado (ver `super::hooks`). Compartir la
/// constante no es estética — dos copias que puedan divergir dejarían un hueco
/// o un solape entre dos campos que se publican juntos.
pub(super) const CLAUDE_SKILLS_HEADER: &str =
    "The following skills are available for use with the Skill tool:";

/// Busca el listado de skills en un texto plano del body.
///
/// Devuelve `None` si no reconoce ninguna de las tres formas medidas, o si lo
/// que encuentra es una mención sin entradas.
pub fn detect_skills(texto: &str) -> Option<SkillsBlock> {
    detect_flat_list(texto)
        .or_else(|| detect_xml(texto))
        .or_else(|| detect_skills_instructions(texto))
}

/// Lista plana de Claude Code: cabecera y a continuación líneas `- nombre: …`,
/// hasta la primera línea EN BLANCO.
///
/// # Por qué la frontera es la línea en blanco y no «la primera que no empiece
/// por `- `»
///
/// Esa era la regla anterior, y estaba mal: la descripción de una skill es
/// CONTENIDO —texto libre del frontmatter— y puede ocupar varias líneas.
/// Bastaba con que una sola lo hiciera para que la continuación cortara el
/// recorrido y se llevara por delante todas las entradas posteriores. Medido
/// sobre captura del 2026-08-09: **63 entradas publicadas de 66 reales, y
/// 14.902 B de 16.355 (−8,9%)**, sin que nadie lo notara porque 63 es un
/// número perfectamente plausible.
///
/// Es el mismo principio que `docs/fixed-toll-claude-code.md` §4 deja escrito
/// tras dos mediciones falsas: **las fronteras las pone el envoltorio, nunca
/// el contenido.** Una línea en blanco sí es estructura del listado —el
/// harness no las mete entre entradas, verificado sobre esa captura: las dos
/// que hay están ANTES de la primera— y es lo que permite seguir reconociendo
/// un listado embebido en un mensaje más largo.
fn detect_flat_list(texto: &str) -> Option<SkillsBlock> {
    let inicio = texto.find(CLAUDE_SKILLS_HEADER)?;
    let resto = &texto[inicio + CLAUDE_SKILLS_HEADER.len()..];

    let mut entradas = 0usize;
    let mut fin = 0usize;
    for linea in resto.split_inclusive('\n') {
        let limpia = linea.trim_end_matches(['\n', '\r']);
        if limpia.starts_with("- ") {
            entradas += 1;
            fin += linea.len();
        } else if limpia.trim().is_empty() {
            // Antes de la primera entrada separa la cabecera; después CIERRA.
            if entradas > 0 {
                break;
            }
            fin += linea.len();
        } else if entradas > 0 {
            // Continuación de la entrada anterior: sus bytes se pagan, pero
            // NO es una skill nueva. `declared` cuenta entradas, no líneas.
            fin += linea.len();
        } else {
            // Texto suelto antes de cualquier entrada: no es un listado.
            break;
        }
    }

    if entradas == 0 {
        return None;
    }
    Some(SkillsBlock {
        declared: entradas,
        listing_bytes: CLAUDE_SKILLS_HEADER.len() + fin,
        format: SkillsFormat::FlatList,
    })
}

/// XML `<available_skills>` de Gemini CLI y opencode.
///
/// Recorre TODAS las aperturas y se queda con la primera que contenga al menos
/// un `<skill>`: las demás son menciones del bloque en el texto del usuario.
fn detect_xml(texto: &str) -> Option<SkillsBlock> {
    bloque_con_entradas(
        texto,
        "<available_skills>",
        "</available_skills>",
        "<skill>",
    )
    .map(|(bloque, n)| SkillsBlock {
        declared: n,
        listing_bytes: bloque.len(),
        format: SkillsFormat::AvailableSkillsXml,
    })
}

/// Bloque `<skills_instructions>` de Codex, con entradas `(file: <ruta>)`.
fn detect_skills_instructions(texto: &str) -> Option<SkillsBlock> {
    bloque_con_entradas(
        texto,
        "<skills_instructions>",
        "</skills_instructions>",
        "(file:",
    )
    .map(|(bloque, n)| SkillsBlock {
        declared: n,
        listing_bytes: bloque.len(),
        format: SkillsFormat::SkillsInstructions,
    })
}

/// Primer bloque delimitado por `abre`/`cierra` que contenga al menos una
/// `entrada`, junto con cuántas contiene.
///
/// Vive en [`super::block_scan`] porque el detector de instrucciones necesita
/// exactamente el mismo recorrido, y por exactamente la misma razón: sus marcas
/// también aparecen en el texto que escribe el usuario. Relajar la regla en uno
/// de los dos volvería mentiroso al otro.
use super::block_scan::primer_bloque_con as bloque_con_entradas;

/// Busca el listado de skills en un body completo, sea cual sea el dialecto.
///
/// Las cuatro herramientas medidas lo ponen en sitios distintos —último turno
/// en Claude Code, `systemInstruction` en Gemini, `messages` en opencode,
/// `input` en Codex—, así que en vez de una regla por proveedor se recorre el
/// JSON entero y se prueba cada cadena.
///
/// Se examina **cadena a cadena, sin concatenar**: unir textos podría formar
/// una marca a caballo entre dos campos que en el body no existe.
pub fn detect_skills_in_body(body: &serde_json::Value) -> Option<SkillsBlock> {
    match body {
        serde_json::Value::String(s) => detect_skills(s),
        serde_json::Value::Array(xs) => xs.iter().find_map(detect_skills_in_body),
        serde_json::Value::Object(o) => o.values().find_map(detect_skills_in_body),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GUARDA DE FORMA de `SkillsBlock` (`skills` en `GET /requests`).
    ///
    /// `FIELDS` declara `skills`, así que el NOMBRE del objeto está cubierto por
    /// el test recursivo de `/version`. Sus tres claves internas no lo estaban:
    /// el snapshot de contrato solo congela el primer nivel de la fila.
    #[test]
    fn la_forma_de_skills_no_cambia_sin_querer() {
        let v = serde_json::to_value(SkillsBlock {
            declared: 66,
            listing_bytes: 15_977,
            format: SkillsFormat::FlatList,
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
            ["declared", "format", "listing_bytes"]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "cambió la forma de skills. Si es ADITIVO, actualiza esta lista. Si \
             RENOMBRA, QUITA o cambia el tipo de una clave, sube además \
             CONTRACT_VERSION en middleware::version y anótalo en \
             docs/telemetry-per-request.md §8"
        );
    }

    /// Claude Code: lista plana tras su cabecera. Medido a 138 B por skill.
    #[test]
    fn reconoce_la_lista_plana_de_claude_code() {
        let texto = format!(
            "bla bla\n\n{CLAUDE_SKILLS_HEADER}\n\n- branch-pr: Crea PRs.\n- judgment-day: Revisión doble.\n\nY sigue el mensaje."
        );

        let b = detect_skills(&texto).expect("debe reconocer la lista plana");

        assert_eq!(b.declared, 2);
        assert_eq!(b.format, SkillsFormat::FlatList);
        assert!(
            b.listing_bytes > 60,
            "bytes irrisorios: {}",
            b.listing_bytes
        );
    }

    /// PRUEBA DE MORDIDA (bug real, medido sobre captura del 2026-08-09).
    ///
    /// El recorrido se paraba en la primera línea que no empezara por `- `, así
    /// que UNA sola skill con la descripción repartida en varias líneas cortaba
    /// el listado y se llevaba por delante TODAS las siguientes. Medido: 63
    /// entradas publicadas de 66 reales, y 14.902 B de 16.355 (−8,9%).
    ///
    /// Nadie lo notó porque 63 es un número perfectamente plausible.
    #[test]
    fn una_descripcion_multilinea_no_corta_el_listado() {
        let texto = format!(
            "{CLAUDE_SKILLS_HEADER}\n\
             - primera: hace algo\n\
             - claude-api: referencia de la API\n\
             TRIGGER — lee esto antes de abrir el fichero\n\
             SKIP solo si otro proveedor esta en juego\n\
             - segunda: viene DESPUES de la multilinea\n\
             - tercera: y esta tambien\n"
        );

        let b = detect_skills(&texto).expect("debe reconocer la lista");

        assert_eq!(
            b.declared, 4,
            "las dos de después de la multilínea no pueden perderse"
        );
        assert_eq!(
            b.listing_bytes,
            texto.len(),
            "y sus bytes tampoco: el listado llega hasta el final"
        );
    }

    /// Una continuación suma BYTES pero no es una entrada nueva. `declared`
    /// cuenta skills, no líneas — confundirlo inflaría el conteo al arreglar
    /// el corte.
    #[test]
    fn una_continuacion_suma_bytes_pero_no_cuenta_como_entrada() {
        let sin = format!("{CLAUDE_SKILLS_HEADER}\n- una: hace algo\n");
        let con = format!("{CLAUDE_SKILLS_HEADER}\n- una: hace algo\n  y sigue aquí\n");

        let a = detect_skills(&sin).expect("a");
        let b = detect_skills(&con).expect("b");

        assert_eq!(a.declared, b.declared, "la misma skill, no dos");
        assert!(
            b.listing_bytes > a.listing_bytes,
            "pero los bytes de la continuación SÍ se pagan"
        );
    }

    /// REGRESIÓN. Una línea en blanco sigue cerrando el listado: es lo que
    /// permite reconocerlo cuando va embebido en un texto más largo. Sin este
    /// corte, arreglar la multilínea se tragaría el resto del mensaje — el
    /// mismo error que este fix viene a deshacer, en la otra dirección.
    #[test]
    fn una_linea_en_blanco_sigue_cerrando_el_listado() {
        let texto =
            format!("{CLAUDE_SKILLS_HEADER}\n- una: hace algo\n\nY sigue el mensaje del usuario.");

        let b = detect_skills(&texto).expect("debe reconocer la lista");

        assert_eq!(b.declared, 1);
        assert!(
            !texto[..b.listing_bytes].contains("mensaje del usuario"),
            "el listado no puede tragarse lo que viene detrás"
        );
    }

    /// Gemini CLI y opencode comparten el XML `<available_skills>`.
    #[test]
    fn reconoce_el_xml_de_gemini_y_opencode() {
        let texto = "<available_skills>\n  <skill>\n    <name>a</name>\n    <location>/x/SKILL.md</location>\n  </skill>\n  <skill>\n    <name>b</name>\n  </skill>\n</available_skills>";

        let b = detect_skills(texto).expect("debe reconocer el XML");

        assert_eq!(b.declared, 2);
        assert_eq!(b.format, SkillsFormat::AvailableSkillsXml);
        assert_eq!(b.listing_bytes, texto.len());
    }

    /// Codex: `<skills_instructions>` con entradas `(file: …)`.
    #[test]
    fn reconoce_el_bloque_de_codex() {
        let texto = "<skills_instructions>\n## Skills\n- a: hace algo (file: /x/a/SKILL.md)\n- b: hace otra (file: /x/b/SKILL.md)\n</skills_instructions>";

        let b = detect_skills(texto).expect("debe reconocer el bloque de Codex");

        assert_eq!(b.declared, 2);
        assert_eq!(b.format, SkillsFormat::SkillsInstructions);
    }

    /// **El caso que de verdad importa.** En tráfico real de opencode la marca
    /// `<available_skills>` aparece CINCO veces y sólo una es el listado: las
    /// otras son el `AGENTS.md` del usuario hablando del bloque. Contar una
    /// mención como listado inventaría skills que nadie declaró.
    #[test]
    fn una_mencion_sin_entradas_no_es_un_listado() {
        let texto = "El bloque `<available_skills>` de tu prompt es autoritativo. \
                     ¿Coincide algo con `<available_skills>`? Entonces lee el SKILL.md.";

        assert!(
            detect_skills(texto).is_none(),
            "una mención no es un listado"
        );
    }

    /// Mención primero, listado real después: hay que saltarse la mención y
    /// quedarse con el bloque bueno, no rendirse en el primer intento.
    #[test]
    fn encuentra_el_listado_real_aunque_haya_menciones_antes() {
        let texto = "Habla del bloque `<available_skills>` sin abrirlo de verdad.\n\
                     Y más abajo:\n\
                     <available_skills>\n  <skill>\n    <name>real</name>\n  </skill>\n</available_skills>";

        let b = detect_skills(texto).expect("debe saltarse la mención");

        assert_eq!(b.declared, 1);
        assert_eq!(b.format, SkillsFormat::AvailableSkillsXml);
    }

    /// Sin ninguna marca, `None` significa "no se pudo ver", nunca "cero
    /// skills". Es el contrato de honestidad del resto de la telemetría.
    #[test]
    fn sin_marcas_devuelve_none_y_no_un_cero_fabricado() {
        assert!(detect_skills("un body cualquiera sin skills").is_none());
        assert!(detect_skills("").is_none());
    }

    /// Una apertura sin cierre no puede tumbar el proxy ni inventar un bloque.
    #[test]
    fn un_bloque_sin_cerrar_no_hace_panic_ni_fabrica_nada() {
        assert!(detect_skills("<available_skills>\n  <skill>\n    <name>a</name>").is_none());
        assert!(detect_skills("<skills_instructions>\n- a: x (file: /y)").is_none());
    }

    /// La cabecera de Claude Code sin ninguna entrada detrás tampoco es un
    /// listado: mismo criterio que con las menciones del XML.
    #[test]
    fn la_cabecera_de_claude_sin_entradas_no_cuenta() {
        let texto = format!("{CLAUDE_SKILLS_HEADER}\n\nY aquí no hay ninguna entrada.");

        assert!(detect_skills(&texto).is_none());
    }

    /// Un slash command aparece en el MISMO bloque y con el MISMO formato que
    /// una skill, sin marca que lo distinga: medido en tráfico real. Así que
    /// `declared` cuenta ambos, y el contrato lo dice en vez de fingir una
    /// separación que el cable no permite.
    #[test]
    fn declared_cuenta_tambien_los_slash_commands() {
        let texto = format!(
            "{CLAUDE_SKILLS_HEADER}\n\n- branch-pr: Crea PRs.\n- micomando: Un slash command cualquiera.\n- engram:memory: Una de plugin.\n\nfin."
        );

        let b = detect_skills(&texto).expect("debe reconocer el listado");

        assert_eq!(
            b.declared, 3,
            "cuenta las tres entradas: skill, comando y plugin"
        );
    }

    /// El listado se encuentra esté donde esté: cada herramienta lo pone en un
    /// sitio distinto del body y el recorrido no depende del dialecto.
    #[test]
    fn lo_encuentra_en_cualquier_rama_del_body() {
        let gemini = serde_json::json!({
            "systemInstruction": {"parts": [{"text":
                "<available_skills>\n  <skill>\n    <name>a</name>\n  </skill>\n</available_skills>"}]},
            "contents": []
        });
        let codex = serde_json::json!({
            "input": [{"content": [{"text":
                "<skills_instructions>\n- a: x (file: /y/SKILL.md)\n</skills_instructions>"}]}]
        });

        assert_eq!(
            detect_skills_in_body(&gemini).map(|b| b.format),
            Some(SkillsFormat::AvailableSkillsXml)
        );
        assert_eq!(
            detect_skills_in_body(&codex).map(|b| b.format),
            Some(SkillsFormat::SkillsInstructions)
        );
    }

    /// Un body sin listado no fabrica uno, y los tipos que no son texto no
    /// rompen el recorrido.
    #[test]
    fn un_body_sin_listado_devuelve_none() {
        let body = serde_json::json!({
            "model": "x", "max_tokens": 1024, "stream": true,
            "messages": [{"role": "user", "content": "hola"}],
            "nada": null, "n": 3.5
        });

        assert!(detect_skills_in_body(&body).is_none());
    }

    /// Las cadenas se examinan por separado: dos campos que juntos formarían
    /// una marca no son un listado, porque en el body no están juntos.
    #[test]
    fn no_concatena_cadenas_para_fabricar_una_marca() {
        let body = serde_json::json!({
            "a": "<available_skills>\n  <skill>\n",
            "b": "    <name>x</name>\n  </skill>\n</available_skills>"
        });

        assert!(detect_skills_in_body(&body).is_none());
    }
}
