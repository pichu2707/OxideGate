//! Proveedor Anthropic (Claude): ruta fija, modelo y `stream` en el body.
//!
//! A diferencia de OpenAI, Anthropic ya manda `usage` con cada evento SSE
//! por defecto: no hace falta pedir nada extra para leer tokens exactos.
//!
//! Sí existe una mutación OPCIONAL del body: la palanca A del optimizador
//! (`AppConfig::force_prompt_cache`). Cuando está prendida y el cliente no
//! gestiona su propio prompt caching, `prepare` inyecta un breakpoint de
//! `cache_control` a nivel raíz para que Anthropic cachee el prefijo estable
//! (`tools` + `system`) y las llamadas repetidas paguen `cache_read` (0.1x)
//! en vez de tarifa plena. Ver `docs/optimizer-prompt-cache.md`.
//!
//! `prepare` también LEE (sin mutar) dos palancas de VELOCIDAD que el
//! cliente ya manda hoy en el body, dialecto exclusivo de Anthropic:
//! `output_config.effort` (nivel de esfuerzo de razonamiento — menos
//! "thinking" ⇒ generación más corta) y `speed` a nivel raíz (modo `fast`
//! beta de Opus 4.8/4.7). Ver [`Outgoing::requested_effort`] y
//! [`Outgoing::requested_speed`] para el contrato completo. `extract_usage`
//! lee el complemento del lado de la respuesta, `usage.speed` (ver
//! [`Usage::speed`]): documentado por Anthropic pero no observado todavía en
//! tráfico real de este proyecto.
use super::{
    array_field, fingerprint, measure_key, measure_other, model_and_stream_from_value, parse_body,
    split_history_and_last_turn, tools_overhead_bytes, ContextBreakdown, Incoming, Outgoing,
    Provider, ToolCalls, ToolServerKind, Usage,
};
use crate::config::AppConfig;
use serde_json::Value;

/// Adaptador del dialecto Anthropic (`/v1/messages`).
pub struct Anthropic;

/// Instancia única y sin estado. Vive `'static` para que `MeteredBody` pueda
/// sostener una referencia al proveedor durante todo el stream de respuesta.
pub static ANTHROPIC: Anthropic = Anthropic;

impl Provider for Anthropic {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    /// Arma el request hacia `{anthropic}/messages`. Parsea el body UNA sola
    /// vez ([`parse_body`]) y reutiliza el `Value` resultante para leer
    /// `model`/`stream`, calcular `context` ([`Provider::decompose`]) y, si
    /// `cfg.force_prompt_cache` está activo, intentar inyectar un breakpoint
    /// de `cache_control` (ver [`force_cache_control`]) — nunca vuelve a
    /// llamar a `serde_json::from_slice` sobre los bytes crudos.
    ///
    /// `prompt_hash`/`prompt_bytes` se calculan siempre sobre `incoming.body`
    /// ORIGINAL (antes de parsear o mutar nada): son la huella y el tamaño
    /// del body tal como llegó del cliente, no del JSON canónico.
    ///
    /// `tools_by_server`/`tools_overhead_bytes` se calculan también del mismo
    /// `Value` ya parseado (nunca un segundo parseo): `tools_by_server` vía
    /// [`Provider::tools_by_server`] (vacío si `parsed` es `None`, es decir
    /// si el body no parseó), y `tools_overhead_bytes` restando esa suma de
    /// `context.tools_bytes` con el helper compartido [`tools_overhead_bytes`]
    /// (`0` si `context` es `None`).
    ///
    /// `requested_effort`/`requested_speed` se leen del mismo `Value` (ver
    /// [`requested_effort_of`]/[`requested_speed_of`]): `None` si `parsed` es
    /// `None` (body no parseó como JSON).
    fn prepare(&self, incoming: Incoming, cfg: &AppConfig) -> Outgoing {
        let prompt_hash = fingerprint(&incoming.body);
        let prompt_bytes = incoming.body.len();
        let parsed = parse_body(&incoming.body);

        let (model, stream) = parsed
            .as_ref()
            .map(model_and_stream_from_value)
            .unwrap_or((None, false));
        let context = parsed.as_ref().and_then(|v| self.decompose(v));
        let skills = parsed.as_ref().and_then(crate::provider::skills::detect_skills_in_body);
        let instructions = parsed
            .as_ref()
            .and_then(crate::provider::instructions::detect_instructions_in_body);
        let by_server = parsed
            .as_ref()
            .map(|v| self.tools_by_server(v))
            .unwrap_or_default();
        let overhead = context
            .as_ref()
            .map(|c| tools_overhead_bytes(c.tools_bytes, &by_server))
            .unwrap_or(0);
        let requested_effort = parsed.as_ref().and_then(requested_effort_of);
        let requested_speed = parsed.as_ref().and_then(requested_speed_of);

        let (body, cache_control_forced, effort_forced) = aplicar_palancas(
            incoming.body,
            parsed,
            cfg.force_prompt_cache,
            cfg.force_effort.as_deref(),
        );

        Outgoing {
            url: format!("{}/messages", cfg.target_anthropic_url),
            route: "/v1/messages".to_string(),
            upstream: self.name(),
            model,
            stream,
            prompt_hash,
            prompt_bytes,
            body,
            cache_control_forced,
            context,
            skills,
            instructions,
            effort_forced,
            tools_by_server: by_server,
            tools_overhead_bytes: overhead,
            requested_effort,
            requested_speed,
            // El mecanismo `tool_search` (carga diferida vía `input[]`) es
            // exclusivo del dialecto Responses/Codex: Anthropic no lo tiene.
            tool_search: None,
            // Anthropic usa el namespacing `mcp__` fiable (Claude Code): su
            // `(native)` no necesita la advertencia de aplanado.
            tools_flattened: None,
        }
    }

    /// `usage` vive en la raíz (evento `message_delta`) o anidado bajo
    /// `message` (evento `message_start`). El conteo de salida es
    /// acumulativo entre eventos: "último gana".
    ///
    /// Anthropic reporta la caché APARTE de `input_tokens`:
    /// `cache_read_input_tokens` (lectura) y `cache_creation_input_tokens`
    /// (escritura) se guardan crudos, sin tocar `input_tokens`.
    ///
    /// `usage.speed` (ver [`Usage::speed`]) se lee con la MISMA semántica
    /// "último gana" y de las MISMAS dos ubicaciones que el resto de los
    /// campos: documentado por Anthropic, todavía no observado en tráfico
    /// real de este proyecto.
    fn extract_usage(&self, value: &Value, usage: &mut Usage) {
        let Some(u) = value
            .get("usage")
            .or_else(|| value.get("message").and_then(|m| m.get("usage")))
        else {
            return;
        };

        if let Some(v) = u.get("input_tokens").and_then(Value::as_u64) {
            usage.input_tokens = Some(v);
        }
        if let Some(v) = u.get("output_tokens").and_then(Value::as_u64) {
            usage.output_tokens = Some(v);
        }
        if let Some(v) = u.get("cache_read_input_tokens").and_then(Value::as_u64) {
            usage.cache_read_tokens = Some(v);
        }
        if let Some(v) = u
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            usage.cache_write_tokens = Some(v);
        }
        if let Some(v) = u.get("speed").and_then(Value::as_str) {
            usage.speed = Some(v.to_string());
        }
    }

    /// Lee las invocaciones del dialecto de Anthropic, que las publica en
    /// DOS formas según el modo de la respuesta:
    ///
    /// - **Streaming**: un evento `content_block_start` por bloque, con el
    ///   bloque colgando de `content_block` (singular). El `name` viaja
    ///   ENTERO en ese evento; lo único troceado en `input_json_delta` es el
    ///   `input`, que este campo no necesita.
    /// - **No-streaming**: el cuerpo entero, con todos los bloques en el
    ///   array `content` de la raíz.
    ///
    /// El `return` tras la rama de streaming NO es un atajo: garantiza que
    /// un mismo `Value` no pueda contarse por los dos caminos. Hoy no puede
    /// ocurrir —`message_start` anida su `content` (vacío) bajo `message`,
    /// no en la raíz— pero un evento futuro que trajera ambas claves
    /// duplicaría cada invocación en silencio, y una fila con el doble de
    /// llamadas no tiene ninguna pinta de estar mal.
    fn extract_tool_use(&self, value: &Value, calls: &mut ToolCalls) {
        if let Some(block) = value.get("content_block") {
            registra_bloque(block, calls);
            return;
        }

        // `message_stop` es la marca de fin del dialecto SSE de Anthropic.
        // Verla es la unica prueba de que la respuesta se escaneo entera: el
        // `status` de la fila no sirve —se captura antes de que fluya el
        // cuerpo— y sin esta senal un turno abortado a mitad seria
        // indistinguible de uno completo sin invocaciones.
        if value.get("type").and_then(Value::as_str) == Some("message_stop") {
            calls.marca_completa();
            return;
        }

        if let Some(bloques) = value.get("content").and_then(Value::as_array) {
            for bloque in bloques {
                registra_bloque(bloque, calls);
            }
            // Un cuerpo no-streaming con `content` parseado ES la respuesta
            // entera: si `finish()` llego a deserializarlo, no falto nada.
            calls.marca_completa();
        }
    }

    /// Anthropic es el unico dialecto verificado en el cable hoy.
    fn captura_invocaciones(&self) -> bool {
        true
    }

    /// Desglosa el body de `/v1/messages`. Mapeo directo del dialecto:
    /// `system` (string o array de bloques de contenido, ambos se miden
    /// igual con `serde_json::to_vec`) → `system_bytes`; `tools` →
    /// `tools_bytes`; `messages` → todo menos el último a `history_bytes`, el
    /// último a `last_turn_bytes`; cualquier otra clave de la raíz (`model`,
    /// `max_tokens`, `temperature`, `stream`…) → `other_bytes`.
    ///
    /// `None` solo si `body` no es un objeto JSON (array, string, número):
    /// nunca hace panic sobre un body inesperado.
    fn decompose(&self, body: &Value) -> Option<ContextBreakdown> {
        let obj = body.as_object()?;

        let system_bytes = measure_key(obj, "system");
        let tools_bytes = measure_key(obj, "tools");
        let messages = array_field(obj, "messages");
        let (history_bytes, last_turn_bytes, messages_count) =
            split_history_and_last_turn(messages.iter());
        let other_bytes = measure_other(obj, &["system", "tools", "messages"]);

        Some(ContextBreakdown {
            system_bytes,
            tools_bytes,
            history_bytes,
            last_turn_bytes,
            other_bytes,
            measured_bytes: system_bytes + tools_bytes + history_bytes + last_turn_bytes + other_bytes,
            messages_count,
        })
    }

    /// Herramientas de `/v1/messages`: `tools[]`, nombre en `tool["name"]`.
    /// Cada entrada mide el objeto COMPLETO de la herramienta (`name` +
    /// `description` + `input_schema`), no solo el nombre: es la unidad que
    /// realmente pesa en el body.
    fn tool_entries<'a>(&self, body: &'a Value) -> Option<Vec<(&'a str, &'a Value)>> {
        let tools = body.as_object()?.get("tools")?.as_array()?;
        Some(
            tools
                .iter()
                .filter_map(|tool| {
                    let name = tool.get("name")?.as_str()?;
                    Some((name, tool))
                })
                .collect(),
        )
    }
}

