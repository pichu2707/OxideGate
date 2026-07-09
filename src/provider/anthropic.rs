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
use super::{
    array_field, fingerprint, measure_key, measure_other, model_and_stream_from_body,
    split_history_and_last_turn, ContextBreakdown, Incoming, Outgoing, Provider, Usage,
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

    /// Arma el request hacia `{anthropic}/messages`. Lee `model`/`stream` para
    /// la métrica y, si `cfg.force_prompt_cache` está activo, intenta inyectar
    /// un breakpoint de `cache_control` (ver [`force_cache_control`]).
    fn prepare(&self, incoming: Incoming, cfg: &AppConfig) -> Outgoing {
        let (model, stream) = model_and_stream_from_body(&incoming.body);
        let prompt_hash = fingerprint(&incoming.body);
        let prompt_bytes = incoming.body.len();

        let (body, cache_control_forced) = if cfg.force_prompt_cache {
            force_cache_control(incoming.body)
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
        }
    }

    /// `usage` vive en la raíz (evento `message_delta`) o anidado bajo
    /// `message` (evento `message_start`). El conteo de salida es
    /// acumulativo entre eventos: "último gana".
    ///
    /// Anthropic reporta la caché APARTE de `input_tokens`:
    /// `cache_read_input_tokens` (lectura) y `cache_creation_input_tokens`
    /// (escritura) se guardan crudos, sin tocar `input_tokens`.
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
fn force_cache_control(raw: Vec<u8>) -> (Vec<u8>, bool) {
    let Ok(mut value) = serde_json::from_slice::<Value>(&raw) else {
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
    use super::super::measure_value;

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

    /// Con la palanca apagada (default), no se inyecta nada aunque el body
    /// no traiga ningún `cache_control`.
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

    /// Body no-JSON: `prepare` no debe romper, solo reenviar intacto y
    /// marcar `cache_control_forced = false`.
    #[test]
    fn does_not_inject_on_invalid_json_body() {
        let cfg = test_config(true);
        let incoming = incoming_with_body("esto no es JSON");
        let original_body = incoming.body.clone();

        let out = ANTHROPIC.prepare(incoming, &cfg);

        assert!(!out.cache_control_forced);
        assert_eq!(out.body, original_body);
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
        }
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
}
