//! Proveedor OpenAI: dos variantes de API que comparten el mismo dialecto de
//! tokens pero difieren en URL, ruta e inyección de `include_usage`.
//!
//! - [`OpenAiChat`] cubre `/v1/chat/completions` (Chat Completions clásica).
//!   En streaming, OpenAI NO manda `usage` salvo que se pida explícitamente
//!   con `stream_options.include_usage = true`; sin esa inyección
//!   perderíamos los tokens de salida exactos.
//! - [`OpenAiResponses`] cubre `/v1/responses` (Responses API, la que usan
//!   clientes modernos como Codex). Ya reporta `usage` en el evento
//!   `response.completed` sin pedir nada: no inyecta.
//!
//! Ninguna de las dos variantes lee `Outgoing::requested_effort` ni
//! `Outgoing::requested_speed`: ambos son dialecto exclusivo de Anthropic
//! (`output_config.effort` y `speed` a nivel raíz), así que acá quedan
//! siempre en `None` a propósito (ver la nota en cada `prepare`).
use super::{
    array_field, fingerprint, measure_key, measure_other, measure_value, model_and_stream_from_value,
    parse_body, split_history_and_last_turn, tools_overhead_bytes, ContextBreakdown, Incoming,
    Outgoing, Provider, Usage,
};
use crate::config::AppConfig;
use serde_json::Value;

/// Adaptador de OpenAI Chat Completions (`/v1/chat/completions`).
pub struct OpenAiChat;

/// Adaptador de OpenAI Responses API (`/v1/responses`).
pub struct OpenAiResponses;

pub static OPENAI_CHAT: OpenAiChat = OpenAiChat;
pub static OPENAI_RESPONSES: OpenAiResponses = OpenAiResponses;