/// Registra UN bloque de contenido en `calls` si es una invocación.
///
/// Discrimina por el `type` del bloque, no por la presencia de `name`: un
/// bloque `text` no tiene `name` y se ignoraría igual, pero apoyarse en esa
/// coincidencia dejaría que cualquier bloque futuro con `name` (uno de
/// citación, uno de adjunto) entrara como si fuera una llamada.
///
/// Los dos tipos van a listas SEPARADAS porque miden cosas distintas:
/// `tool_use` es una herramienta que el agente declaró y ejecuta él —lo que
/// alimenta al recomendador de MCP—, mientras que `server_tool_use` la
/// ejecuta el proveedor y no sale de la configuración del usuario. Sumarlas
/// inflaría el "sí lo usas" de un servidor MCP con llamadas que no son suyas.
fn registra_bloque(bloque: &Value, calls: &mut ToolCalls) {
    let Some(name) = bloque.get("name").and_then(Value::as_str) else {
        return;
    };

    match bloque.get("type").and_then(Value::as_str) {
        // `tool_use`: el nombre SIGUE la convencion `mcp__<server>__<tool>`
        // cuando viene de un MCP client-side, asi que el servidor se deduce
        // de el.
        Some("tool_use") => calls.push_invoked(name),

        // `mcp_tool_use` es el conector MCP server-side de Anthropic, y su
        // forma es DISTINTA: el nombre llega DESNUDO (`search_docs`) y el
        // servidor viaja en un campo hermano. Pasarlo por la convencion de
        // nombres lo atribuiria a `(native)` en el 100% de los casos, el
        // servidor real apareceria sin usar, y el recomendador aconsejaria
        // borrar justo el que se invoca en cada turno.
        //
        // Sin `server_name` legible no se inventa un servidor: cae a la
        // deduccion por nombre, que al menos es honesta sobre no saberlo.
        Some("mcp_tool_use") => match bloque.get("server_name").and_then(Value::as_str) {
            Some(servidor) if !servidor.is_empty() => {
                calls.push_invoked_de(name, servidor, ToolServerKind::Mcp)
            }
            _ => calls.push_invoked(name),
        },

        Some("server_tool_use") => calls.push_server_invoked(name),
        _ => {}
    }
}

/// Lee `output_config.effort` de un `Value` YA PARSEADO (ver
/// [`Outgoing::requested_effort`] para el contrato completo del campo).
/// `None` si `output_config` está ausente, si `effort` está ausente dentro de
/// `output_config`, o si `effort` no es un string — nunca hace panic ni
/// inventa un valor a partir de un tipo inesperado (p. ej. un número).
fn requested_effort_of(value: &Value) -> Option<String> {
    value
        .get("output_config")?
        .get("effort")?
        .as_str()
        .map(str::to_string)
}

/// Lee `speed` a nivel RAÍZ de un `Value` YA PARSEADO (ver
/// [`Outgoing::requested_speed`] para el contrato completo del campo). A
/// diferencia de `effort`, este campo NO está anidado bajo `output_config`.
/// `None` si `speed` está ausente en la raíz o no es un string.
fn requested_speed_of(value: &Value) -> Option<String> {
    value.get("speed")?.as_str().map(str::to_string)
}

/// Palanca A del optimizador: si el body es JSON válido y NO trae ya ningún
/// `cache_control`, inyecta uno a nivel raíz (`{"type": "ephemeral"}`).
///
/// Anthropic hace *prefix match*: un `cache_control` en la raíz del request
/// se auto-coloca en el último bloque cacheable, cubriendo `tools` + `system`
/// sin que haga falta localizar el bloque a mano. No hace falta pedirlo si el
/// cliente YA gestiona su propio caching (evita pisar sus breakpoints y
/// superar el máximo de 4 por request, que Anthropic responde con `400`).
///
/// Devuelve `(body, forced)`: `body` reenviable tal cual (mutado o no) y
/// `forced` para que la métrica sepa si esta petición llevó la inyección.
/// Si el body no es JSON válido —o es JSON válido pero no un objeto (array,
/// string, número…), que no es indexable— se reenvía intacto y
/// `forced = false` (preferimos no medir/mutar a romper el request).
///
/// Toma `raw` (para poder devolverlo intacto sin reserializar cuando no hay
/// mutación) y `parsed`, el `Value` que YA parseó `prepare` a partir de
/// `raw`: esta función nunca vuelve a llamar a `serde_json::from_slice`.
/// Aplica las palancas del optimizador sobre el body saliente y lo serializa
/// **una sola vez**.
///
/// Existe porque las dos palancas mutan el MISMO body: encadenarlas por
/// separado significaría serializar dos veces y, peor, que la segunda tuviera
/// que volver a parsear lo que la primera acababa de escribir.
///
/// Con las dos apagadas —el default— devuelve `raw` **intacto**, sin pasar por
/// una vuelta de reserializado. Es la invariante 3 del contrato de `prepare`:
/// parsear no es reserializar, y un proxy que promete no tocar nada tiene que
/// devolver los mismos bytes que recibió.
///
/// Si algo falla a mitad —el body no es un objeto, la serialización revienta—
/// se reenvía `raw` y **no se declara ninguna intervención**: preferimos no
/// mutar a romper el request, y sobre todo preferimos no decir que mutamos
/// algo que no mutamos.
fn aplicar_palancas(
    raw: Vec<u8>,
    parsed: Option<Value>,
    force_cache: bool,
    force_effort: Option<&str>,
) -> (Vec<u8>, bool, Option<String>) {
    if !force_cache && force_effort.is_none() {
        return (raw, false, None);
    }

    let Some(mut value) = parsed else {
        return (raw, false, None);
    };

    // Solo los objetos JSON son indexables por clave: mutar un array o un
    // escalar entraría en pánico.
    if !value.is_object() {
        return (raw, false, None);
    }

    let cache = force_cache && inyecta_cache_control(&mut value);
    let effort = force_effort.and_then(|objetivo| fuerza_effort(&mut value, objetivo));

    if !cache && effort.is_none() {
        return (raw, false, None);
    }

    match serde_json::to_vec(&value) {
        Ok(body) => (body, cache, effort),
        Err(_) => (raw, false, None),
    }
}

