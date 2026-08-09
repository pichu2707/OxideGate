//! Salida de los hooks de `SessionStart`: el 29% del peaje fijo de una sesión
//! y el último de sus tres bloques sin campo propio.
//!
//! # La frontera, que es todo el problema
//!
//! `docs/fixed-toll-claude-code.md` §4 deja escrito el principio, tras dos
//! mediciones falsas: **las fronteras las pone el envoltorio del harness, nunca
//! el contenido.** Un corte «hasta la siguiente cabecera `#`» midió un bloque
//! en 17.002 B cuando eran 1.025: no había siguiente `#` y se tragó el listado
//! de skills entero.
//!
//! Con `instructions` ese envoltorio existía (`<system-reminder>`…`</…>`).
//! Aquí **no**. Verificado sobre captura real del 2026-08-09 (`claude -p`
//! contra sonda local, coste cero): la parte `messages[1]` / `role: "system"`
//! son 28.452 B y contiene EXACTAMENTE dos cosas pegadas —la salida de los
//! hooks y el listado de skills— sin nada que marque dónde acaba la primera:
//!
//! ```text
//! SessionStart:startup hook success: …salida…\n\n
//! The following skills are available for use with the Skill tool:
//! ```
//!
//! El harness abre el bloque con una marca y no lo cierra. La única frontera
//! disponible es dónde EMPIEZA el listado de skills.
//!
//! Sobre esa captura, el detector da **12.097 B y 1 marca**, y
//! `hooks.bytes + tramo_de_skills == parte` exactamente. Para rehacerla:
//!
//! ```sh
//! ANTHROPIC_BASE_URL=http://127.0.0.1:8911 ANTHROPIC_API_KEY=dummy \
//!   claude -p "Responde solo: ok"
//! ```
//!
//! con una sonda local que guarde el cuerpo y devuelva 400 (§4 del documento).
//! El cuerpo NO se versiona: lleva el `CLAUDE.md` y la configuración de quien
//! lo capture. Y por eso no hay aquí un test contra fichero: uno que se salte
//! solo cuando el fichero no está aparenta cobertura sin darla.
//!
//! # Por qué se falla CERRADO
//!
//! Depender de la cabecera de skills tiene un riesgo obvio: si esa cadena
//! cambia, deja de encontrarse. Lo que decide el diseño no es el riesgo, es el
//! MODO DE FALLO.
//!
//! - Correr hasta el final de la parte cuando no se encuentra: correcto si de
//!   verdad no hay skills, pero si la cabecera cambió, `hooks.bytes` se traga
//!   el listado y publica ~16 kB de más. Un número plausible y falso, que es
//!   justo el error que este módulo viene a no repetir.
//! - Devolver `None`: honesto siempre. Cuesta un falso negativo en máquinas
//!   sin NINGUNA skill — y Claude Code trae las suyas de serie, así que ese
//!   caso es casi vacío.
//!
//! Se elige `None`, coherente con el contrato que ya tienen sus dos hermanos:
//! `null` significa **«no se reconoció ningún bloque»**, nunca «no tienes
//! hooks».
//!
//! # Lo que este módulo NO hace
//!
//! Restar. `hooks.bytes = parte − skills.listing_bytes` parece equivalente y
//! no lo es: `listing_bytes` subestima el listado cuando una skill trae la
//! descripción en varias líneas (ver issue #84, medido en −1.453 B sobre esta
//! misma captura), y restar heredaría ese error convertido en bytes de hooks
//! que nunca existieron. La frontera es el INICIO de la cabecera, no una
//! diferencia entre dos medidas.

use super::skills::CLAUDE_SKILLS_HEADER;

/// Dialecto reconocido.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HooksFormat {
    /// Claude Code: marcas `<evento>:<matcher> hook success:` al principio de
    /// la parte `messages[1]` con `role: "system"`.
    ClaudeCode,
}

