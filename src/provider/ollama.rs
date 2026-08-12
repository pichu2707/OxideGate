//! Dialecto NATIVO de ollama: `/api/generate` y `/api/chat`.
//!
//! # Qué aporta sobre el endpoint OpenAI-compatible
//!
//! Ollama expone un endpoint OpenAI-compatible que el proxy ya sabe medir,
//! pero ese endpoint publica **solo contadores de tokens**: `prompt_tokens`,
//! `completion_tokens` y `total_tokens`. Verificado sobre tráfico real.
//!
//! Su API nativa reporta además **cómo se repartió el tiempo dentro del
//! motor**, y eso es lo único que permite separar cargar el modelo de inferir
//! con él. Medido a través del proxy, con el modelo frío:
//!
//! ```text
//! load_us          1.451 ms   ← el 57% de la peticion
//! prompt_eval_us      23 ms
//! eval_us          1.070 ms
//! total_ms         2.548 ms
//! ```
//!
//! Sin esas tres cifras, `ttft_ms` mezcla la carga con el procesado del prompt
//! y no hay forma de saber cuánto fue cada cosa.
//!
//! Importa para lo que viene: **los vatios-hora por token exigen excluir la
//! carga**. Y aparte, el tráfico nativo de ollama **no se medía en absoluto**
//! — el proxy solo veía el camino OpenAI-compatible.
//!
//! # Corrección: cargar cuesta tiempo, no vatios
//!
//! Una versión anterior de este comentario decía que una petición fría
//! inflaría la cuenta por token «unas 2,5 veces». **Es falso**: convertía una
//! proporción de TIEMPO en una afirmación sobre ENERGÍA sin medirla.
//!
//! Medido con una petición que es 98% carga: la ventana dibuja **43,0 W** de
//! media, contra los **~189 W** que dibuja la misma tarjeta generando. Cargar
//! mueve memoria, no calcula. Sobre `qwen2.5:7b` con 200 tokens fijos, la
//! carga fue el **54% del tiempo** y el **11% de la energía atribuible**: la
//! cuenta por token se infló un **17%**, no dos veces y media.
//!
//! Excluirla sigue haciendo falta —con respuestas cortas la proporción
//! crece— pero por ese motivo, no por el que estaba escrito.
//!
//! # Lo que este dialecto NO arregla
//!
//! **No corrige ningún error en `tokens_per_sec`.** Una versión anterior de
//! esta documentación lo afirmaba y era falso; se verificó contra el sistema.
//!
//! En streaming, `tokens_per_sec` se calcula como `salida / (total_ms −
//! ttft_ms)`, y el **`ttft_ms` ya absorbe la carga del modelo**: el primer
//! chunk no llega hasta que el modelo está cargado. Medido en la misma
//! petición de arriba: `ttft_ms` 1.477 ms contra `load_us + prompt_eval_us`
//! 1.474 ms, y la velocidad publicada fue **126,1 tok/s frente a 126,2
//! reales**. Fuera de streaming el campo es `None`, no un número malo.
//!
//! El 42% de error que se citaba salía de dividir por el reloj de pared a
//! mano, no de lo que el proxy publica.
//!
//! # NDJSON, no SSE
//!
//! Ollama nativo **hace streaming por defecto** —al revés que OpenAI— y su
//! stream es NDJSON: un objeto JSON por línea, sin prefijo `data:`. Los
//! totales viajan en la ÚLTIMA línea, la que trae `done: true`.
//!
//! Antes de [`Provider::payload_de_linea`] el escáner exigía `data:` a todo el
//! mundo, así que contra este dialecto habría ignorado cada línea y publicado
//! **cero tokens en silencio** — indistinguible de una respuesta sin tokens.
use super::{
    ContextBreakdown, Incoming, Outgoing, Provider, ToolCalls, Usage, array_field, fingerprint,
    measure_key, measure_other, parse_body, split_history_and_last_turn,
};
use crate::config::AppConfig;
use serde_json::Value;

/// Proveedor del dialecto nativo de ollama.
pub struct Ollama;