/// Palanca A sobre un `Value` ya parseado. `true` si mutó.
///
/// No hace falta pedir caché si el cliente YA gestiona el suyo: pisar sus
/// breakpoints puede superar el máximo de 4 por request, que Anthropic
/// responde con `400`.
fn inyecta_cache_control(value: &mut Value) -> bool {
    if has_cache_control(value) {
        return false;
    }
    value["cache_control"] = serde_json::json!({"type": "ephemeral"});
    true
}

/// Palanca B sobre un `Value` ya parseado. Devuelve el nivel impuesto, o
/// `None` si no hubo intervención que declarar.
///
/// **Sobrescribe lo que el cliente pidió, a propósito.** Medido: Claude Code
/// manda `{"effort": "high"}` explícito en cada petición, así que una palanca
/// que solo actuara ante su ausencia no haría nada nunca. Lo que la hace
/// honesta no es abstenerse, es que la fila publique las dos cosas:
/// `requested_effort` (leído ANTES de mutar) y `effort_forced`.
///
/// Dos casos en los que no toca nada:
///
/// - **Ya está en el nivel pedido.** No hay intervención que declarar, y
///   declararla sería mentir en la única dirección que importa.
/// - **`output_config` existe y no es un objeto.** Entonces esto no es el
///   dialecto que creemos, y meter una clave dentro rompería el request.
fn fuerza_effort(value: &mut Value, objetivo: &str) -> Option<String> {
    // Comparación insensible a mayúsculas: `objetivo` viene normalizado de
    // `parse_force_effort`, pero lo que trae el cliente no pasa por ahí. Sin
    // esto, un `"LOW"` del cliente contra un `low` configurado se reportaría
    // como intervención cuando lo único que cambia es el casing — y una
    // intervención declarada que no cambia nada es tan mentira como una real
    // sin declarar.
    if requested_effort_of(value).is_some_and(|actual| actual.eq_ignore_ascii_case(objetivo)) {
        return None;
    }
    if value.get("output_config").is_some_and(|oc| !oc.is_object()) {
        return None;
    }

    value
        .as_object_mut()?
        .entry("output_config")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()?
        .insert("effort".to_string(), Value::String(objetivo.to_string()));

    Some(objetivo.to_string())
}