impl Provider for OpenAiChat {
    fn name(&self) -> &'static str {
        "openai"
    }

    /// Arma el request hacia `{openai}/chat/completions`. Parsea el body UNA
    /// sola vez ([`parse_body`]) y reutiliza el `Value` para leer
    /// `model`/`stream`, calcular `context` y, si el body pide streaming,
    /// inyectar `stream_options.include_usage = true` (ver
    /// [`inject_include_usage`]) para que el chunk final traiga `usage`.
    ///
    /// `prompt_hash`/`prompt_bytes` se calculan siempre sobre `incoming.body`
    /// ORIGINAL, nunca sobre el `Value` parseado.
    ///
    /// `tools_by_server`/`tools_overhead_bytes` salen del mismo `Value` ya
    /// parseado (nunca un segundo parseo): ver el contrato completo en
    /// `Anthropic::prepare`, idéntico acá.
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

        let body = if stream {
            inject_include_usage(incoming.body, parsed)
        } else {
            incoming.body
        };

        Outgoing {
            url: format!("{}/chat/completions", cfg.target_openai_url),
            route: "/v1/chat/completions".to_string(),
            upstream: self.name(),
            model,
            stream,
            prompt_hash,
            prompt_bytes,
            body,
            // OpenAI cachea el prefijo estable de forma automática (no hace
            // falta ningún breakpoint explícito): esta palanca no aplica acá.
            cache_control_forced: false,
            context,
            tools_by_server: by_server,
            tools_overhead_bytes: overhead,
            // `output_config.effort` y `speed` (raíz) son dialecto EXCLUSIVO
            // de Anthropic: Chat Completions no tiene un equivalente hoy. Se
            // deja `None` a propósito, en vez de heredar en silencio un
            // default, para que un futuro campo equivalente de OpenAI se
            // decida conscientemente acá y no se cuele por accidente.
            requested_effort: None,
            requested_speed: None,
        }
    }

    fn extract_usage(&self, value: &Value, usage: &mut Usage) {
        extract_openai_usage(value, usage);
    }

    /// Desglosa el body de `/v1/chat/completions`. A diferencia de
    /// Anthropic, acá NO hay un campo `system` a nivel raíz: el prompt de
    /// sistema es un mensaje más, con `role: "system"` (o `"developer"`, el
    /// alias que usan los modelos de razonamiento). Por eso el reparto es en
    /// dos pasadas sobre `messages`:
    /// 1. Los mensajes con `role` `system`/`developer` van íntegros a
    ///    `system_bytes` (sin importar en qué posición del array estén).
    /// 2. De los mensajes RESTANTES (los de conversación real), todos menos
    ///    el último van a `history_bytes` y el último a `last_turn_bytes`.
    ///
    /// `messages_count` es el total del array `messages` (incluye los de
    /// sistema): representa el tamaño real del payload conversacional, no
    /// solo la porción de historial/turno.
    ///
    /// Si TODOS los mensajes son `system`/`developer` (sin turno de usuario
    /// todavía), no queda nada para el segundo paso: `history_bytes = 0` y
    /// `last_turn_bytes = 0`, y el body entero de mensajes queda en
    /// `system_bytes`.
    fn decompose(&self, body: &Value) -> Option<ContextBreakdown> {
        let obj = body.as_object()?;
        let tools_bytes = measure_key(obj, "tools");
        let messages = array_field(obj, "messages");
        let messages_count = messages.len();

        let mut system_bytes = 0usize;
        let mut rest: Vec<&Value> = Vec::with_capacity(messages.len());
        for m in messages {
            let role = m.get("role").and_then(Value::as_str);
            if matches!(role, Some("system") | Some("developer")) {
                system_bytes += measure_value(m);
            } else {
                rest.push(m);
            }
        }
        let (history_bytes, last_turn_bytes, _) = split_history_and_last_turn(rest);

        let other_bytes = measure_other(obj, &["messages", "tools"]);

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

    /// Herramientas de `/v1/chat/completions`: `tools[]`, cada una
    /// `{type:"function", function:{name,...}}` ⇒ nombre en
    /// `tool["function"]["name"]` (ANIDADO bajo `function`, a diferencia de
    /// Responses). Si `tools` está AUSENTE, se tolera el array legado
    /// `functions[]` (nombre en `f["name"]`, sin anidar) que algunos
    /// clientes viejos todavía mandan.
    ///
    /// PRECEDENCIA: si `tools` está presente (aunque sea `[]`), se usa
    /// EXCLUSIVAMENTE `tools` y `functions` se ignora por completo, aunque
    /// también esté presente en el body (ambos dialectos no deberían
    /// coexistir en un request real, pero si pasara, `tools` es el vigente).
    fn tool_entries<'a>(&self, body: &'a Value) -> Option<Vec<(&'a str, &'a Value)>> {
        let obj = body.as_object()?;
        if let Some(tools) = obj.get("tools") {
            let tools = tools.as_array()?;
            return Some(
                tools
                    .iter()
                    .filter_map(|tool| {
                        let name = tool.get("function")?.get("name")?.as_str()?;
                        Some((name, tool))
                    })
                    .collect(),
            );
        }
        let functions = obj.get("functions")?.as_array()?;
        Some(
            functions
                .iter()
                .filter_map(|f| {
                    let name = f.get("name")?.as_str()?;
                    Some((name, f))
                })
                .collect(),
        )
    }
}

