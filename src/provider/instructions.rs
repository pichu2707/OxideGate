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
pub enum InstructionsFormat {
    /// Claude Code: `<system-reminder>` en `messages[0]` con la cabecera
    /// `# claudeMd` dentro. Medido: 33.716 B en una máquina real.
    ClaudeMd,
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

/// Busca el bloque de instrucciones en un texto plano del body.
///
/// Devuelve `None` si no reconoce ningún dialecto, o si lo que encuentra es un
/// envoltorio sin la marca interna.
pub fn detect_instructions(texto: &str) -> Option<InstructionsBlock> {
    detect_claude_md(texto)
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