/// Detecta recursivamente si la clave `cache_control` aparece en cualquier
/// nivel del `Value` (raíz, `system`, `tools`, `messages`, o anidado dentro
/// de esos). Basta con UN hallazgo para respetar el caching que ya gestiona
/// el cliente y no forzar nada encima.
fn has_cache_control(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key("cache_control") || map.values().any(has_cache_control)
        }
        Value::Array(items) => items.iter().any(has_cache_control),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{MAX_TOOL_NAME_LEN, MAX_TOOL_NAMES, NATIVE_TOOLS_LABEL, measure_value};

    /// Solo los nombres de las invocaciones de cliente. La ATRIBUCIÓN a
    /// servidor tiene sus propios tests: mezclarlas aquí haría que un fallo
    /// de atribución se leyera como un fallo de captura.
    fn nombres(calls: &ToolCalls) -> Vec<String> {
        calls.invoked.iter().map(|c| c.name.clone()).collect()
    }

    /// Anthropic manda el input en `message_start` y el output acumulado en
    /// `message_delta`. Extraer ambos eventos por separado debe dejar los
    /// dos contadores seteados sobre el mismo acumulador.
    #[test]
    fn extracts_anthropic_usage_from_sse() {
        let mut usage = Usage::default();
        let start: Value = serde_json::from_str(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":42,"output_tokens":1}}}"#,
        )
        .unwrap();
        let delta: Value =
            serde_json::from_str(r#"{"type":"message_delta","usage":{"output_tokens":99}}"#)
                .unwrap();

        ANTHROPIC.extract_usage(&start, &mut usage);
        ANTHROPIC.extract_usage(&delta, &mut usage);

        assert_eq!(usage.input_tokens, Some(42));
        assert_eq!(usage.output_tokens, Some(99));
    }

    /// Respuesta no-streaming: `usage` en la raíz de un único JSON completo.
    #[test]
    fn extracts_usage_from_non_stream_body() {
        let mut usage = Usage::default();
        let value: Value = serde_json::from_str(
            r#"{"model":"claude","usage":{"input_tokens":5,"output_tokens":8}}"#,
        )
        .unwrap();

        ANTHROPIC.extract_usage(&value, &mut usage);

        assert_eq!(usage.input_tokens, Some(5));
        assert_eq!(usage.output_tokens, Some(8));
    }

    /// La caché de Anthropic va APARTE del input: `cache_read_input_tokens`
    /// y `cache_creation_input_tokens` deben quedar en sus propios campos,
    /// sin alterar `input_tokens`.
    #[test]
    fn extracts_anthropic_cache_tokens() {
        let mut usage = Usage::default();
        let value: Value = serde_json::from_str(
            r#"{"model":"claude","usage":{"input_tokens":5,"output_tokens":8,"cache_read_input_tokens":100,"cache_creation_input_tokens":20}}"#,
        )
        .unwrap();

        ANTHROPIC.extract_usage(&value, &mut usage);

        assert_eq!(usage.input_tokens, Some(5));
        assert_eq!(usage.output_tokens, Some(8));
        assert_eq!(usage.cache_read_tokens, Some(100));
        assert_eq!(usage.cache_write_tokens, Some(20));
    }

    /// Construye un `AppConfig` mínimo para los tests de `prepare`, sin pasar
    /// por `AppConfig::load()` (que lee variables de entorno del proceso).
    fn test_config(force_prompt_cache: bool) -> AppConfig {
        config_con(force_prompt_cache, None)
    }

    /// Config de test con las DOS palancas gobernables, para poder afirmar
    /// que no se pisan entre ellas.
    fn config_con(force_prompt_cache: bool, force_effort: Option<&str>) -> AppConfig {
        AppConfig {
            local_port: 8080,
            bind_host: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            bind_host_warning: None,
            target_openai_url: "https://api.openai.com/v1".to_string(),
            target_anthropic_url: "https://api.anthropic.com/v1".to_string(),
            target_gemini_url: "https://generativelanguage.googleapis.com".to_string(),
            target_codex_url: "https://chatgpt.com/backend-api/codex".to_string(),
            storage_dir: std::path::PathBuf::from("/tmp/oxidegate-test"),
            force_prompt_cache,
            force_effort: force_effort.map(str::to_string),
            force_effort_warning: None,
        }
    }

    fn incoming_with_body(body: &str) -> Incoming {
        Incoming {
            path: "/v1/messages".to_string(),
            query: None,
            body: body.as_bytes().to_vec(),
            content_encoding: None,
        }
    }

    /// Con la palanca prendida y un body SIN `cache_control`, `prepare` debe
    /// inyectar el breakpoint a nivel raíz y marcar `cache_control_forced`.
    #[test]
    fn injects_cache_control_when_forced_and_absent() {
        let cfg = test_config(true);
        let incoming = incoming_with_body(
            r#"{"model":"claude-3-5-sonnet","system":"eres un asistente","messages":[]}"#,
        );

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert!(out.cache_control_forced);
        let body: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(body["cache_control"]["type"], "ephemeral");
    }

    /// Con la palanca prendida pero el body YA trae un `cache_control` (p.
    /// ej. el cliente cachea su propio bloque `system`), `prepare` no debe
    /// tocar nada: se respeta el caching del cliente y no se arriesga a
    /// superar el máximo de 4 breakpoints.
    #[test]
    fn does_not_inject_when_cache_control_already_present() {
        let cfg = test_config(true);
        let incoming = incoming_with_body(
            r#"{"model":"claude-3-5-sonnet","system":[{"type":"text","text":"eres un asistente","cache_control":{"type":"ephemeral"}}],"messages":[]}"#,
        );
        let original_body = incoming.body.clone();

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert!(!out.cache_control_forced);
        assert_eq!(out.body, original_body);
    }

    /// Palanca B: con el flag puesto, `effort` pasa a ser el configurado y la
    /// fila declara a QUÉ se forzó — no un simple booleano.
    #[test]
    fn la_palanca_b_fuerza_el_effort_y_declara_a_que() {
        let cfg = config_con(false, Some("low"));
        let incoming = incoming_with_body(
            r#"{"model":"claude-opus-4-8","output_config":{"effort":"high"},"messages":[]}"#,
        );

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert_eq!(out.effort_forced.as_deref(), Some("low"));
        let body: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(body["output_config"]["effort"], "low");
    }

    /// **La fila tiene que contar las DOS cosas.** `requested_effort` se lee
    /// ANTES de mutar, así que sigue diciendo lo que pidió el cliente aunque
    /// el proxy lo haya pisado. Sin esto, una medición sobre un body que el
    /// propio medidor alteró no se distinguiría de una limpia.
    #[test]
    fn requested_effort_sigue_siendo_el_del_cliente_tras_forzar() {
        let cfg = config_con(false, Some("low"));
        let incoming = incoming_with_body(
            r#"{"model":"claude-opus-4-8","output_config":{"effort":"high"},"messages":[]}"#,
        );

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert_eq!(
            out.requested_effort.as_deref(),
            Some("high"),
            "lo que pidió el cliente"
        );
        assert_eq!(
            out.effort_forced.as_deref(),
            Some("low"),
            "lo que impuso el proxy"
        );
    }

    /// Si el cliente YA pedía el nivel configurado, no hay intervención que
    /// declarar: el body no se toca y la fila no miente diciendo que sí.
    #[test]
    fn no_declara_intervencion_si_el_cliente_ya_pedia_ese_nivel() {
        let cfg = config_con(false, Some("low"));
        let incoming = incoming_with_body(
            r#"{"model":"claude-opus-4-8","output_config":{"effort":"low"},"messages":[]}"#,
        );
        let original = incoming.body.clone();

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert!(out.effort_forced.is_none());
        assert_eq!(out.body, original, "sin mutación no hay reserializado");
    }

    /// Sin `output_config` en el body, forzar significa crearlo.
    #[test]
    fn crea_output_config_si_no_venia() {
        let cfg = config_con(false, Some("xhigh"));
        let incoming = incoming_with_body(r#"{"model":"claude-opus-4-8","messages":[]}"#);

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert_eq!(out.effort_forced.as_deref(), Some("xhigh"));
        let body: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(body["output_config"]["effort"], "xhigh");
    }

    /// `output_config` presente pero NO objeto: no es el dialecto que creemos,
    /// así que no se toca. Preferimos no mutar a romper el request — mismo
    /// criterio que la palanca A con un body que no es objeto.
    #[test]
    fn un_output_config_que_no_es_objeto_no_se_toca() {
        let cfg = config_con(false, Some("low"));
        let incoming = incoming_with_body(
            r#"{"model":"claude-opus-4-8","output_config":"raro","messages":[]}"#,
        );
        let original = incoming.body.clone();

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert!(out.effort_forced.is_none());
        assert_eq!(out.body, original);
    }

    /// `output_config: null` es distinto de ausente y de no-objeto, y es un
    /// valor que un cliente o un harness con un bug puede mandar
    /// perfectamente. No debe petar ni mutar.
    #[test]
    fn un_output_config_nulo_no_se_toca_ni_peta() {
        let cfg = config_con(false, Some("low"));
        let incoming =
            incoming_with_body(r#"{"model":"claude-opus-4-8","output_config":null,"messages":[]}"#);
        let original = incoming.body.clone();

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert!(out.effort_forced.is_none());
        assert_eq!(out.body, original);
    }

    /// El casing del cliente no es una intervención. Si pide `"LOW"` y la
    /// palanca está en `low`, no hay nada que forzar: declararlo sería
    /// reportar una intervención que no cambia el esfuerzo, solo la grafía.
    #[test]
    fn el_casing_del_cliente_no_cuenta_como_intervencion() {
        let cfg = config_con(false, Some("low"));
        let incoming = incoming_with_body(
            r#"{"model":"claude-opus-4-8","output_config":{"effort":"LOW"},"messages":[]}"#,
        );
        let original = incoming.body.clone();

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert!(out.effort_forced.is_none());
        assert_eq!(out.body, original, "sin intervención no hay reserializado");
    }

    /// Con la palanca B encendida y un body que NO es un objeto JSON, se
    /// reenvía intacto. La guarda está compartida con la palanca A, pero un
    /// test que solo la ejercita por un camino no prueba el otro.
    #[test]
    fn la_palanca_b_no_toca_un_body_que_no_es_objeto() {
        let cfg = config_con(false, Some("low"));
        for crudo in ["[1,2,3]", "\"no soy un objeto\"", "esto no es json"] {
            let incoming = incoming_with_body(crudo);
            let original = incoming.body.clone();

            let out = ANTHROPIC.prepare(incoming, &cfg);

            assert!(out.effort_forced.is_none(), "crudo: {crudo}");
            assert_eq!(out.body, original, "crudo: {crudo}");
        }
    }

    /// **La palanca sube tanto como baja, y hay que fijarlo.** Un comentario
    /// afirmaba lo contrario y el código nunca lo cumplió; lo cazó una
    /// revisión doble. El test existe para que la afirmación viva aquí y no
    /// en una frase que nadie vuelve a comprobar.
    #[test]
    fn la_palanca_b_tambien_sube_el_nivel() {
        let cfg = config_con(false, Some("max"));
        let incoming = incoming_with_body(
            r#"{"model":"claude-opus-4-8","output_config":{"effort":"low"},"messages":[]}"#,
        );

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert_eq!(out.effort_forced.as_deref(), Some("max"));
        let body: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(body["output_config"]["effort"], "max");
    }

    /// Las DOS palancas a la vez sobre el mismo body: se aplican ambas y el
    /// body se serializa UNA sola vez. Que una encienda no puede tragarse a la
    /// otra.
    #[test]
    fn las_dos_palancas_conviven_en_una_sola_serializacion() {
        let cfg = config_con(true, Some("low"));
        let incoming = incoming_with_body(
            r#"{"model":"claude-opus-4-8","output_config":{"effort":"high"},"messages":[]}"#,
        );

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert!(out.cache_control_forced, "palanca A");
        assert_eq!(out.effort_forced.as_deref(), Some("low"), "palanca B");
        let body: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(body["cache_control"]["type"], "ephemeral");
        assert_eq!(body["output_config"]["effort"], "low");
    }

    /// La palanca B no toca a los otros dialectos: `effort` es de Anthropic.
    /// Lo guarda el propio tipo —OpenAI y Gemini devuelven `None`— pero se
    /// afirma para que un cambio futuro tenga que romper un test.
    #[test]
    fn la_palanca_b_no_existe_fuera_de_anthropic() {
        let out = ANTHROPIC.prepare(
            incoming_with_body(r#"{"model":"x","messages":[]}"#),
            &test_config(false),
        );

        assert!(out.effort_forced.is_none(), "apagada por defecto");
    }

    /// REGRESIÓN de bytes: con la palanca apagada (default), el body
    /// reenviado debe ser BYTE-IDÉNTICO al original — ni siquiera pasa por
    /// una vuelta de reserializado, aunque `prepare` sí parsea el body para
    /// leer `model`/`stream`/`context`. Guarda la invariante 3 del contrato
    /// de `prepare`: parsear no es reserializar.
    #[test]
    fn does_not_inject_when_flag_disabled() {
        let cfg = test_config(false);
        let incoming = incoming_with_body(
            r#"{"model":"claude-3-5-sonnet","system":"eres un asistente","messages":[]}"#,
        );
        let original_body = incoming.body.clone();

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert!(!out.cache_control_forced);
        assert_eq!(out.body, original_body);
    }

    /// El dialecto de Anthropic NO tiene `tool_search` (mecanismo exclusivo de
    /// Responses/Codex): `prepare` debe dejar el campo en `None` de punta a
    /// punta, no solo el método del trait. Complementa la cobertura del método
    /// que ya hace `chat_tool_search_es_none` en `provider::openai`.
    #[test]
    fn prepare_deja_tool_search_en_none() {
        let cfg = test_config(false);
        let incoming = incoming_with_body(
            r#"{"model":"claude-3-5-sonnet","system":"hola","messages":[]}"#,
        );

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert_eq!(out.tool_search, None);
    }

    /// Body no-JSON: `prepare` no debe romper, solo reenviar intacto, marcar
    /// `cache_control_forced = false` y dejar `context` en `None` (no hay
    /// `Value` del que calcular ningún desglose).
    #[test]
    fn does_not_inject_on_invalid_json_body() {
        let cfg = test_config(true);
        let incoming = incoming_with_body("esto no es JSON");
        let original_body = incoming.body.clone();

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert!(!out.cache_control_forced);
        assert_eq!(out.body, original_body);
        assert_eq!(out.context, None);
    }

    /// Body JSON VÁLIDO pero no-objeto (array/escalar): no es indexable por
    /// clave, así que `prepare` debe reenviarlo intacto en vez de entrar en
    /// pánico. Antes rompía la petición; ahora se comporta como el no-JSON.
    #[test]
    fn does_not_inject_on_non_object_json_body() {
        let cfg = test_config(true);
        for body in [r#"[1,2,3]"#, r#""solo un string""#, r#"42"#, r#"true"#] {
            let incoming = incoming_with_body(body);
            let original_body = incoming.body.clone();

            let out = ANTHROPIC.prepare(incoming, &cfg);

            assert!(!out.cache_control_forced, "body {body} no debe forzar caché");
            assert_eq!(out.body, original_body, "body {body} debe reenviarse intacto");
            assert_eq!(out.context, None, "body {body} no debe producir desglose");
        }
    }

    /// `prepare` con un body Anthropic válido debe producir un `context`
    /// `Some`, y con números CONCRETOS calculados a mano sobre el fixture
    /// (no solo consistencia interna: `measured_bytes` podría "cerrar" con
    /// los cinco baldes aunque los cinco estuvieran mal en la misma
    /// dirección). Los tamaños esperados se obtuvieron con
    /// `serde_json::to_vec` fuera de este test sobre cada fragmento:
    /// `"hola"` → 6 bytes, `[]` → 2 bytes,
    /// `{"role":"user","content":"hi"}` → 30 bytes,
    /// `"claude-3-5-sonnet"` → 19 bytes.
    #[test]
    fn prepare_produce_context_con_numeros_concretos() {
        let cfg = test_config(false);
        let incoming = incoming_with_body(
            r#"{"model":"claude-3-5-sonnet","system":"hola","tools":[],"messages":[{"role":"user","content":"hi"}]}"#,
        );

        let out = ANTHROPIC.prepare(incoming, &cfg);
        let bd = out.context.expect("body válido debe producir contexto");

        assert_eq!(bd.system_bytes, 6);
        assert_eq!(bd.tools_bytes, 2);
        assert_eq!(bd.history_bytes, 0);
        assert_eq!(bd.last_turn_bytes, 30);
        assert_eq!(bd.other_bytes, 19);
        assert_eq!(bd.messages_count, 1);
        // Consistencia interna, a mayores del chequeo contra números concretos.
        assert_eq!(bd.measured_bytes, 57);
        let ratio = bd.context_tax_ratio().expect("measured_bytes > 0");
        assert!((ratio - (8.0 / 57.0)).abs() < 1e-9);
    }

    /// El refactor de "parsear una vez" no debe alterar `prompt_hash`: se
    /// calcula siempre sobre los bytes ORIGINALES, nunca sobre el `Value`
    /// parseado o reserializado. Lo verificamos calculando la huella de forma
    /// independiente (con la misma función pública) y comparándola contra la
    /// que produjo `prepare`.
    #[test]
    fn prepare_prompt_hash_se_calcula_sobre_bytes_originales() {
        let cfg = test_config(false);
        let raw = r#"{"model":"claude-3-5-sonnet","system":"hola","messages":[]}"#;
        let incoming = incoming_with_body(raw);
        let expected_hash = fingerprint(raw.as_bytes());

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert_eq!(out.prompt_hash, expected_hash);
        assert_eq!(out.prompt_bytes, raw.len());
    }

    /// Body realista: `system` string, `tools` con un esquema, y 3 mensajes.
    /// Cada balde debe coincidir con su fragmento y la suma debe cerrar con
    /// `measured_bytes`.
    #[test]
    fn decompose_body_realista_con_system_string() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "claude-3-5-sonnet",
                "max_tokens": 1024,
                "system": "eres un asistente que ayuda con código Rust",
                "tools": [{"name": "buscar", "input_schema": {"type": "object"}}],
                "messages": [
                    {"role": "user", "content": "hola"},
                    {"role": "assistant", "content": "hola, en qué te ayudo"},
                    {"role": "user", "content": "explicame ownership"}
                ]
            }"#,
        )
        .unwrap();

        let bd = ANTHROPIC.decompose(&body).expect("body es objeto");

        assert_eq!(bd.system_bytes, measure_value(&body["system"]));
        assert_eq!(bd.tools_bytes, measure_value(&body["tools"]));
        assert_eq!(bd.messages_count, 3);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            bd.history_bytes,
            measure_value(&messages[0]) + measure_value(&messages[1])
        );
        assert_eq!(bd.last_turn_bytes, measure_value(&messages[2]));
        assert_eq!(
            bd.other_bytes,
            measure_value(&body["model"]) + measure_value(&body["max_tokens"])
        );
        assert_eq!(
            bd.measured_bytes,
            bd.system_bytes + bd.tools_bytes + bd.history_bytes + bd.last_turn_bytes + bd.other_bytes
        );
    }

    /// `system` como array de bloques de contenido (con `cache_control`
    /// propio del cliente, por ejemplo): debe medirse igual que el string,
    /// sin distinción especial.
    #[test]
    fn decompose_system_como_array_de_bloques() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "claude-3-5-sonnet",
                "system": [{"type": "text", "text": "instrucciones largas", "cache_control": {"type": "ephemeral"}}],
                "messages": [{"role": "user", "content": "hola"}]
            }"#,
        )
        .unwrap();

        let bd = ANTHROPIC.decompose(&body).expect("body es objeto");

        assert_eq!(bd.system_bytes, measure_value(&body["system"]));
        assert!(bd.system_bytes > 0);
        assert_eq!(bd.messages_count, 1);
        assert_eq!(bd.history_bytes, 0);
        assert_eq!(bd.last_turn_bytes, measure_value(&body["messages"][0]));
    }

    /// Body que no es un objeto JSON (array): `decompose` debe devolver
    /// `None`, nunca panic.
    #[test]
    fn decompose_none_en_body_no_objeto() {
        let body: Value = serde_json::from_str("[1,2,3]").unwrap();
        assert_eq!(ANTHROPIC.decompose(&body), None);
    }

    /// `messages` ausente: ceros limpios en historial/turno, sin panic.
    #[test]
    fn decompose_messages_ausente() {
        let body: Value = serde_json::from_str(r#"{"model": "claude-3-5-sonnet"}"#).unwrap();
        let bd = ANTHROPIC.decompose(&body).expect("body es objeto");
        assert_eq!(bd.history_bytes, 0);
        assert_eq!(bd.last_turn_bytes, 0);
        assert_eq!(bd.messages_count, 0);
    }

    /// `messages` vacío: igual que ausente, ceros limpios.
    #[test]
    fn decompose_messages_vacio() {
        let body: Value =
            serde_json::from_str(r#"{"model": "claude-3-5-sonnet", "messages": []}"#).unwrap();
        let bd = ANTHROPIC.decompose(&body).expect("body es objeto");
        assert_eq!(bd.history_bytes, 0);
        assert_eq!(bd.last_turn_bytes, 0);
        assert_eq!(bd.messages_count, 0);
    }

    /// `tools` ausente: `tools_bytes = 0`, no `None`.
    #[test]
    fn decompose_tools_ausente_da_cero_no_none() {
        let body: Value = serde_json::from_str(
            r#"{"model": "claude-3-5-sonnet", "messages": [{"role": "user", "content": "hola"}]}"#,
        )
        .unwrap();
        let bd = ANTHROPIC.decompose(&body).expect("body es objeto");
        assert_eq!(bd.tools_bytes, 0);
    }

    /// Body realista con mezcla de herramientas nativas y de dos servidores
    /// MCP distintos. Los bytes esperados de cada bucket se calculan a mano
    /// con `measure_value` sobre los nodos del fixture, NO recomputando con
    /// `group_tools_by_server` (que es justo lo que se está probando): una
    /// aserción tautológica no valdría nada.
    #[test]
    fn tools_by_server_fixture_realista_con_nativas_y_mcp() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "claude-3-5-sonnet",
                "tools": [
                    {"name": "Read", "description": "lee un archivo"},
                    {"name": "Write", "description": "escribe un archivo"},
                    {"name": "mcp__claude_ai_Gmail__search_threads", "description": "busca hilos"},
                    {"name": "mcp__claude_ai_Gmail__get_message", "description": "trae un mensaje"},
                    {"name": "mcp__claude_ai_Google_Calendar__list_events", "description": "lista eventos"}
                ],
                "messages": [{"role": "user", "content": "hola"}]
            }"#,
        )
        .unwrap();
        let tools = body["tools"].as_array().unwrap();

        let by_server = ANTHROPIC.tools_by_server(&body);

        let native = by_server
            .iter()
            .find(|s| s.server == NATIVE_TOOLS_LABEL)
            .expect("debe existir el bucket nativo");
        assert_eq!(native.tools, 2);
        assert_eq!(
            native.bytes,
            measure_value(&tools[0]) + measure_value(&tools[1])
        );

        let gmail = by_server
            .iter()
            .find(|s| s.server == "claude_ai_Gmail")
            .expect("debe existir el bucket de Gmail");
        assert_eq!(gmail.tools, 2);
        assert_eq!(
            gmail.bytes,
            measure_value(&tools[2]) + measure_value(&tools[3])
        );

        let calendar = by_server
            .iter()
            .find(|s| s.server == "claude_ai_Google_Calendar")
            .expect("debe existir el bucket de Calendar");
        assert_eq!(calendar.tools, 1);
        assert_eq!(calendar.bytes, measure_value(&tools[4]));
    }

    /// `prepare` con un body Anthropic realista (2 herramientas nativas + 2
    /// MCP de servidores DISTINTOS) debe producir `tools_by_server` con las
    /// filas correctas y `tools_overhead_bytes` exacto. Los bytes esperados
    /// se derivan INDEPENDIENTEMENTE con `measure_value` sobre los nodos del
    /// fixture, nunca recomputando con el propio código bajo prueba (una
    /// aserción tautológica no valdría nada).
    #[test]
    fn prepare_produce_tools_by_server_con_numeros_concretos() {
        let cfg = test_config(false);
        let incoming = incoming_with_body(
            r#"{
                "model": "claude-3-5-sonnet",
                "tools": [
                    {"name": "Read", "description": "lee un archivo"},
                    {"name": "Write", "description": "escribe un archivo"},
                    {"name": "mcp__claude_ai_Gmail__search_threads", "description": "busca hilos"},
                    {"name": "mcp__claude_ai_Google_Calendar__list_events", "description": "lista eventos"}
                ],
                "messages": [{"role": "user", "content": "hola"}]
            }"#,
        );
        let body: Value = serde_json::from_slice(&incoming.body).unwrap();
        let tools = body["tools"].as_array().unwrap();

        let out = ANTHROPIC.prepare(incoming, &cfg);

        let expected_tools_bytes = measure_value(&body["tools"]);
        let expected_native = measure_value(&tools[0]) + measure_value(&tools[1]);
        let expected_gmail = measure_value(&tools[2]);
        let expected_calendar = measure_value(&tools[3]);
        let expected_sum = expected_native + expected_gmail + expected_calendar;

        let native = out
            .tools_by_server
            .iter()
            .find(|s| s.server == NATIVE_TOOLS_LABEL)
            .expect("debe existir el bucket nativo");
        assert_eq!(native.tools, 2);
        assert_eq!(native.bytes, expected_native);

        let gmail = out
            .tools_by_server
            .iter()
            .find(|s| s.server == "claude_ai_Gmail")
            .expect("debe existir el bucket de Gmail");
        assert_eq!(gmail.tools, 1);
        assert_eq!(gmail.bytes, expected_gmail);

        let calendar = out
            .tools_by_server
            .iter()
            .find(|s| s.server == "claude_ai_Google_Calendar")
            .expect("debe existir el bucket de Calendar");
        assert_eq!(calendar.tools, 1);
        assert_eq!(calendar.bytes, expected_calendar);

        assert_eq!(out.tools_overhead_bytes, expected_tools_bytes - expected_sum);
    }

    /// EXTREMO A EXTREMO por `prepare`: un body real con un servidor MCP
    /// totalmente diferido y OTRO servidor MCP sin diferir nada.
    /// `tools_by_server[i].deferred_tools`, por servidor, tiene que
    /// distinguir al servidor que no difirió nada del que sí
    /// (`docs/optimizer-tool-search.md`, defecto de revisión adversarial
    /// ronda 3).
    #[test]
    fn prepare_deferred_tools_por_servidor_en_body_mixto() {
        let cfg = test_config(false);
        let incoming = incoming_with_body(
            r#"{
                "model": "claude-opus-4",
                "tools": [
                    {"type": "tool_search_tool_bm25_20251119", "name": "tool_search_tool_bm25"},
                    {"name": "mcp__servidor_diferido__x", "defer_loading": true},
                    {"name": "mcp__servidor_diferido__y", "defer_loading": true},
                    {"name": "mcp__servidor_completo__z", "description": "esquema completo"}
                ],
                "messages": []
            }"#,
        );

        let out = ANTHROPIC.prepare(incoming, &cfg);

        let diferido = out
            .tools_by_server
            .iter()
            .find(|s| s.server == "servidor_diferido")
            .expect("servidor_diferido presente");
        assert_eq!(diferido.tools, 2);
        assert_eq!(diferido.deferred_tools, 2, "totalmente diferido");

        let completo = out
            .tools_by_server
            .iter()
            .find(|s| s.server == "servidor_completo")
            .expect("servidor_completo presente");
        assert_eq!(completo.tools, 1);
        assert_eq!(
            completo.deferred_tools, 0,
            "NADA diferido: sus bytes son reales y desconectables"
        );
    }

    /// Body no-JSON: `prepare` no debe romper; `tools_by_server` vacío,
    /// `tools_overhead_bytes` en cero, y el body reenviado BYTE-IDÉNTICO al
    /// original (mismo criterio que ya vale para `context`).
    #[test]
    fn prepare_tools_by_server_vacio_en_body_no_json() {
        let cfg = test_config(false);
        let incoming = incoming_with_body("esto no es JSON");
        let original_body = incoming.body.clone();

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert!(out.tools_by_server.is_empty());
        assert_eq!(out.tools_overhead_bytes, 0);
        assert_eq!(out.body, original_body);
    }

    /// `tools: []`: el body SÍ parseó como objeto pero declaró cero
    /// herramientas. A nivel de `Outgoing` esto es indistinguible de "no
    /// declaró tools en absoluto" (ambos dan vector vacío): la distinción
    /// `None`/`Some(vec![])` recién aparece en `RequestMetric`, no acá (ver
    /// `telemetry::logger::tools_fields`).
    ///
    /// `tools_overhead_bytes` NO da `0` acá: los corchetes `[]` del array
    /// vacío SÍ pesan (2 bytes), y `tools_overhead_bytes` los atribuye
    /// enteros al overhead porque no hay ningún servidor al que restárselos
    /// (`by_server` está vacío). Esto es consistente con el contrato ya
    /// documentado y probado de `super::tools_overhead_bytes` ("los
    /// corchetes y comas del array SÍ pesan algo"): un array vacío sigue
    /// siendo un array, con su propia estructura JSON.
    #[test]
    fn prepare_tools_by_server_vacio_cuando_tools_es_vacio() {
        let cfg = test_config(false);
        let incoming = incoming_with_body(r#"{"model":"claude-3-5-sonnet","tools":[],"messages":[]}"#);

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert!(out.tools_by_server.is_empty());
        assert_eq!(
            out.tools_overhead_bytes,
            measure_value(&serde_json::json!([]))
        );
    }

    /// Con `output_config.effort: "xhigh"` y `speed: "fast"` en la raíz,
    /// `prepare` debe capturar ambos como `Some`, y el body reenviado debe
    /// seguir siendo BYTE-IDÉNTICO al original (leer no es mutar).
    #[test]
    fn prepare_captura_effort_y_speed_cuando_estan_presentes() {
        let cfg = test_config(false);
        let incoming = incoming_with_body(
            r#"{"model":"claude-opus-4","output_config":{"effort":"xhigh"},"speed":"fast","messages":[]}"#,
        );
        let original_body = incoming.body.clone();

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert_eq!(out.requested_effort.as_deref(), Some("xhigh"));
        assert_eq!(out.requested_speed.as_deref(), Some("fast"));
        assert_eq!(out.body, original_body);
    }

    /// `output_config: {}` (presente pero sin `effort`) y sin `speed` en la
    /// raíz: ambos campos deben quedar en `None`, no en un string vacío ni en
    /// pánico.
    #[test]
    fn prepare_effort_y_speed_none_cuando_ausentes() {
        let cfg = test_config(false);
        let incoming = incoming_with_body(
            r#"{"model":"claude-opus-4","output_config":{},"messages":[]}"#,
        );

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert_eq!(out.requested_effort, None);
        assert_eq!(out.requested_speed, None);
    }

    /// `output_config.effort` presente pero de tipo NÚMERO, no string: debe
    /// dar `None`, nunca un pánico ni una conversión implícita a `"5"`.
    #[test]
    fn prepare_effort_none_cuando_no_es_string() {
        let cfg = test_config(false);
        let incoming = incoming_with_body(
            r#"{"model":"claude-opus-4","output_config":{"effort":5},"messages":[]}"#,
        );

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert_eq!(out.requested_effort, None);
    }

    /// `usage.speed` en un evento `message_start` (anidado bajo `message`)
    /// debe capturarse en `Usage.speed`; un evento equivalente sin `speed`
    /// debe dejarlo en `None`.
    #[test]
    fn extracts_served_speed_from_message_start() {
        let mut usage = Usage::default();
        let with_speed: Value = serde_json::from_str(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1,"speed":"fast"}}}"#,
        )
        .unwrap();

        ANTHROPIC.extract_usage(&with_speed, &mut usage);
        assert_eq!(usage.speed.as_deref(), Some("fast"));

        let mut usage_sin_speed = Usage::default();
        let without_speed: Value = serde_json::from_str(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}"#,
        )
        .unwrap();

        ANTHROPIC.extract_usage(&without_speed, &mut usage_sin_speed);
        assert_eq!(usage_sin_speed.speed, None);
    }

    /// La forma que hace posible todo el campo: en streaming el NOMBRE de la
    /// herramienta viaja entero en `content_block_start`. Lo único troceado
    /// entre eventos (`input_json_delta`) es el `input`, que no se mide.
    /// Fixture calcada del ejemplo publicado en la documentación de
    /// streaming de Anthropic.
    #[test]
    fn el_nombre_llega_entero_en_content_block_start() {
        let evento = serde_json::json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "type": "tool_use",
                "id": "toolu_01T1x1fJ34qAmk2tNTrN7Up6",
                "name": "get_weather",
                "input": {}
            }
        });

        let mut calls = ToolCalls::default();
        ANTHROPIC.extract_tool_use(&evento, &mut calls);

        assert_eq!(nombres(&calls), vec!["get_weather".to_string()]);
        assert!(calls.server_invoked.is_empty(), "no es de servidor");
    }

    /// `server_tool_use` es un tipo de bloque DISTINTO (IDs `srvtoolu_`) y
    /// va a su propia lista: lo ejecuta el proveedor, no el agente, así que
    /// no cuenta como uso de un servidor MCP del usuario. Mezclarlas
    /// inflaría el "sí lo usas" del recomendador con llamadas ajenas.
    #[test]
    fn las_de_servidor_van_a_su_propia_lista() {
        let evento = serde_json::json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "type": "server_tool_use",
                "id": "srvtoolu_014hJH82Qum7Td6UV8gDXThB",
                "name": "web_search",
                "input": {}
            }
        });

        let mut calls = ToolCalls::default();
        ANTHROPIC.extract_tool_use(&evento, &mut calls);

        assert!(calls.invoked.is_empty(), "no es de cliente");
        assert_eq!(calls.server_invoked, vec!["web_search".to_string()]);
    }

    /// No-streaming: el cuerpo entero trae los bloques en `content` de la
    /// raíz. Mismo método, misma salida — el proveedor cubre las dos formas
    /// igual que hace `extract_usage`.
    #[test]
    fn tambien_lee_el_cuerpo_completo_sin_streaming() {
        let cuerpo = serde_json::json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Miro el tiempo:"},
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {}},
                {"type": "tool_use", "id": "toolu_2", "name": "mcp__context7__get-docs", "input": {}}
            ],
            "stop_reason": "tool_use"
        });

        let mut calls = ToolCalls::default();
        ANTHROPIC.extract_tool_use(&cuerpo, &mut calls);

        assert_eq!(
            nombres(&calls),
            vec![
                "get_weather".to_string(),
                "mcp__context7__get-docs".to_string()
            ],
            "el bloque de texto no cuenta; los dos tool_use sí, en orden"
        );
    }

    /// REGRESIÓN DE DISEÑO. Un stream real empieza por `message_start`, que
    /// trae un `content` (vacío) anidado bajo `message`. Si el extractor
    /// mirase `content` en cualquier profundidad, o si no cortara tras la
    /// rama de streaming, cada invocación podría contarse dos veces — y una
    /// fila con el doble de llamadas no tiene ninguna pinta de estar mal.
    #[test]
    fn un_stream_completo_no_cuenta_dos_veces() {
        let eventos = [
            serde_json::json!({
                "type": "message_start",
                "message": {"id": "msg_01", "content": [], "role": "assistant"}
            }),
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
            serde_json::json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {
                    "type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}
                }
            }),
            serde_json::json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}
            }),
            serde_json::json!({"type": "content_block_stop", "index": 1}),
            serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "tool_use"},
                "usage": {"output_tokens": 89}
            }),
            serde_json::json!({"type": "message_stop"}),
        ];

        let mut calls = ToolCalls::default();
        for evento in &eventos {
            ANTHROPIC.extract_tool_use(evento, &mut calls);
        }

        assert_eq!(
            nombres(&calls),
            vec!["Read".to_string()],
            "una invocación en el stream es UNA en la fila"
        );
    }

    /// Las repeticiones se preservan: cuántas veces se llamó a una
    /// herramienta es un dato real del cable. Deduplicar al escribir lo
    /// perdería para siempre, y el histórico es lo que consumirá el
    /// recomendador.
    #[test]
    fn las_repeticiones_se_preservan() {
        let cuerpo = serde_json::json!({
            "content": [
                {"type": "tool_use", "id": "a", "name": "Read", "input": {}},
                {"type": "tool_use", "id": "b", "name": "Read", "input": {}},
                {"type": "tool_use", "id": "c", "name": "Grep", "input": {}}
            ]
        });

        let mut calls = ToolCalls::default();
        ANTHROPIC.extract_tool_use(&cuerpo, &mut calls);

        assert_eq!(
            nombres(&calls),
            vec!["Read".to_string(), "Read".to_string(), "Grep".to_string()]
        );
    }

    /// Discrimina por `type`, no por "tiene name". Un bloque futuro con
    /// `name` que no sea una invocación no debe colarse en la cuenta.
    #[test]
    fn un_bloque_con_name_que_no_es_invocacion_no_cuenta() {
        let cuerpo = serde_json::json!({
            "content": [
                {"type": "text", "text": "hola"},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "name": "inventado"},
                {"type": "thinking", "thinking": "", "signature": ""}
            ]
        });

        let mut calls = ToolCalls::default();
        ANTHROPIC.extract_tool_use(&cuerpo, &mut calls);

        assert!(calls.invoked.is_empty());
        assert!(calls.server_invoked.is_empty());
    }

    /// Mismas guardas que los nombres DECLARADOS: el nombre que llega en la
    /// respuesta es texto de fuera igual que el de la petición, y estas
    /// filas viven además en el buffer de 200 de `/requests`.
    #[test]
    fn las_guardas_de_tamano_valen_igual_en_la_respuesta() {
        let largo = "z".repeat(MAX_TOOL_NAME_LEN * 4);
        let mut bloques: Vec<serde_json::Value> = (0..MAX_TOOL_NAMES * 2)
            .map(|i| serde_json::json!({"type": "tool_use", "id": i, "name": "Read"}))
            .collect();
        bloques.push(serde_json::json!({"type": "tool_use", "id": "x", "name": largo}));
        let cuerpo = serde_json::json!({"content": bloques});

        let mut calls = ToolCalls::default();
        ANTHROPIC.extract_tool_use(&cuerpo, &mut calls);

        assert_eq!(calls.invoked.len(), MAX_TOOL_NAMES, "cupo de entradas");

        let mut solo_largo = ToolCalls::default();
        ANTHROPIC.extract_tool_use(
            &serde_json::json!({"content": [{"type": "tool_use", "name": "z".repeat(MAX_TOOL_NAME_LEN * 4)}]}),
            &mut solo_largo,
        );
        assert_eq!(
            solo_largo.invoked[0].name.chars().count(),
            MAX_TOOL_NAME_LEN,
            "cupo de longitud, contado en CARACTERES (cortar bytes UTF-8 haría panic)"
        );
    }

    /// Los dos cupos son independientes: un modelo que agote el de una
    /// lista no debe poder silenciar la otra.
    #[test]
    fn los_dos_cupos_no_se_comparten() {
        let mut bloques: Vec<serde_json::Value> = (0..MAX_TOOL_NAMES + 10)
            .map(|i| serde_json::json!({"type": "tool_use", "id": i, "name": "Read"}))
            .collect();
        bloques.push(serde_json::json!({"type": "server_tool_use", "name": "web_search"}));
        let cuerpo = serde_json::json!({"content": bloques});

        let mut calls = ToolCalls::default();
        ANTHROPIC.extract_tool_use(&cuerpo, &mut calls);

        assert_eq!(calls.invoked.len(), MAX_TOOL_NAMES);
        assert_eq!(
            calls.server_invoked,
            vec!["web_search".to_string()],
            "la de servidor entra aunque la de cliente esté a tope"
        );
    }

    /// REGRESIÓN. `mcp_tool_use` es el conector MCP server-side de Anthropic
    /// y cuenta como invocación de CLIENTE: sale de un servidor que el
    /// usuario configuró, justo lo que el recomendador mide. Descartarlo
    /// publicaría listas vacías en filas con decenas de llamadas, y el
    /// escape "mira el upstream" no salvaría el caso: el upstream es
    /// `anthropic` igual.
    #[test]
    fn el_conector_mcp_tambien_cuenta_como_invocacion() {
        let evento = serde_json::json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "type": "mcp_tool_use",
                "id": "mcptoolu_1",
                "name": "search_docs",
                "server_name": "remoto",
                "input": {}
            }
        });

        let mut calls = ToolCalls::default();
        ANTHROPIC.extract_tool_use(&evento, &mut calls);

        assert_eq!(nombres(&calls), vec!["search_docs".to_string()]);
        assert!(calls.server_invoked.is_empty(), "no es server_tool_use");
    }

    /// REGRESIÓN. El `status` de la fila NO dice si la respuesta se escaneó
    /// entera: se captura antes de que fluya el cuerpo, así que un turno
    /// abortado sale con 200. `complete` es la única señal, y solo la pone
    /// la marca de fin del dialecto.
    #[test]
    fn sin_message_stop_la_respuesta_queda_marcada_incompleta() {
        let a_medias = [
            serde_json::json!({"type": "message_start", "message": {"content": []}}),
            serde_json::json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {"type": "tool_use", "id": "t1", "name": "Read"}
            }),
        ];

        let mut calls = ToolCalls::default();
        for evento in &a_medias {
            ANTHROPIC.extract_tool_use(evento, &mut calls);
        }
        assert!(
            !calls.complete,
            "el turno se cortó: la lista es un prefijo, no un total"
        );

        ANTHROPIC.extract_tool_use(&serde_json::json!({"type": "message_stop"}), &mut calls);
        assert!(calls.complete, "message_stop cierra el escaneo");
        assert_eq!(
            nombres(&calls),
            vec!["Read".to_string()],
            "y no altera la lista"
        );
    }

    /// Un cuerpo no-streaming que llegó a parsearse ES la respuesta entera.
    #[test]
    fn el_cuerpo_completo_se_marca_completo() {
        let cuerpo = serde_json::json!({
            "id": "msg_1",
            "content": [{"type": "tool_use", "id": "t1", "name": "Read"}]
        });

        let mut calls = ToolCalls::default();
        ANTHROPIC.extract_tool_use(&cuerpo, &mut calls);

        assert!(calls.complete);
    }

    /// REGRESIÓN. El cupo recorta la LISTA pero nunca el contador, que es lo
    /// que delata el recorte — igual que `tool_names.len() < tools` lo
    /// delata en el lado declarado. Sin esto, una respuesta con 70 llamadas
    /// publicaría 64 y sería indistinguible de una con exactamente 64.
    #[test]
    fn el_truncado_queda_a_la_vista_en_el_contador() {
        let bloques: Vec<serde_json::Value> = (0..MAX_TOOL_NAMES + 6)
            .map(|i| serde_json::json!({"type": "tool_use", "id": i, "name": "Read"}))
            .collect();
        let cuerpo = serde_json::json!({"content": bloques});

        let mut calls = ToolCalls::default();
        ANTHROPIC.extract_tool_use(&cuerpo, &mut calls);

        assert_eq!(
            calls.invoked.len(),
            MAX_TOOL_NAMES,
            "la lista sí se recorta"
        );
        assert_eq!(
            calls.invoked_total,
            MAX_TOOL_NAMES + 6,
            "el contador NO: es lo que revela el recorte"
        );
        assert!(
            calls.invoked_total > calls.invoked.len(),
            "la comparacion que hara el consumidor: total > publicados"
        );
    }

    /// REGRESIÓN CRÍTICA. El conector MCP manda el nombre DESNUDO y el
    /// servidor en un campo hermano. Deducirlo del nombre lo mandaba a
    /// `(native)`, el servidor real salía con cero invocaciones, y el
    /// recomendador aconsejaba borrar justo el que se usa en cada turno.
    #[test]
    fn el_conector_mcp_se_atribuye_a_su_servidor_no_a_native() {
        let evento = serde_json::json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "type": "mcp_tool_use",
                "id": "mcptoolu_1",
                "name": "search_docs",
                "server_name": "remoto",
                "input": {}
            }
        });

        let mut calls = ToolCalls::default();
        ANTHROPIC.extract_tool_use(&evento, &mut calls);

        let llamada = &calls.invoked[0];
        assert_eq!(llamada.name, "search_docs", "el nombre viaja desnudo");
        assert_eq!(
            llamada.server, "remoto",
            "y el servidor sale de server_name, NO de deducirlo del nombre"
        );
        assert_eq!(llamada.kind, ToolServerKind::Mcp);
    }

    /// Sin `server_name` utilizable no se inventa un servidor: se cae a la
    /// deducción por nombre, que al menos es honesta sobre no saberlo.
    #[test]
    fn sin_server_name_el_conector_cae_a_la_deduccion_por_nombre() {
        for bloque in [
            serde_json::json!({"type": "mcp_tool_use", "name": "algo"}),
            serde_json::json!({"type": "mcp_tool_use", "name": "algo", "server_name": ""}),
            serde_json::json!({"type": "mcp_tool_use", "name": "algo", "server_name": 42}),
        ] {
            let mut calls = ToolCalls::default();
            ANTHROPIC.extract_tool_use(&serde_json::json!({"content_block": bloque}), &mut calls);
            assert_eq!(
                calls.invoked[0].kind,
                ToolServerKind::Native,
                "sin servidor legible, nativo — nunca un servidor inventado"
            );
        }
    }

    /// REGRESIÓN. El servidor se resuelve ANTES de truncar el nombre. Si se
    /// dedujera del nombre ya recortado, un servidor de más de 128 caracteres
    /// perdería su segundo `__` y caería en `(native)`, publicando `unused`
    /// para un servidor en uso.
    #[test]
    fn el_servidor_se_resuelve_antes_de_truncar_el_nombre() {
        // EXCEDE el tope a proposito. La version anterior de este test
        // usaba exactamente MAX_TOOL_NAME_LEN, justo el punto donde truncar
        // es un no-op: pasaba sin comprobar nada de lo que su nombre promete.
        let servidor_largo = "s".repeat(MAX_TOOL_NAME_LEN * 2);
        let nombre = format!("mcp__{servidor_largo}__una-herramienta");

        let mut calls = ToolCalls::default();
        ANTHROPIC.extract_tool_use(
            &serde_json::json!({
                "content_block": {"type": "tool_use", "id": "t1", "name": nombre}
            }),
            &mut calls,
        );

        let llamada = &calls.invoked[0];
        assert_eq!(
            llamada.name.chars().count(),
            MAX_TOOL_NAME_LEN,
            "el nombre sí se recorta"
        );
        assert_eq!(
            llamada.kind,
            ToolServerKind::Mcp,
            "pero la atribución NO se hace sobre el recorte del NOMBRE"
        );
        assert_eq!(
            llamada.server,
            crate::provider::etiqueta_servidor(&servidor_largo),
            "y la etiqueta se acota con el MISMO helper que el lado declarado"
        );
    }

    /// REGRESIÓN MEDIDA. La primera versión de `tool_calls` guardaba solo el
    /// nombre. Cambiar la forma sin aceptar la vieja hacía que `serde`
    /// fallara al parsear la fila ENTERA, y `rehydrate` la descartaba con sus
    /// tokens, coste y latencia — verificado contra un `telemetry.jsonl` real
    /// antes de este arreglo: «1 filas no se pudieron leer».
    #[test]
    fn una_fila_con_la_forma_vieja_de_invoked_sigue_entrando() {
        let vieja: ToolCalls = serde_json::from_str(
            r#"{"invoked":["mcp__context7__get-docs","Read"],
                "server_invoked":[],"invoked_total":2,
                "server_invoked_total":0,"complete":true}"#,
        )
        .expect("una fila de una build anterior tiene que entrar");

        assert_eq!(vieja.invoked.len(), 2);
        assert_eq!(vieja.invoked[0].name, "mcp__context7__get-docs");
        assert_eq!(
            vieja.invoked[0].server, "context7",
            "el servidor se deriva del nombre, que es lo que hacia el consumidor de entonces"
        );
        assert_eq!(vieja.invoked[0].kind, ToolServerKind::Mcp);
        assert_eq!(vieja.invoked[1].server, NATIVE_TOOLS_LABEL);
    }

    /// Y la forma actual, con el servidor ya resuelto, se lee tal cual — sin
    /// volver a derivarlo del nombre.
    #[test]
    fn la_forma_actual_conserva_el_servidor_resuelto() {
        let nueva: ToolCalls = serde_json::from_str(
            r#"{"invoked":[{"name":"buscar","server":"conector-remoto","kind":"mcp"}],
                "server_invoked":[],"invoked_total":1,
                "server_invoked_total":0,"complete":true}"#,
        )
        .expect("la forma actual tiene que entrar");

        assert_eq!(
            nueva.invoked[0].server, "conector-remoto",
            "NO se re-deriva de `buscar`, que daria (native)"
        );
        assert_eq!(nueva.invoked[0].kind, ToolServerKind::Mcp);
    }
}