impl Provider for OpenAiResponses {
    fn name(&self) -> &'static str {
        "openai"
    }

    /// Arma el request hacia `{openai}/responses`. Modelo y `stream` van en
    /// el body igual que en Chat Completions, pero acá NO se inyecta nada:
    /// la Responses API ya manda `usage` por su cuenta. Parsea el body UNA
    /// sola vez ([`parse_body`]) y reutiliza el `Value` para `model`/`stream`,
    /// `context` y `tools_by_server`/`tools_overhead_bytes` (mismo contrato
    /// que `OpenAiChat::prepare`); el body reenviado es siempre
    /// `incoming.body` intacto (no hay mutación en esta variante).
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

        Outgoing {
            url: format!("{}/responses", cfg.target_openai_url),
            route: "/v1/responses".to_string(),
            upstream: self.name(),
            model,
            stream,
            prompt_hash,
            prompt_bytes,
            body: incoming.body,
            // Ídem: caché automática del lado de OpenAI, no aplica.
            cache_control_forced: false,
            context,
            tools_by_server: by_server,
            tools_overhead_bytes: overhead,
            // Ídem Chat Completions: `effort`/`speed` son dialecto exclusivo
            // de Anthropic, no aplica acá (ver esa nota para el contrato
            // completo).
            requested_effort: None,
            requested_speed: None,
        }
    }

    fn extract_usage(&self, value: &Value, usage: &mut Usage) {
        extract_openai_usage(value, usage);
    }

    /// Desglosa el body de `/v1/responses`. `instructions` → `system_bytes`
    /// (es el equivalente del `system` de Anthropic en este dialecto);
    /// `tools` → `tools_bytes` igual que en el resto de proveedores.
    ///
    /// `input` tiene DOS formas válidas en esta API y hay que manejar ambas:
    /// - String plano (el caso simple, un solo turno de texto): entra
    ///   ENTERO en `last_turn_bytes`, no hay historial (`history_bytes = 0`)
    ///   y `messages_count = 1` (un único "mensaje" implícito).
    /// - Array de items (turnos/mensajes estructurados, como en Chat
    ///   Completions): se reparte igual que `messages` en Anthropic, todos
    ///   menos el último a `history_bytes`, el último a `last_turn_bytes`.
    ///
    /// Si `input` está ausente o no es ninguna de las dos formas, se trata
    /// como vacío: ceros limpios, sin panic.
    fn decompose(&self, body: &Value) -> Option<ContextBreakdown> {
        let obj = body.as_object()?;
        let system_bytes = measure_key(obj, "instructions");
        let tools_bytes = measure_key(obj, "tools");

        let (history_bytes, last_turn_bytes, messages_count) = match obj.get("input") {
            Some(input @ Value::String(_)) => (0, measure_value(input), 1),
            Some(Value::Array(items)) => split_history_and_last_turn(items.iter()),
            _ => (0, 0, 0),
        };

        let other_bytes = measure_other(obj, &["instructions", "tools", "input"]);

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

    /// Herramientas de `/v1/responses`: `tools[]`, cada una
    /// `{type:"function", name, parameters,...}` ⇒ nombre en `tool["name"]`
    /// PLANO (a diferencia de Chat Completions, que lo anida bajo
    /// `function`). Esta asimetría entre las dos APIs de OpenAI es real, no
    /// un error de tipeo: está confirmada contra la forma del dialecto que
    /// ya usa `decompose` en esta misma variante (`input`/`instructions`
    /// también viven planos acá, sin anidar).
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

/// Inyecta `stream_options.include_usage = true` en el body JSON. Si el body
/// no es JSON válido, o es JSON válido pero no un objeto (array, string,
/// número…, no indexable por clave), lo devuelve intacto (sin tokens
/// exactos, pero sin romper el request ni arriesgar panic: preferimos
/// reenviar a fallar).
///
/// Toma `parsed`, el `Value` que YA parseó `prepare` a partir de `raw`: esta
/// función nunca vuelve a llamar a `serde_json::from_slice`.
fn inject_include_usage(raw: Vec<u8>, parsed: Option<Value>) -> Vec<u8> {
    let Some(mut value) = parsed else {
        return raw;
    };
    if !value.is_object() {
        return raw;
    }
    value["stream_options"]["include_usage"] = Value::Bool(true);
    serde_json::to_vec(&value).unwrap_or(raw)
}

/// Extractor compartido por ambas variantes de OpenAI: `usage` en la raíz
/// (Chat Completions) o anidado bajo `response` (Responses API, evento
/// `response.completed`). Campos: `prompt_tokens`/`completion_tokens`.
///
/// Los tokens de caché son SUBCONJUNTO del prompt/input (no se restan acá,
/// `input_tokens` se queda crudo). El nombre del campo anidado difiere por
/// variante: Chat Completions manda `prompt_tokens_details.cached_tokens`,
/// Responses manda `input_tokens_details.cached_tokens`; probamos ambos ya
/// que esta función es compartida. No hay cache-write en ninguna variante.
fn extract_openai_usage(value: &Value, usage: &mut Usage) {
    let Some(u) = value
        .get("usage")
        .or_else(|| value.get("response").and_then(|r| r.get("usage")))
    else {
        return;
    };

    if let Some(v) = u.get("prompt_tokens").and_then(Value::as_u64) {
        usage.input_tokens = Some(v);
    }
    if let Some(v) = u.get("completion_tokens").and_then(Value::as_u64) {
        usage.output_tokens = Some(v);
    }
    if let Some(v) = u
        .get("prompt_tokens_details")
        .or_else(|| u.get("input_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
    {
        usage.cache_read_tokens = Some(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::NATIVE_TOOLS_LABEL;

    /// OpenAI (con include_usage) manda el `usage` en el chunk final, con
    /// `prompt_tokens`/`completion_tokens` y `choices` vacío.
    #[test]
    fn extracts_openai_usage_from_sse() {
        let mut usage = Usage::default();
        let value: Value = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30}}"#,
        )
        .unwrap();

        OPENAI_CHAT.extract_usage(&value, &mut usage);

        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(20));
    }

    /// OpenAI Responses API (Codex): `usage` anidado bajo `response` en el
    /// evento `response.completed`. Comparte el extractor con Chat Completions.
    #[test]
    fn extracts_usage_from_responses_completed_event() {
        let mut usage = Usage::default();
        let value: Value = serde_json::from_str(
            r#"{"type":"response.completed","response":{"usage":{"prompt_tokens":4,"completion_tokens":6}}}"#,
        )
        .unwrap();

        OPENAI_RESPONSES.extract_usage(&value, &mut usage);

        assert_eq!(usage.input_tokens, Some(4));
        assert_eq!(usage.output_tokens, Some(6));
    }

    /// Chat Completions reporta la caché como subconjunto de `prompt_tokens`
    /// bajo `prompt_tokens_details.cached_tokens`.
    #[test]
    fn extracts_openai_chat_cache_read_tokens() {
        let mut usage = Usage::default();
        let value: Value = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":60}}}"#,
        )
        .unwrap();

        OPENAI_CHAT.extract_usage(&value, &mut usage);

        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.cache_read_tokens, Some(60));
    }

    /// Responses API reporta la caché bajo `input_tokens_details.cached_tokens`,
    /// también subconjunto del prompt/input.
    #[test]
    fn extracts_openai_responses_cache_read_tokens() {
        let mut usage = Usage::default();
        let value: Value = serde_json::from_str(
            r#"{"type":"response.completed","response":{"usage":{"prompt_tokens":50,"completion_tokens":10,"input_tokens_details":{"cached_tokens":30}}}}"#,
        )
        .unwrap();

        OPENAI_RESPONSES.extract_usage(&value, &mut usage);

        assert_eq!(usage.input_tokens, Some(50));
        assert_eq!(usage.cache_read_tokens, Some(30));
    }

    /// Chat Completions con un mensaje `system` al frente y 3 más de
    /// conversación: el `system` debe ir entero a `system_bytes`, y de los
    /// 3 restantes, los 2 primeros a historial y el último al turno nuevo.
    ///
    /// `other_bytes` se asegura EXACTAMENTE contra `measure_value(&body["model"])`
    /// (la única clave de raíz fuera de `messages`/`tools` en este fixture):
    /// esto también sirve de regresión, porque si alguien saca `"messages"`
    /// o `"tools"` de la exclude list de `measure_other`, esos bytes se
    /// contarían dos veces y la igualdad exacta deja de cumplirse.
    #[test]
    fn decompose_chat_con_system_y_tres_mensajes() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "gpt-4o",
                "tools": [{"type": "function", "function": {"name": "buscar"}}],
                "messages": [
                    {"role": "system", "content": "eres un asistente útil"},
                    {"role": "user", "content": "hola"},
                    {"role": "assistant", "content": "hola, en qué te ayudo"},
                    {"role": "user", "content": "explicame closures"}
                ]
            }"#,
        )
        .unwrap();

        let bd = OPENAI_CHAT.decompose(&body).expect("body es objeto");
        let messages = body["messages"].as_array().unwrap();

        assert_eq!(bd.system_bytes, measure_value(&messages[0]));
        assert_eq!(bd.tools_bytes, measure_value(&body["tools"]));
        assert_eq!(bd.other_bytes, measure_value(&body["model"]));
        assert_eq!(bd.messages_count, 4);
        assert_eq!(
            bd.history_bytes,
            measure_value(&messages[1]) + measure_value(&messages[2])
        );
        assert_eq!(bd.last_turn_bytes, measure_value(&messages[3]));
        assert_eq!(
            bd.measured_bytes,
            bd.system_bytes + bd.tools_bytes + bd.history_bytes + bd.last_turn_bytes + bd.other_bytes
        );
    }

    /// Si TODOS los mensajes son `system`/`developer` (sin turno de usuario
    /// aún), no debe quedar nada para historial/turno nuevo: todo va a
    /// `system_bytes`.
    #[test]
    fn decompose_chat_todos_los_mensajes_son_system() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "o1",
                "messages": [
                    {"role": "system", "content": "primera instrucción"},
                    {"role": "developer", "content": "segunda instrucción"}
                ]
            }"#,
        )
        .unwrap();

        let bd = OPENAI_CHAT.decompose(&body).expect("body es objeto");
        let messages = body["messages"].as_array().unwrap();

        assert_eq!(bd.history_bytes, 0);
        assert_eq!(bd.last_turn_bytes, 0);
        assert_eq!(bd.messages_count, 2);
        assert_eq!(
            bd.system_bytes,
            measure_value(&messages[0]) + measure_value(&messages[1])
        );
    }

    /// Un solo mensaje de usuario, sin `system`: todo el mensaje va a
    /// `last_turn_bytes`, sin historial.
    #[test]
    fn decompose_chat_un_solo_mensaje() {
        let body: Value = serde_json::from_str(
            r#"{"model": "gpt-4o", "messages": [{"role": "user", "content": "hola"}]}"#,
        )
        .unwrap();

        let bd = OPENAI_CHAT.decompose(&body).expect("body es objeto");
        assert_eq!(bd.history_bytes, 0);
        assert_eq!(bd.messages_count, 1);
        assert_eq!(bd.last_turn_bytes, measure_value(&body["messages"][0]));
    }

    /// `tools` ausente en Chat Completions: `tools_bytes = 0`, no `None`.
    #[test]
    fn decompose_chat_tools_ausente_da_cero() {
        let body: Value = serde_json::from_str(
            r#"{"model": "gpt-4o", "messages": [{"role": "user", "content": "hola"}]}"#,
        )
        .unwrap();
        let bd = OPENAI_CHAT.decompose(&body).expect("body es objeto");
        assert_eq!(bd.tools_bytes, 0);
    }

    /// Body no-objeto en Chat Completions: `None`, sin panic.
    #[test]
    fn decompose_chat_none_en_body_no_objeto() {
        let body: Value = serde_json::from_str(r#""solo un string""#).unwrap();
        assert_eq!(OPENAI_CHAT.decompose(&body), None);
    }

    /// Responses API con `input` como STRING plano: todo el input es el
    /// turno nuevo, sin historial, un solo "mensaje" implícito.
    ///
    /// `tools` está ausente en este fixture (debe dar `0`, no `None`), y
    /// `other_bytes` se asegura EXACTAMENTE contra `measure_value(&body["model"])`
    /// (la única clave de raíz fuera de `instructions`/`input` acá): si
    /// alguien saca `"input"` o `"instructions"` de la exclude list de
    /// `measure_other`, esos bytes se contarían dos veces y esta igualdad
    /// exacta deja de cumplirse.
    #[test]
    fn decompose_responses_con_input_string() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "gpt-4o",
                "instructions": "eres un asistente útil",
                "input": "explicame el patrón builder"
            }"#,
        )
        .unwrap();

        let bd = OPENAI_RESPONSES.decompose(&body).expect("body es objeto");

        assert_eq!(bd.system_bytes, measure_value(&body["instructions"]));
        assert_eq!(bd.tools_bytes, 0);
        assert_eq!(bd.other_bytes, measure_value(&body["model"]));
        assert_eq!(bd.history_bytes, 0);
        assert_eq!(bd.messages_count, 1);
        assert_eq!(bd.last_turn_bytes, measure_value(&body["input"]));
        assert_eq!(
            bd.measured_bytes,
            bd.system_bytes + bd.tools_bytes + bd.history_bytes + bd.last_turn_bytes + bd.other_bytes
        );
    }

    /// Responses API con `input` como ARRAY estructurado: se reparte igual
    /// que `messages` en el resto de los proveedores.
    ///
    /// `tools_bytes` y `other_bytes` se aseguran independientemente contra
    /// sus fragmentos crudos: si alguien saca `"input"` de la exclude list
    /// de `measure_other`, el array completo se contaría dos veces (como
    /// historial/turno Y como `other_bytes`) y la igualdad exacta de
    /// `other_bytes` deja de cumplirse.
    #[test]
    fn decompose_responses_con_input_array() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "gpt-4o",
                "instructions": "eres un asistente útil",
                "tools": [{"type": "function", "function": {"name": "buscar"}}],
                "input": [
                    {"role": "user", "content": "hola"},
                    {"role": "assistant", "content": "hola, en qué te ayudo"},
                    {"role": "user", "content": "explicame generics"}
                ]
            }"#,
        )
        .unwrap();

        let bd = OPENAI_RESPONSES.decompose(&body).expect("body es objeto");
        let input = body["input"].as_array().unwrap();

        assert_eq!(bd.tools_bytes, measure_value(&body["tools"]));
        assert_eq!(bd.other_bytes, measure_value(&body["model"]));
        assert_eq!(bd.messages_count, 3);
        assert_eq!(
            bd.history_bytes,
            measure_value(&input[0]) + measure_value(&input[1])
        );
        assert_eq!(bd.last_turn_bytes, measure_value(&input[2]));
        assert_eq!(
            bd.measured_bytes,
            bd.system_bytes + bd.tools_bytes + bd.history_bytes + bd.last_turn_bytes + bd.other_bytes
        );
    }

    /// Responses API sin `input`: ceros limpios, sin panic.
    #[test]
    fn decompose_responses_sin_input() {
        let body: Value =
            serde_json::from_str(r#"{"model": "gpt-4o", "instructions": "hola"}"#).unwrap();
        let bd = OPENAI_RESPONSES.decompose(&body).expect("body es objeto");
        assert_eq!(bd.history_bytes, 0);
        assert_eq!(bd.last_turn_bytes, 0);
        assert_eq!(bd.messages_count, 0);
    }

    /// Body no-objeto en Responses: `None`, sin panic.
    #[test]
    fn decompose_responses_none_en_body_no_objeto() {
        let body: Value = serde_json::from_str("42").unwrap();
        assert_eq!(OPENAI_RESPONSES.decompose(&body), None);
    }

    /// Construye un `AppConfig` mínimo para los tests de `prepare`, sin pasar
    /// por `AppConfig::load()` (que lee variables de entorno del proceso).
    fn test_config() -> AppConfig {
        AppConfig {
            local_port: 8080,
            target_openai_url: "https://api.openai.com/v1".to_string(),
            target_anthropic_url: "https://api.anthropic.com/v1".to_string(),
            target_gemini_url: "https://generativelanguage.googleapis.com".to_string(),
            storage_dir: std::path::PathBuf::from("/tmp/oxidegate-test"),
            force_prompt_cache: false,
        }
    }

    fn incoming_with_body(body: &str) -> Incoming {
        Incoming {
            path: "/v1/chat/completions".to_string(),
            query: None,
            body: body.as_bytes().to_vec(),
        }
    }

    /// REGRESIÓN de bytes (Chat Completions, invariante 3): con `stream`
    /// ausente/`false` no hay mutación posible (`inject_include_usage` ni se
    /// invoca), así que el body reenviado debe ser BYTE-IDÉNTICO al
    /// original, aunque `prepare` sí lo haya parseado para leer
    /// `model`/`context`.
    #[test]
    fn chat_prepare_no_muta_body_sin_stream() {
        let cfg = test_config();
        let incoming = incoming_with_body(
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hola"}]}"#,
        );
        let original_body = incoming.body.clone();

        let out = OPENAI_CHAT.prepare(incoming, &cfg);

        assert!(!out.stream);
        assert_eq!(out.body, original_body);
    }

    /// Body no-JSON en Chat Completions: `prepare` no debe romper, reenvía
    /// intacto y deja `context` en `None`.
    #[test]
    fn chat_prepare_body_no_json_no_panica() {
        let cfg = test_config();
        let incoming = incoming_with_body("esto no es JSON");
        let original_body = incoming.body.clone();

        let out = OPENAI_CHAT.prepare(incoming, &cfg);

        assert_eq!(out.body, original_body);
        assert_eq!(out.context, None);
        assert!(!out.stream);
        assert_eq!(out.model, None);
    }

    /// `prepare` con `stream: true` SÍ debe inyectar `stream_options.include_usage`,
    /// y por lo tanto el body reenviado difiere del original (mutación
    /// deliberada, la única excepción a la invariante de bytes intactos).
    #[test]
    fn chat_prepare_inyecta_include_usage_con_stream() {
        let cfg = test_config();
        let incoming = incoming_with_body(
            r#"{"model":"gpt-4o","stream":true,"messages":[{"role":"user","content":"hola"}]}"#,
        );

        let out = OPENAI_CHAT.prepare(incoming, &cfg);

        assert!(out.stream);
        let body: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    /// `prepare` con un body Chat Completions válido produce un `context`
    /// `Some` con números CONCRETOS (no solo consistencia interna),
    /// calculados a mano con `serde_json::to_vec` sobre cada fragmento del
    /// fixture: mensaje `system` → 38 bytes, mensaje `user` → 30 bytes,
    /// `tools: []` → 2 bytes, `"gpt-4o"` → 8 bytes.
    #[test]
    fn chat_prepare_produce_context_con_numeros_concretos() {
        let cfg = test_config();
        let incoming = incoming_with_body(
            r#"{"model":"gpt-4o","tools":[],"messages":[{"role":"system","content":"be brief"},{"role":"user","content":"hi"}]}"#,
        );

        let out = OPENAI_CHAT.prepare(incoming, &cfg);
        let bd = out.context.expect("body válido debe producir contexto");

        assert_eq!(bd.system_bytes, 38);
        assert_eq!(bd.tools_bytes, 2);
        assert_eq!(bd.history_bytes, 0);
        assert_eq!(bd.last_turn_bytes, 30);
        assert_eq!(bd.other_bytes, 8);
        assert_eq!(bd.messages_count, 2);
        assert_eq!(bd.measured_bytes, 78);
    }

    /// El refactor de "parsear una vez" no debe alterar `prompt_hash`: se
    /// calcula siempre sobre los bytes originales.
    #[test]
    fn chat_prepare_prompt_hash_se_calcula_sobre_bytes_originales() {
        let cfg = test_config();
        let raw = r#"{"model":"gpt-4o","messages":[]}"#;
        let incoming = incoming_with_body(raw);
        let expected_hash = fingerprint(raw.as_bytes());

        let out = OPENAI_CHAT.prepare(incoming, &cfg);

        assert_eq!(out.prompt_hash, expected_hash);
        assert_eq!(out.prompt_bytes, raw.len());
    }

    /// REGRESIÓN de bytes (Responses API, invariante 3): esta variante nunca
    /// muta el body, así que el reenviado debe ser SIEMPRE byte-idéntico al
    /// original, con o sin streaming.
    #[test]
    fn responses_prepare_nunca_muta_body() {
        let cfg = test_config();
        let incoming = incoming_with_body(
            r#"{"model":"gpt-4o","stream":true,"input":"hola"}"#,
        );
        let original_body = incoming.body.clone();

        let out = OPENAI_RESPONSES.prepare(incoming, &cfg);

        assert_eq!(out.body, original_body);
    }

    /// Body no-JSON en Responses: `prepare` no debe romper, reenvía intacto
    /// y deja `context` en `None`.
    #[test]
    fn responses_prepare_body_no_json_no_panica() {
        let cfg = test_config();
        let incoming = incoming_with_body("esto no es JSON");
        let original_body = incoming.body.clone();

        let out = OPENAI_RESPONSES.prepare(incoming, &cfg);

        assert_eq!(out.body, original_body);
        assert_eq!(out.context, None);
    }

    /// `prepare` con un body Responses válido (`input` string) produce un
    /// `context` `Some` con números CONCRETOS: `"be helpful"` → 12 bytes,
    /// `"explain the builder pattern"` → 29 bytes, `"gpt-4o"` → 8 bytes.
    #[test]
    fn responses_prepare_produce_context_con_numeros_concretos() {
        let cfg = test_config();
        let incoming = incoming_with_body(
            r#"{"model":"gpt-4o","instructions":"be helpful","input":"explain the builder pattern"}"#,
        );

        let out = OPENAI_RESPONSES.prepare(incoming, &cfg);
        let bd = out.context.expect("body válido debe producir contexto");

        assert_eq!(bd.system_bytes, 12);
        assert_eq!(bd.tools_bytes, 0);
        assert_eq!(bd.history_bytes, 0);
        assert_eq!(bd.last_turn_bytes, 29);
        assert_eq!(bd.other_bytes, 8);
        assert_eq!(bd.messages_count, 1);
        assert_eq!(bd.measured_bytes, 49);
    }

    /// Chat Completions: body realista con mezcla de herramienta nativa y
    /// dos de un mismo servidor MCP. Bytes esperados calculados a mano con
    /// `measure_value` sobre los nodos del fixture (no recomputando con
    /// `group_tools_by_server`, que es lo que se está probando).
    #[test]
    fn chat_tools_by_server_fixture_realista() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "gpt-4o",
                "tools": [
                    {"type": "function", "function": {"name": "Read", "description": "lee"}},
                    {"type": "function", "function": {"name": "mcp__claude_ai_Gmail__search_threads", "description": "busca"}},
                    {"type": "function", "function": {"name": "mcp__claude_ai_Gmail__get_message", "description": "trae"}}
                ],
                "messages": [{"role": "user", "content": "hola"}]
            }"#,
        )
        .unwrap();
        let tools = body["tools"].as_array().unwrap();

        let by_server = OPENAI_CHAT.tools_by_server(&body);

        let native = by_server
            .iter()
            .find(|s| s.server == NATIVE_TOOLS_LABEL)
            .expect("debe existir el bucket nativo");
        assert_eq!(native.tools, 1);
        assert_eq!(native.bytes, measure_value(&tools[0]));

        let gmail = by_server
            .iter()
            .find(|s| s.server == "claude_ai_Gmail")
            .expect("debe existir el bucket de Gmail");
        assert_eq!(gmail.tools, 2);
        assert_eq!(
            gmail.bytes,
            measure_value(&tools[1]) + measure_value(&tools[2])
        );
    }

    /// Responses API: body realista con mezcla de herramienta nativa y dos
    /// de un mismo servidor MCP, con el nombre PLANO (`tool["name"]`, sin
    /// anidar bajo `function`).
    #[test]
    fn responses_tools_by_server_fixture_realista() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "gpt-4o",
                "tools": [
                    {"type": "function", "name": "Read", "parameters": {}},
                    {"type": "function", "name": "mcp__claude_ai_Google_Drive__search_files", "parameters": {}},
                    {"type": "function", "name": "mcp__claude_ai_Google_Drive__list_recent_files", "parameters": {}}
                ],
                "input": "hola"
            }"#,
        )
        .unwrap();
        let tools = body["tools"].as_array().unwrap();

        let by_server = OPENAI_RESPONSES.tools_by_server(&body);

        let native = by_server
            .iter()
            .find(|s| s.server == NATIVE_TOOLS_LABEL)
            .expect("debe existir el bucket nativo");
        assert_eq!(native.tools, 1);
        assert_eq!(native.bytes, measure_value(&tools[0]));

        let drive = by_server
            .iter()
            .find(|s| s.server == "claude_ai_Google_Drive")
            .expect("debe existir el bucket de Drive");
        assert_eq!(drive.tools, 2);
        assert_eq!(
            drive.bytes,
            measure_value(&tools[1]) + measure_value(&tools[2])
        );
    }

    /// Sin `tools`, se tolera el array legado `functions[]` (nombre PLANO,
    /// sin anidar bajo `function`).
    #[test]
    fn chat_tool_entries_legacy_functions_cuando_tools_ausente() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "gpt-4o",
                "functions": [{"name": "Read"}, {"name": "mcp__srv__tool"}]
            }"#,
        )
        .unwrap();

        let entries = OPENAI_CHAT.tool_entries(&body).expect("functions presente");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "Read");
        assert_eq!(entries[1].0, "mcp__srv__tool");
    }

    /// Si AMBOS `tools` y `functions` están presentes, `tools` tiene
    /// precedencia absoluta: `functions` se ignora por completo.
    #[test]
    fn chat_tool_entries_tools_tiene_precedencia_sobre_functions() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "gpt-4o",
                "tools": [{"type": "function", "function": {"name": "Write"}}],
                "functions": [{"name": "Read"}]
            }"#,
        )
        .unwrap();

        let entries = OPENAI_CHAT.tool_entries(&body).expect("tools presente");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "Write");
    }
}