/// Bloque de salida de hooks encontrado en el body de una petición.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HooksBlock {
    /// Bytes del bloque COMPLETO, marcas incluidas. Es lo que se paga en CADA
    /// petición de la sesión.
    ///
    /// **No es lo que ocupan los hooks en disco ni lo que imprimen en tu
    /// terminal**: es lo que el harness inyecta en el cuerpo.
    pub bytes: usize,
    /// Marcas `hook success:` contadas dentro del bloque.
    ///
    /// Cuenta lo que el CABLE trae, no lo que hay en `settings.json`: un hook
    /// configurado que no produjo salida no aparece aquí, y eso es correcto —
    /// no cuesta nada.
    pub declared: usize,
    /// Qué dialecto se reconoció.
    pub format: HooksFormat,
}

/// Marca con la que Claude Code abre la salida de cada hook.
///
/// El evento y el matcher van delante (`SessionStart:startup hook success:`),
/// así que la parte estable es esta.
const MARCA: &str = " hook success:";

/// Busca el bloque de salida de hooks en un texto plano del body.
///
/// Devuelve `None` si no hay marca, o si no se puede establecer la frontera
/// final — ver el porqué en la documentación del módulo.
pub fn detect_hooks(texto: &str) -> Option<HooksBlock> {
    let marca = texto.find(MARCA)?;

    // El bloque empieza al principio de la LÍNEA de la primera marca, no en la
    // marca: el evento y el matcher que van delante también se pagan.
    let inicio = texto[..marca].rfind('\n').map_or(0, |i| i + 1);

    // Única frontera disponible, y se falla cerrado si no está.
    let fin = texto.find(CLAUDE_SKILLS_HEADER)?;
    if fin <= inicio {
        // El listado va ANTES que la marca: no es la disposición que este
        // detector sabe leer, y adivinar aquí es inventar.
        return None;
    }

    let bloque = &texto[inicio..fin];
    Some(HooksBlock {
        bytes: bloque.len(),
        declared: bloque.matches(MARCA).count(),
        format: HooksFormat::ClaudeCode,
    })
}

