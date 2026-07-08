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
use super::{fingerprint, model_and_stream_from_body, Incoming, Outgoing, Provider, Usage};
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

    /// Arma el request hacia `{openai}/chat/completions`. Si el body pide
    /// streaming, inyecta `stream_options.include_usage = true` para que el
    /// chunk final traiga `usage`.
    fn prepare(&self, incoming: Incoming, cfg: &AppConfig) -> Outgoing {
        let prompt_hash = fingerprint(&incoming.body);
        let prompt_bytes = incoming.body.len();
        let (model, stream) = model_and_stream_from_body(&incoming.body);

        let body = if stream {
            inject_include_usage(incoming.body)
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
        }
    }

    fn extract_usage(&self, value: &Value, usage: &mut Usage) {
        extract_openai_usage(value, usage);
    }
}

impl Provider for OpenAiResponses {
    fn name(&self) -> &'static str {
        "openai"
    }

    /// Arma el request hacia `{openai}/responses`. Modelo y `stream` van en
    /// el body igual que en Chat Completions, pero acá NO se inyecta nada:
    /// la Responses API ya manda `usage` por su cuenta.
    fn prepare(&self, incoming: Incoming, cfg: &AppConfig) -> Outgoing {
        let prompt_hash = fingerprint(&incoming.body);
        let prompt_bytes = incoming.body.len();
        let (model, stream) = model_and_stream_from_body(&incoming.body);

        Outgoing {
            url: format!("{}/responses", cfg.target_openai_url),
            route: "/v1/responses".to_string(),
            upstream: self.name(),
            model,
            stream,
            prompt_hash,
            prompt_bytes,
            body: incoming.body,
        }
    }

    fn extract_usage(&self, value: &Value, usage: &mut Usage) {
        extract_openai_usage(value, usage);
    }
}

/// Inyecta `stream_options.include_usage = true` en el body JSON. Si el body
/// no es JSON válido, lo devuelve intacto (sin tokens exactos, pero sin
/// romper el request: preferimos reenviar a fallar).
fn inject_include_usage(raw: Vec<u8>) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<Value>(&raw) else {
        return raw;
    };
    value["stream_options"]["include_usage"] = Value::Bool(true);
    serde_json::to_vec(&value).unwrap_or(raw)
}

/// Extractor compartido por ambas variantes de OpenAI: `usage` en la raíz
/// (Chat Completions) o anidado bajo `response` (Responses API, evento
/// `response.completed`). Campos: `prompt_tokens`/`completion_tokens`.
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
