//! Proveedor Google Gemini: modelo y método viven en la URL, no en el body.
//!
//! A diferencia de Anthropic/OpenAI, la ruta es comodín (`/v1beta/*`) y hay
//! que preservar path + query originales (que llevan `alt=sse` y a veces la
//! API key) al reenviar hacia el host de Gemini.
use super::{
    array_field, fingerprint, measure_key, measure_other, split_history_and_last_turn,
    ContextBreakdown, Incoming, Outgoing, Provider, Usage,
};
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
            // La caché de Gemini se gestiona aparte (implícita o explícita
            // vía `cachedContent`), no con esta palanca: no aplica acá.
            cache_control_forced: false,
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

    /// Desglosa el body de `generateContent`/`streamGenerateContent`.
    /// `systemInstruction` → `system_bytes`; `tools` → `tools_bytes`;
    /// `contents` → todo menos el último a `history_bytes`, el último a
    /// `last_turn_bytes`, igual que `messages` en Anthropic.
    ///
    /// `None` solo si `body` no es un objeto JSON: nunca hace panic.
    fn decompose(&self, body: &Value) -> Option<ContextBreakdown> {
        let obj = body.as_object()?;

        let system_bytes = measure_key(obj, "systemInstruction");
        let tools_bytes = measure_key(obj, "tools");
        let contents = array_field(obj, "contents");
        let (history_bytes, last_turn_bytes, messages_count) =
            split_history_and_last_turn(contents.iter());
        let other_bytes = measure_other(obj, &["systemInstruction", "tools", "contents"]);

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
    use super::super::measure_value;
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

    /// Body realista con `systemInstruction` + `tools` + `generationConfig`
    /// (root extra, ver más abajo) + 3 `contents`: cada balde debe coincidir
    /// con su fragmento y la suma debe cerrar con `measured_bytes`.
    ///
    /// `generationConfig` está a propósito para que `other_bytes` deje de
    /// ser cero por construcción (sin esta clave el fixture no tenía NINGÚN
    /// campo de raíz fuera de `systemInstruction`/`tools`/`contents`, así que
    /// `other_bytes` daba 0 sin que la aserción probara nada real). Al
    /// asertar `other_bytes` EXACTAMENTE contra `measure_value` de esa única
    /// clave, este test también funciona como regresión: si alguien saca
    /// `"contents"` de la exclude list de `measure_other`, `contents` se
    /// contaría dos veces (como historial/turno Y como `other_bytes`) y la
    /// igualdad exacta deja de cumplirse.
    #[test]
    fn decompose_body_realista() {
        let body: Value = serde_json::from_str(
            r#"{
                "systemInstruction": {"parts": [{"text": "eres un asistente útil"}]},
                "tools": [{"functionDeclarations": [{"name": "buscar"}]}],
                "generationConfig": {"temperature": 0.7, "maxOutputTokens": 1024},
                "contents": [
                    {"role": "user", "parts": [{"text": "hola"}]},
                    {"role": "model", "parts": [{"text": "hola, en qué te ayudo"}]},
                    {"role": "user", "parts": [{"text": "explicame traits"}]}
                ]
            }"#,
        )
        .unwrap();

        let bd = GEMINI.decompose(&body).expect("body es objeto");
        let contents = body["contents"].as_array().unwrap();

        assert_eq!(bd.system_bytes, measure_value(&body["systemInstruction"]));
        assert_eq!(bd.tools_bytes, measure_value(&body["tools"]));
        assert_eq!(bd.other_bytes, measure_value(&body["generationConfig"]));
        assert_eq!(bd.messages_count, 3);
        assert_eq!(
            bd.history_bytes,
            measure_value(&contents[0]) + measure_value(&contents[1])
        );
        assert_eq!(bd.last_turn_bytes, measure_value(&contents[2]));
        assert_eq!(
            bd.measured_bytes,
            bd.system_bytes + bd.tools_bytes + bd.history_bytes + bd.last_turn_bytes + bd.other_bytes
        );
    }

    /// `contents` ausente: ceros limpios, sin panic.
    #[test]
    fn decompose_contents_ausente() {
        let body: Value = serde_json::from_str(r#"{"tools": []}"#).unwrap();
        let bd = GEMINI.decompose(&body).expect("body es objeto");
        assert_eq!(bd.history_bytes, 0);
        assert_eq!(bd.last_turn_bytes, 0);
        assert_eq!(bd.messages_count, 0);
    }

    /// Un solo elemento en `contents`: todo va a `last_turn_bytes`.
    #[test]
    fn decompose_contents_un_solo_elemento() {
        let body: Value = serde_json::from_str(
            r#"{"contents": [{"role": "user", "parts": [{"text": "hola"}]}]}"#,
        )
        .unwrap();
        let bd = GEMINI.decompose(&body).expect("body es objeto");
        assert_eq!(bd.history_bytes, 0);
        assert_eq!(bd.messages_count, 1);
        assert_eq!(bd.last_turn_bytes, measure_value(&body["contents"][0]));
    }

    /// Body no-objeto: `None`, sin panic.
    #[test]
    fn decompose_none_en_body_no_objeto() {
        let body: Value = serde_json::from_str(r#"[1,2,3]"#).unwrap();
        assert_eq!(GEMINI.decompose(&body), None);
    }
}