/// Instancia única, igual que el resto de proveedores.
pub static OLLAMA: Ollama = Ollama;

/// Nanosegundos → microsegundos. Ollama reporta en ns; el resto de la
/// telemetría de tiempo propio del proxy (`prepare_us`, `scan_us`) va en µs, y
/// mezclar unidades en la misma fila es una trampa que se paga al leerla.
fn ns_a_us(v: Option<&Value>) -> Option<u64> {
    Some(v?.as_u64()? / 1_000)
}

impl Provider for Ollama {
    fn name(&self) -> &'static str {
        "ollama"
    }

    fn prepare(&self, incoming: Incoming, cfg: &AppConfig) -> Outgoing {
        let parsed = parse_body(&incoming.body);
        let context = parsed.as_ref().and_then(|v| self.decompose(v));
        // El bloque de instrucciones y el listado de skills se buscan igual
        // que en el resto de dialectos: quien manda el body puede ser
        // cualquier harness apuntando a un modelo local.
        let skills = parsed
            .as_ref()
            .and_then(crate::provider::skills::detect_skills_in_body);
        let instructions = parsed
            .as_ref()
            .and_then(crate::provider::instructions::detect_instructions_in_body)
            .map(|b| b.publicable(cfg.instructions_headings));
        let by_server = parsed
            .as_ref()
            .map(|v| self.tools_by_server(v))
            .unwrap_or_default();
        let overhead = context
            .as_ref()
            .map(|c| super::tools_overhead_bytes(c.tools_bytes, &by_server))
            .unwrap_or(0);

        let model = parsed
            .as_ref()
            .and_then(|v| v.get("model"))
            .and_then(Value::as_str)
            .map(str::to_string);

        // **El default de `stream` es TRUE**, al revés que en OpenAI. Darlo
        // por `false` haría que el escáner tratase un NDJSON entero como un
        // solo JSON y no encontrase el `usage` de nadie.
        let stream = parsed
            .as_ref()
            .and_then(|v| v.get("stream"))
            .and_then(Value::as_bool)
            .unwrap_or(true);

        Outgoing {
            url: format!("{}{}", cfg.target_ollama_url, incoming.path),
            route: incoming.path,
            upstream: self.name(),
            model,
            stream,
            prompt_hash: fingerprint(&incoming.body),
            prompt_bytes: incoming.body.len(),
            body: incoming.body,
            // Un motor local no tiene prompt caching que forzar.
            cache_control_forced: false,
            context,
            skills,
            instructions,
            // La marca `hook success:` es de Claude Code hablando Anthropic.
            hooks: None,
            effort_forced: None,
            tools_by_server: by_server,
            tools_overhead_bytes: overhead,
            requested_effort: None,
            requested_speed: None,
            tool_search: None,
            tools_flattened: None,
        }
    }

    /// Los contadores y las tres duraciones viajan en el MISMO objeto: el
    /// último del stream (`done: true`) o el cuerpo entero si no hay stream.
    ///
    /// Semántica «último gana», igual que el resto de dialectos: las líneas
    /// intermedias no traen estos campos, así que no pisan nada.
    fn extract_usage(&self, value: &Value, usage: &mut Usage) {
        if let Some(v) = value.get("prompt_eval_count").and_then(Value::as_u64) {
            usage.input_tokens = Some(v);
        }
        if let Some(v) = value.get("eval_count").and_then(Value::as_u64) {
            usage.output_tokens = Some(v);
        }
        // Un motor local no sirve desde caché de proveedor: se dejan en `None`
        // a propósito, que significa «no lo reporta», no «fue cero».
        if let Some(us) = ns_a_us(value.get("load_duration")) {
            usage.load_us = Some(us);
        }
        if let Some(us) = ns_a_us(value.get("prompt_eval_duration")) {
            usage.prompt_eval_us = Some(us);
        }
        if let Some(us) = ns_a_us(value.get("eval_duration")) {
            usage.eval_us = Some(us);
        }
    }

    /// Vacío: ollama soporta herramientas en `/api/chat`, pero **no se ha
    /// capturado una invocación real contra este dialecto**. Publicar un
    /// extractor sin haberlo visto funcionar dejaría filas diciendo «no se
    /// invocó nada» de forma indistinguible de la verdad — que es justo lo
    /// que `captura_invocaciones` existe para declarar.
    fn extract_tool_use(&self, _value: &Value, _calls: &mut ToolCalls) {}

    fn captura_invocaciones(&self) -> bool {
        false
    }

    /// **NDJSON**: la línea ENTERA es el JSON, sin prefijo. Se descartan las
    /// vacías; el resto lo valida `serde` aguas arriba.
    fn payload_de_linea<'a>(&self, linea: &'a str) -> Option<&'a str> {
        let t = linea.trim();
        if t.is_empty() { None } else { Some(t) }
    }

    /// Reparte el body por componente. Los dos endpoints tienen forma
    /// distinta y se tratan distinto:
    ///
    /// - `/api/chat` trae `messages`: el último es el turno nuevo, el resto
    ///   historial — igual criterio que el resto de dialectos conversacionales.
    /// - `/api/generate` trae `prompt` suelto y opcionalmente `system`. Sin
    ///   conversación, todo el prompt ES el último turno y no hay historial.
    fn decompose(&self, body: &Value) -> Option<ContextBreakdown> {
        let obj = body.as_object()?;

        let system_bytes = measure_key(obj, "system");
        let tools_bytes = measure_key(obj, "tools");

        // `/api/chat` trae `messages` y se reparte con el MISMO criterio que
        // el resto de dialectos conversacionales —helper compartido, para que
        // "qué es historial" no dependa del proveedor—.
        //
        // `/api/generate` no tiene conversación: trae un `prompt` suelto, así
        // que todo él ES el turno nuevo y el historial es CERO medido, no un
        // hueco. Inventar historial ahí falsearía `context_history_bytes`.
        let messages = array_field(obj, "messages");
        let (history_bytes, last_turn_bytes, messages_count) = if messages.is_empty() {
            (0, measure_key(obj, "prompt"), 0)
        } else {
            split_history_and_last_turn(messages.iter())
        };

        let other_bytes = measure_other(obj, &["system", "tools", "messages", "prompt"]);

        Some(ContextBreakdown {
            system_bytes,
            tools_bytes,
            history_bytes,
            last_turn_bytes,
            other_bytes,
            measured_bytes: system_bytes
                + tools_bytes
                + history_bytes
                + last_turn_bytes
                + other_bytes,
            messages_count,
        })
    }

    /// `/api/chat` acepta `tools` con la MISMA forma que OpenAI: un array de
    /// `{"type":"function","function":{"name":…}}`.
    fn tool_entries<'a>(&self, body: &'a Value) -> Option<Vec<(&'a str, &'a Value)>> {
        let tools = body.get("tools")?.as_array()?;
        Some(
            tools
                .iter()
                .filter_map(|t| {
                    let nombre = t
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .or_else(|| t.get("name"))
                        .and_then(Value::as_str)?;
                    Some((nombre, t))
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Última línea REAL de un stream de `/api/generate`, recortada a los
    /// campos que se consumen.
    const FINAL_REAL: &str = r#"{"model":"llama3.2:3b","done":true,"done_reason":"stop",
        "total_duration":4488000000,"load_duration":1853000000,
        "prompt_eval_count":46,"prompt_eval_duration":21000000,
        "eval_count":320,"eval_duration":2613000000}"#;

    #[test]
    fn extrae_tokens_y_las_tres_duraciones_del_motor() {
        let v: Value = serde_json::from_str(FINAL_REAL).expect("json");
        let mut u = Usage::default();

        OLLAMA.extract_usage(&v, &mut u);

        assert_eq!(u.input_tokens, Some(46));
        assert_eq!(u.output_tokens, Some(320));
        // ns -> us, para no mezclar unidades con `prepare_us`/`scan_us`.
        assert_eq!(u.load_us, Some(1_853_000));
        assert_eq!(u.prompt_eval_us, Some(21_000));
        assert_eq!(u.eval_us, Some(2_613_000));
    }

    /// **EL DATO QUE JUSTIFICA EL DIALECTO**: separar la carga del modelo de
    /// la inferencia. Con el modelo frío la carga puede ser la mayor parte de
    /// la petición, y sin `load_us` no hay forma de saber cuánto fue.
    ///
    /// Importa para los vatios-hora por token: medido sobre `qwen2.5:7b` con
    /// 200 tokens fijos, incluir la carga infla la cifra un **17%**. No más,
    /// porque cargar mueve memoria y no calcula: 43 W de media contra los
    /// ~189 W de generar (ver el doc del módulo).
    #[test]
    fn load_us_separa_la_carga_del_modelo_de_la_inferencia() {
        let v: Value = serde_json::from_str(FINAL_REAL).expect("json");
        let mut u = Usage::default();
        OLLAMA.extract_usage(&v, &mut u);

        let total_us = 4_488_000.0;
        let fraccion_carga = u.load_us.unwrap() as f64 / total_us;

        assert!(
            fraccion_carga > 0.40,
            "en esta captura la carga fue la mayor parte: {:.0}%",
            fraccion_carga * 100.0
        );
        // Y la generación se puede medir aparte, sin la carga dentro.
        let generando = u.output_tokens.unwrap() as f64 / (u.eval_us.unwrap() as f64 / 1e6);
        assert!(
            (generando - 122.5).abs() < 1.0,
            "tok/s generando: {generando}"
        );
    }

    /// **NO se corrige ningún error de `tokens_per_sec`**, y conviene que un
    /// test lo fije porque la documentación llegó a afirmar lo contrario.
    ///
    /// En streaming el proxy calcula `salida / (total − ttft)`, y el `ttft` ya
    /// incluye la carga: el primer chunk no sale hasta que el modelo está
    /// cargado. Sobre la captura real, `ttft` fue 1.477 ms y
    /// `load + prompt_eval` 1.474 — la misma cosa.
    #[test]
    fn el_tramo_de_generacion_del_proxy_ya_coincide_con_el_motor() {
        let v: Value = serde_json::from_str(FINAL_REAL).expect("json");
        let mut u = Usage::default();
        OLLAMA.extract_usage(&v, &mut u);

        // Lo que el proxy llama "tramo de generación": total menos TTFT.
        let ttft_us = u.load_us.unwrap() + u.prompt_eval_us.unwrap();
        let tramo_del_proxy = 4_488_000 - ttft_us;

        let diferencia = (tramo_del_proxy as i64 - u.eval_us.unwrap() as i64).abs();
        assert!(
            diferencia < 20_000,
            "el tramo del proxy y el `eval_us` del motor miden lo mismo: \
             {tramo_del_proxy} us contra {} us",
            u.eval_us.unwrap()
        );
    }

    /// Las líneas intermedias del stream no traen totales: no deben pisar lo
    /// ya visto con `None`.
    #[test]
    fn una_linea_intermedia_no_borra_lo_ya_medido() {
        let mut u = Usage::default();
        OLLAMA.extract_usage(
            &serde_json::from_str::<Value>(FINAL_REAL).expect("json"),
            &mut u,
        );
        let intermedia: Value =
            serde_json::from_str(r#"{"model":"x","response":"tok","done":false}"#).expect("json");

        OLLAMA.extract_usage(&intermedia, &mut u);

        assert_eq!(u.output_tokens, Some(320), "no se pierde el total");
        assert_eq!(u.eval_us, Some(2_613_000));
    }

    /// NDJSON: la línea entera es el JSON. Un `data:` NO se quita, porque en
    /// este dialecto formaría parte del contenido.
    #[test]
    fn el_payload_de_linea_es_la_linea_entera() {
        assert_eq!(OLLAMA.payload_de_linea(r#"{"a":1}"#), Some(r#"{"a":1}"#));
        assert_eq!(OLLAMA.payload_de_linea("  {\"a\":1}\n"), Some(r#"{"a":1}"#));
        assert_eq!(OLLAMA.payload_de_linea("   "), None);
        assert_eq!(OLLAMA.payload_de_linea(""), None);
    }

    /// **El default de `stream` es TRUE**, al revés que en OpenAI. Tratarlo
    /// como `false` haría que el escáner leyese un NDJSON entero como un solo
    /// JSON y no encontrara el `usage` de nadie.
    #[test]
    fn sin_campo_stream_se_asume_streaming() {
        let cfg = cfg_de_prueba();
        let out = OLLAMA.prepare(
            entrante(br#"{"model":"llama3.2:3b","prompt":"hola"}"#),
            &cfg,
        );

        assert!(out.stream, "ollama nativo hace streaming por defecto");
        assert_eq!(out.model.as_deref(), Some("llama3.2:3b"));
    }

    #[test]
    fn stream_explicito_en_false_se_respeta() {
        let cfg = cfg_de_prueba();
        let out = OLLAMA.prepare(
            entrante(br#"{"model":"m","prompt":"hola","stream":false}"#),
            &cfg,
        );

        assert!(!out.stream);
    }

    /// `/api/chat`: el último mensaje es el turno nuevo, el resto historial.
    #[test]
    fn decompose_separa_historial_del_ultimo_turno_en_chat() {
        let body: Value = serde_json::from_str(
            r#"{"model":"m","messages":[{"role":"user","content":"viejo"},
                {"role":"user","content":"nuevo"}],"tools":[{"name":"t"}]}"#,
        )
        .expect("json");

        let c = OLLAMA.decompose(&body).expect("descompone");

        assert!(c.history_bytes > 0, "el primer mensaje es historial");
        assert!(c.last_turn_bytes > 0, "el último es el turno nuevo");
        assert!(c.tools_bytes > 0);
    }

    /// `/api/generate` no tiene conversación: todo el prompt es el turno
    /// nuevo y el historial es CERO medido, no un hueco.
    #[test]
    fn decompose_en_generate_no_inventa_historial() {
        let body: Value =
            serde_json::from_str(r#"{"model":"m","prompt":"hola","system":"se breve"}"#)
                .expect("json");

        let c = OLLAMA.decompose(&body).expect("descompone");

        assert_eq!(c.history_bytes, 0, "sin conversación no hay historial");
        assert!(c.last_turn_bytes > 0);
        assert!(c.system_bytes > 0);
    }

    /// Un motor local no sirve desde caché de proveedor: los dos contadores
    /// se quedan en `None`, que dice «no lo reporta», no «fue cero».
    #[test]
    fn no_se_fabrican_contadores_de_cache() {
        let v: Value = serde_json::from_str(FINAL_REAL).expect("json");
        let mut u = Usage::default();

        OLLAMA.extract_usage(&v, &mut u);

        assert!(u.cache_read_tokens.is_none());
        assert!(u.cache_write_tokens.is_none());
    }

    fn cfg_de_prueba() -> AppConfig {
        AppConfig {
            local_port: 8080,
            bind_host: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            bind_host_warning: None,
            target_openai_url: "https://api.openai.com/v1".to_string(),
            target_anthropic_url: "https://api.anthropic.com/v1".to_string(),
            target_gemini_url: "https://generativelanguage.googleapis.com".to_string(),
            target_codex_url: "https://chatgpt.com/backend-api/codex".to_string(),
            target_ollama_url: "http://127.0.0.1:11434".to_string(),
            storage_dir: std::path::PathBuf::from("/tmp/oxidegate-test"),
            storage_dir_source: crate::config::StorageDirSource::Default,
            force_prompt_cache: false,
            force_effort: None,
            force_effort_warning: None,
            instructions_headings: false,
        }
    }

    fn entrante(body: &[u8]) -> Incoming {
        Incoming {
            body: body.to_vec(),
            path: "/api/generate".to_string(),
            query: None,
            content_encoding: None,
        }
    }
}
