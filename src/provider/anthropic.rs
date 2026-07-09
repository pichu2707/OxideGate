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
    Provider, Usage,
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

        let (body, cache_control_forced) = if cfg.force_prompt_cache {
            force_cache_control(incoming.body, parsed)
        } else {
            (incoming.body, false)
        };

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
            tools_by_server: by_server,
            tools_overhead_bytes: overhead,
            requested_effort,
            requested_speed,
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
fn force_cache_control(raw: Vec<u8>, parsed: Option<Value>) -> (Vec<u8>, bool) {
    let Some(mut value) = parsed else {
        return (raw, false);
    };

    // Solo los objetos JSON son indexables por clave: inyectar en un array o
    // escalar entraría en pánico. Además, si el cliente ya cachea, no tocamos.
    if !value.is_object() || has_cache_control(&value) {
        return (raw, false);
    }

    value["cache_control"] = serde_json::json!({"type": "ephemeral"});
    match serde_json::to_vec(&value) {
        Ok(body) => (body, true),
        Err(_) => (raw, false),
    }
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
    use super::super::{measure_value, NATIVE_TOOLS_LABEL};

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
        AppConfig {
            local_port: 8080,
            target_openai_url: "https://api.openai.com/v1".to_string(),
            target_anthropic_url: "https://api.anthropic.com/v1".to_string(),
            target_gemini_url: "https://generativelanguage.googleapis.com".to_string(),
            storage_dir: std::path::PathBuf::from("/tmp/oxidegate-test"),
            force_prompt_cache,
        }
    }

    fn incoming_with_body(body: &str) -> Incoming {
        Incoming {
            path: "/v1/messages".to_string(),
            query: None,
            body: body.as_bytes().to_vec(),
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
}
