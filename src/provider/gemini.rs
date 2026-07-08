//! Proveedor Google Gemini: modelo y método viven en la URL, no en el body.
//!
//! A diferencia de Anthropic/OpenAI, la ruta es comodín (`/v1beta/*`) y hay
//! que preservar path + query originales (que llevan `alt=sse` y a veces la
//! API key) al reenviar hacia el host de Gemini.
use super::{fingerprint, Incoming, Outgoing, Provider, Usage};
use crate::config::AppConfig;
use serde_json::Value;

/// Adaptador del dialecto Gemini (`/v1beta/models/{model}:{método}`).
pub struct Gemini;

pub static GEMINI: Gemini = Gemini;

impl Provider for Gemini {
    fn name(&self) -> &'static str {
        "gemini"
    }

    /// Preserva path y query originales sobre el host de Gemini. No muta el
    /// body: Gemini ya reporta `usageMetadata` por defecto en el stream SSE,
    /// no hay nada que inyectar.
    fn prepare(&self, incoming: Incoming, cfg: &AppConfig) -> Outgoing {
        let (model, stream) = parse_gemini_path(&incoming.path);

        let mut url = format!("{}{}", cfg.target_gemini_url, incoming.path);
        if let Some(query) = &incoming.query {
            url.push('?');
            url.push_str(query);
        }

        Outgoing {
            url,
            route: incoming.path,
            upstream: self.name(),
            model,
            stream,
            prompt_hash: fingerprint(&incoming.body),
            prompt_bytes: incoming.body.len(),
            body: incoming.body,
        }
    }

    /// `usageMetadata` en la raíz, con `promptTokenCount`/`candidatesTokenCount`.
    /// Nota: los tokens de "thinking" (`thoughtsTokenCount`) van aparte y aún
    /// no se suman acá.
    ///
    /// `cachedContentTokenCount` es SUBCONJUNTO de `promptTokenCount` (no se
    /// resta acá: `input_tokens` se queda crudo, tal como lo valida el
    /// ground-truth del CLI). Gemini no reporta cache-write en
    /// `generateContent`.
    fn extract_usage(&self, value: &Value, usage: &mut Usage) {
        let Some(u) = value.get("usageMetadata") else {
            return;
        };

        if let Some(v) = u.get("promptTokenCount").and_then(Value::as_u64) {
            usage.input_tokens = Some(v);
        }
        if let Some(v) = u.get("candidatesTokenCount").and_then(Value::as_u64) {
            usage.output_tokens = Some(v);
        }
        if let Some(v) = u.get("cachedContentTokenCount").and_then(Value::as_u64) {
            usage.cache_read_tokens = Some(v);
        }
    }
}

/// Extrae `(modelo, es_stream)` del path de Gemini.
///
/// El path tiene la forma `/v1beta/models/{model}:{método}`, donde el método
/// `streamGenerateContent` indica streaming y `generateContent` no.
fn parse_gemini_path(path: &str) -> (Option<String>, bool) {
    match path.split("/models/").nth(1) {
        Some(tail) => {
            let mut it = tail.splitn(2, ':');
            let model = it.next().filter(|s| !s.is_empty()).map(str::to_string);
            let stream = it.next().unwrap_or("").contains("stream");
            (model, stream)
        }
        None => (None, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gemini (`alt=sse`) manda `usageMetadata` con otros nombres de campo,
    /// acumulativo en el chunk final. El extractor debe mapearlo.
    #[test]
    fn extracts_gemini_usage_from_sse() {
        let mut usage = Usage::default();
        let value: Value = serde_json::from_str(
            r#"{"candidates":[{"content":{"parts":[{"text":"hola"}]}}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":3,"totalTokenCount":14}}"#,
        )
        .unwrap();

        GEMINI.extract_usage(&value, &mut usage);

        assert_eq!(usage.input_tokens, Some(11));
        assert_eq!(usage.output_tokens, Some(3));
    }

    /// `cachedContentTokenCount` es subconjunto de `promptTokenCount`: se
    /// captura crudo en `cache_read_tokens` sin restarlo de `input_tokens`.
    #[test]
    fn extracts_gemini_cache_read_tokens() {
        let mut usage = Usage::default();
        let value: Value = serde_json::from_str(
            r#"{"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":20,"cachedContentTokenCount":80}}"#,
        )
        .unwrap();

        GEMINI.extract_usage(&value, &mut usage);

        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.cache_read_tokens, Some(80));
    }

    /// El método `streamGenerateContent` en la URL indica streaming; el
    /// modelo se lee del segmento entre `/models/` y `:`.
    #[test]
    fn parses_model_and_stream_from_gemini_path() {
        let (model, stream) =
            parse_gemini_path("/v1beta/models/gemini-1.5-flash:streamGenerateContent");
        assert_eq!(model.as_deref(), Some("gemini-1.5-flash"));
        assert!(stream);
    }

    /// Sin `streamGenerateContent`, la respuesta no es streaming.
    #[test]
    fn parses_non_stream_gemini_path() {
        let (model, stream) = parse_gemini_path("/v1beta/models/gemini-1.5-flash:generateContent");
        assert_eq!(model.as_deref(), Some("gemini-1.5-flash"));
        assert!(!stream);
    }
}