/// Busca el bloque de hooks en un body completo.
///
/// Se examina **cadena a cadena, sin concatenar**, por el mismo motivo que sus
/// hermanos: unir textos podría formar una frontera a caballo entre dos campos
/// que en el body no existe — y aquí sería peor que en los otros dos, porque
/// la frontera de este bloque es el principio de OTRO.
pub fn detect_hooks_in_body(body: &serde_json::Value) -> Option<HooksBlock> {
    match body {
        serde_json::Value::String(s) => detect_hooks(s),
        serde_json::Value::Array(xs) => xs.iter().find_map(detect_hooks_in_body),
        serde_json::Value::Object(o) => o.values().find_map(detect_hooks_in_body),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduce la disposición real: marcas de hook, y pegado detrás y sin
    /// separador el listado de skills.
    fn parte(hooks: &str, skills_entradas: &str) -> String {
        format!("{hooks}\n\n{CLAUDE_SKILLS_HEADER}\n{skills_entradas}")
    }

    #[test]
    fn mide_desde_la_marca_hasta_donde_empieza_el_listado_de_skills() {
        let hooks = "SessionStart:startup hook success: salida del primero";
        let texto = parte(hooks, "- una: hace algo\n- otra: hace otra cosa\n");

        let b = detect_hooks(&texto).expect("debe reconocerlo");

        assert_eq!(b.declared, 1);
        assert_eq!(b.format, HooksFormat::ClaudeCode);
        // El `\n\n` de separación entra en el bloque: se paga.
        assert_eq!(b.bytes, hooks.len() + 2);
    }

    #[test]
    fn cuenta_una_marca_por_hook_aunque_vayan_concatenados() {
        let hooks = "SessionStart:startup hook success: uno\n\
                     SessionStart:startup hook success: dos\n\
                     PreToolUse:Bash hook success: tres";
        let texto = parte(hooks, "- una: hace algo\n");

        assert_eq!(detect_hooks(&texto).expect("reconocido").declared, 3);
    }

    /// EL INVARIANTE QUE PIDE EL ISSUE. Los dos campos no pueden solaparse:
    /// medir «de la marca al final» contaría dos veces el listado que
    /// `skills.listing_bytes` ya publica, y cada número por separado seguiría
    /// pareciendo plausible.
    #[test]
    fn el_bloque_no_invade_el_listado_de_skills() {
        let entradas = "- una: hace algo\n- otra: hace otra cosa\n";
        let texto = parte("SessionStart:startup hook success: salida", entradas);
        let tramo_skills = texto.find(CLAUDE_SKILLS_HEADER).expect("hay listado");

        let b = detect_hooks(&texto).expect("reconocido");

        assert_eq!(
            b.bytes + texto[tramo_skills..].len(),
            texto.len(),
            "el bloque de hooks y el tramo de skills tienen que cubrir la parte EXACTAMENTE"
        );
    }

    /// FALLA CERRADO. Sin cabecera de skills no hay frontera, y correr hasta el
    /// final publicaria un número plausible que puede llevar ~16 kB ajenos.
    #[test]
    fn sin_frontera_devuelve_none_en_vez_de_correr_hasta_el_final() {
        let texto = "SessionStart:startup hook success: salida sin nada detrás";

        assert!(
            detect_hooks(texto).is_none(),
            "sin frontera, `null` — que significa «no lo reconozco», no «no tienes hooks»"
        );
    }

    #[test]
    fn sin_marca_de_hook_no_hay_bloque() {
        let texto = parte("texto cualquiera sin marcas", "- una: hace algo\n");

        assert!(detect_hooks(&texto).is_none());
    }

    /// Si el listado va DELANTE de la marca, la disposición no es la que este
    /// detector sabe leer. Sin este corte, la resta daría un rango invertido.
    #[test]
    fn con_el_listado_delante_de_la_marca_no_se_adivina() {
        let texto = format!(
            "{CLAUDE_SKILLS_HEADER}\n- una: hace algo\n\nSessionStart:startup hook success: x"
        );

        assert!(detect_hooks(&texto).is_none());
    }

    /// El bloque arranca al principio de la línea: el evento y el matcher que
    /// preceden a la marca también viajan y también se pagan.
    #[test]
    fn el_evento_y_el_matcher_entran_en_el_conteo() {
        let texto = parte("SessionStart:startup hook success: x", "- una: y\n");

        let b = detect_hooks(&texto).expect("reconocido");

        assert!(
            b.bytes > " hook success: x".len(),
            "empieza en `SessionStart`, no en la marca: {}",
            b.bytes
        );
    }

    #[test]
    fn recorre_el_body_entero_cadena_a_cadena() {
        let texto = parte("SessionStart:startup hook success: x", "- una: y\n");
        let body = serde_json::json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hola"}]},
                {"role": "system", "content": [{"type": "text", "text": texto}]}
            ]
        });

        assert_eq!(
            detect_hooks_in_body(&body).map(|b| b.declared),
            Some(1),
            "el bloque vive en messages[1], y el recorrido no tiene por qué saberlo"
        );
    }

    /// GUARDA DE FORMA. Mismo contrato que `SkillsBlock` y `ToolServerBytes`:
    /// esta estructura se publica en `GET /requests` y se persiste.
    #[test]
    fn la_forma_de_hooks_block_no_cambia_sin_querer() {
        let v = serde_json::to_value(HooksBlock {
            bytes: 12_097,
            declared: 1,
            format: HooksFormat::ClaudeCode,
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
            ["bytes", "declared", "format"]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "cambió la forma de `hooks`. Si es ADITIVO, actualiza esta lista; \
             si quita o renombra, las filas ya escritas dejan de leerse."
        );
        assert_eq!(v["format"], "claude_code");
    }
}
