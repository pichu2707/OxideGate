//! Proveedor Google Gemini: modelo y método viven en la URL, no en el body.
//!
//! A diferencia de Anthropic/OpenAI, la ruta es comodín (`/v1beta/*`) y hay
//! que preservar path + query originales (que llevan `alt=sse` y a veces la
//! API key) al reenviar hacia el host de Gemini.
use super::{
    array_field, fingerprint, measure_key, measure_other, parse_body, split_history_and_last_turn,
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
    ///
    /// COSTO NUEVO Y CONSCIENTE: a diferencia de Anthropic/OpenAI, Gemini no
    /// necesitaba parsear el body para nada (modelo y `stream` viven en el
    /// path). Para calcular `context` acá SÍ hace falta parsearlo una vez
    /// ([`parse_body`]): es un costo de CPU nuevo en este camino, agregado a
    /// propósito para tener el mismo desglose que el resto de proveedores. Si
    /// el body no parsea como JSON (o no es un objeto), `context` queda en
    /// `None` y el body se reenvía intacto igual que siempre: el parseo es
    /// de solo lectura, nunca reserializa ni muta nada acá.
    fn prepare(&self, incoming: Incoming, cfg: &AppConfig) -> Outgoing {
        let (model, stream) = parse_gemini_path(&incoming.path);
        let context = parse_body(&incoming.body).and_then(|v| self.decompose(&v));

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
            context,
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

    /// Herramientas de Gemini: `tools[]`, cada elemento
    /// `{functionDeclarations:[{name,...}]}`. Las entradas medidas son las
    /// declaraciones INDIVIDUALES (`functionDeclarations[i]`), no el
    /// wrapper que las contiene: cada declaración es la unidad que un
    /// servidor MCP registraría como una herramienta propia.
    ///
    /// Un elemento de `tools[]` sin `functionDeclarations` (o con un valor
    /// que no es array) se OMITE por completo, sin afectar al resto de los
    /// elementos.
    fn tool_entries<'a>(&self, body: &'a Value) -> Option<Vec<(&'a str, &'a Value)>> {
        let tools = body.as_object()?.get("tools")?.as_array()?;
        Some(
            tools
                .iter()
                .filter_map(|wrapper| wrapper.get("functionDeclarations").and_then(Value::as_array))
                .flatten()
                .filter_map(|decl| {
                    let name = decl.get("name")?.as_str()?;
                    Some((name, decl))
                })
                .collect(),
        )
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
    use super::super::{measure_value, NATIVE_TOOLS_LABEL};
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

    fn incoming_with_body(path: &str, body: &str) -> Incoming {
        Incoming {
            path: path.to_string(),
            query: None,
            body: body.as_bytes().to_vec(),
        }
    }

    /// REGRESIÓN de bytes (invariante 3): Gemini nunca muta el body, así que
    /// aunque `prepare` ahora lo parsea (costo nuevo, ver doc de `prepare`)
    /// para calcular `context`, el body reenviado debe seguir siendo
    /// BYTE-IDÉNTICO al original.
    #[test]
    fn prepare_no_muta_el_body() {
        let cfg = test_config();
        let incoming = incoming_with_body(
            "/v1beta/models/gemini-1.5-flash:generateContent",
            r#"{"contents":[{"role":"user","parts":[{"text":"hola"}]}]}"#,
        );
        let original_body = incoming.body.clone();

        let out = GEMINI.prepare(incoming, &cfg);

        assert_eq!(out.body, original_body);
    }

    /// Body no-JSON: `prepare` no debe romper, reenvía intacto y deja
    /// `context` en `None` (modelo/stream siguen viniendo del path, no del
    /// body, así que no se ven afectados).
    #[test]
    fn prepare_body_no_json_no_panica() {
        let cfg = test_config();
        let incoming = incoming_with_body(
            "/v1beta/models/gemini-1.5-flash:generateContent",
            "esto no es JSON",
        );
        let original_body = incoming.body.clone();

        let out = GEMINI.prepare(incoming, &cfg);

        assert_eq!(out.body, original_body);
        assert_eq!(out.context, None);
        assert_eq!(out.model.as_deref(), Some("gemini-1.5-flash"));
    }

    /// `prepare` con un body Gemini válido produce un `context` `Some` con
    /// números CONCRETOS (no solo consistencia interna), calculados a mano
    /// con `serde_json::to_vec` sobre cada fragmento del fixture:
    /// `{"parts":[{"text":"be helpful"}]}` → 33 bytes,
    /// `{"role":"user","parts":[{"text":"hi"}]}` → 39 bytes.
    #[test]
    fn prepare_produce_context_con_numeros_concretos() {
        let cfg = test_config();
        let incoming = incoming_with_body(
            "/v1beta/models/gemini-1.5-flash:generateContent",
            r#"{"systemInstruction":{"parts":[{"text":"be helpful"}]},"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
        );

        let out = GEMINI.prepare(incoming, &cfg);
        let bd = out.context.expect("body válido debe producir contexto");

        assert_eq!(bd.system_bytes, 33);
        assert_eq!(bd.tools_bytes, 0);
        assert_eq!(bd.history_bytes, 0);
        assert_eq!(bd.last_turn_bytes, 39);
        assert_eq!(bd.other_bytes, 0);
        assert_eq!(bd.messages_count, 1);
        assert_eq!(bd.measured_bytes, 72);
    }

    /// El refactor de "parsear una vez" no debe alterar `prompt_hash`.
    #[test]
    fn prepare_prompt_hash_se_calcula_sobre_bytes_originales() {
        let cfg = test_config();
        let raw = r#"{"contents":[]}"#;
        let incoming = incoming_with_body(
            "/v1beta/models/gemini-1.5-flash:generateContent",
            raw,
        );
        let expected_hash = fingerprint(raw.as_bytes());

        let out = GEMINI.prepare(incoming, &cfg);

        assert_eq!(out.prompt_hash, expected_hash);
        assert_eq!(out.prompt_bytes, raw.len());
    }

    /// Body realista con mezcla de herramienta nativa y dos de un mismo
    /// servidor MCP, repartidas entre dos wrappers `functionDeclarations`
    /// distintos. Bytes esperados calculados a mano con `measure_value`
    /// sobre las declaraciones del fixture (no recomputando con
    /// `group_tools_by_server`, que es lo que se está probando).
    #[test]
    fn tools_by_server_fixture_realista_con_nativas_y_mcp() {
        let body: Value = serde_json::from_str(
            r#"{
                "tools": [
                    {"functionDeclarations": [
                        {"name": "Read", "description": "lee"},
                        {"name": "mcp__claude_ai_Google_Calendar__list_events", "description": "lista"}
                    ]},
                    {"functionDeclarations": [
                        {"name": "mcp__claude_ai_Google_Calendar__create_event", "description": "crea"}
                    ]}
                ],
                "contents": [{"role": "user", "parts": [{"text": "hola"}]}]
            }"#,
        )
        .unwrap();
        let decl_0 = &body["tools"][0]["functionDeclarations"];
        let decl_1 = &body["tools"][1]["functionDeclarations"];

        let by_server = GEMINI.tools_by_server(&body);

        let native = by_server
            .iter()
            .find(|s| s.server == NATIVE_TOOLS_LABEL)
            .expect("debe existir el bucket nativo");
        assert_eq!(native.tools, 1);
        assert_eq!(native.bytes, measure_value(&decl_0[0]));

        let calendar = by_server
            .iter()
            .find(|s| s.server == "claude_ai_Google_Calendar")
            .expect("debe existir el bucket de Calendar");
        assert_eq!(calendar.tools, 2);
        assert_eq!(
            calendar.bytes,
            measure_value(&decl_0[1]) + measure_value(&decl_1[0])
        );
    }

    /// Un elemento de `tools[]` sin `functionDeclarations` (o con un valor
    /// que no es array) se omite por completo, sin afectar al resto.
    #[test]
    fn tool_entries_omite_wrapper_sin_function_declarations_validas() {
        let body: Value = serde_json::from_str(
            r#"{
                "tools": [
                    {"functionDeclarations": [{"name": "Read"}]},
                    {"otroCampo": true},
                    {"functionDeclarations": "no es un array"}
                ]
            }"#,
        )
        .unwrap();

        let entries = GEMINI.tool_entries(&body).expect("tools es un array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "Read");
    }
}
