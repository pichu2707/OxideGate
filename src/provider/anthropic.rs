//! Proveedor Anthropic (Claude): ruta fija, modelo y `stream` en el body.
//!
//! A diferencia de OpenAI, Anthropic ya manda `usage` con cada evento SSE
//! por defecto: no hace falta pedir nada extra ni mutar el body saliente.
use super::{fingerprint, model_and_stream_from_body, Incoming, Outgoing, Provider, Usage};
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

    /// Arma el request hacia `{anthropic}/messages`. No muta el body: solo
    /// lee `model`/`stream` para la métrica.
    fn prepare(&self, incoming: Incoming, cfg: &AppConfig) -> Outgoing {
        let (model, stream) = model_and_stream_from_body(&incoming.body);
        Outgoing {
            url: format!("{}/messages", cfg.target_anthropic_url),
            route: "/v1/messages".to_string(),
            upstream: self.name(),
            model,
            stream,
            prompt_hash: fingerprint(&incoming.body),
            prompt_bytes: incoming.body.len(),
            body: incoming.body,
        }
    }

    /// `usage` vive en la raíz (evento `message_delta`) o anidado bajo
    /// `message` (evento `message_start`). El conteo de salida es
    /// acumulativo entre eventos: "último gana".
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
