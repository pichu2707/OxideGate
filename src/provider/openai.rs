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
    ContextBreakdown, Incoming, Outgoing, Provider, ToolCalls, ToolSearchSignal, ToolServerBytes,
    ToolServerKind, Usage, array_field, classify, fingerprint, maybe_decompress, measure_key,
    measure_other, measure_value, model_and_stream_from_value, parse_body,
    split_history_and_last_turn, tools_overhead_bytes,
};
use crate::config::AppConfig;
use serde_json::Value;

/// Adaptador de OpenAI Chat Completions (`/v1/chat/completions`).
pub struct OpenAiChat;

/// Adaptador de OpenAI Responses API (`/v1/responses`).
pub struct OpenAiResponses;

/// Adaptador de la Responses API de Codex (`/v1/codex/responses`, la ruta
/// que usa el cliente `pi`), reenviada a `chatgpt.com/backend-api/codex` en
/// vez de `api.openai.com`. Mismo dialecto JSON exacto que [`OpenAiResponses`]
/// (`instructions`/`input`/`tools`, `usage` bajo `response`): solo cambian
/// `url` y `route`. `decompose`/`extract_usage`/`tool_entries` DELEGAN en
/// [`OPENAI_RESPONSES`] en vez de duplicar el parseo del dialecto — ver la
/// nota de cada método.
///
/// Diferencia real con `OpenAiResponses`: el cliente `pi` manda el body
/// comprimido (`content-encoding: zstd`, a veces `gzip`). `prepare` mide la
/// telemetría (`prompt_hash`, `prompt_bytes`, `context`, `tools_by_server`)
/// sobre el JSON LÓGICO descomprimido (ver [`maybe_decompress`]), pero
/// reenvía `incoming.body` CRUDO intacto — igual que `OpenAiResponses`, el
/// forward nunca muta ni recomprime nada.
pub struct OpenAiCodexResponses;

pub static OPENAI_CHAT: OpenAiChat = OpenAiChat;
pub static OPENAI_RESPONSES: OpenAiResponses = OpenAiResponses;
pub static OPENAI_CODEX_RESPONSES: OpenAiCodexResponses = OpenAiCodexResponses;

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
        let skills = parsed
            .as_ref()
            .and_then(crate::provider::skills::detect_skills_in_body);
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
            skills,
            instructions,
            // Sin medir en este dialecto: ver `Outgoing::hooks`.
            hooks: None,
            effort_forced: None,
            tools_by_server: by_server,
            tools_overhead_bytes: overhead,
            // `output_config.effort` y `speed` (raíz) son dialecto EXCLUSIVO
            // de Anthropic: Chat Completions no tiene un equivalente hoy. Se
            // deja `None` a propósito, en vez de heredar en silencio un
            // default, para que un futuro campo equivalente de OpenAI se
            // decida conscientemente acá y no se cuele por accidente.
            requested_effort: None,
            requested_speed: None,
            // El mecanismo `tool_search` (carga diferida vía `input[]`) es
            // exclusivo del dialecto Responses/Codex: Chat Completions no lo
            // tiene. Mismo criterio de `None` explícito que arriba.
            tool_search: None,
            // El aplanado del namespacing MCP se mide en el dialecto Responses/
            // Codex (pi/opencode), no en Chat Completions: `None`.
            tools_flattened: None,
        }
    }

    fn extract_usage(&self, value: &Value, usage: &mut Usage) {
        extract_openai_usage(value, usage);
    }

    /// OpenAI NO publica invocaciones en esta fila. Mismo criterio que en
    /// Gemini: su dialecto las manda como `tool_calls` (Chat Completions) o
    /// como items `function_call` (Responses API), formas distintas de la de
    /// Anthropic que no se capturaron todavía contra tráfico real.
    ///
    /// Listas vacías significan "no se reconoció ninguna invocación", nunca
    /// "el modelo no invocó nada".
    fn extract_tool_use(&self, _value: &Value, _calls: &mut ToolCalls) {}

    /// Dialecto no capturado todavia: la fila publica `None`, no listas
    /// vacias. Ver [`Provider::captura_invocaciones`].
    /// Dialecto SSE: el JSON va tras `data:`. Se declara explícitamente
    /// porque el trait no da default — ver `Provider::payload_de_linea`.
    fn payload_de_linea<'a>(&self, linea: &'a str) -> Option<&'a str> {
        super::payload_sse(linea)
    }

    fn captura_invocaciones(&self) -> bool {
        false
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
            measured_bytes: system_bytes
                + tools_bytes
                + history_bytes
                + last_turn_bytes
                + other_bytes,
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
    ///
    /// `prompt_hash`/`prompt_bytes`/`parsed` se calculan sobre el JSON LÓGICO
    /// (ver [`maybe_decompress`]), no sobre `incoming.body` crudo: si el
    /// cliente mandó `content-encoding: zstd`/`gzip` (hoy nadie lo hace en
    /// esta variante, pero el mismo dialecto lo comparte
    /// [`super::OpenAiCodexResponses`], que SÍ recibe tráfico comprimido de
    /// `pi`), medir sobre el wire comprimido daría un `prompt_hash`/`context`
    /// sin sentido (bytes de un frame zstd, no del JSON real). Sin
    /// `content-encoding` (el caso de hoy), `maybe_decompress` es una copia
    /// transparente: este cambio no altera ningún número ya medido.
    fn prepare(&self, incoming: Incoming, cfg: &AppConfig) -> Outgoing {
        let logical = maybe_decompress(&incoming.body, incoming.content_encoding.as_deref());
        let prompt_hash = fingerprint(&logical);
        let prompt_bytes = logical.len();
        let parsed = parse_body(&logical);

        let (model, stream) = parsed
            .as_ref()
            .map(model_and_stream_from_value)
            .unwrap_or((None, false));
        let context = parsed.as_ref().and_then(|v| self.decompose(v));
        let skills = parsed
            .as_ref()
            .and_then(crate::provider::skills::detect_skills_in_body);
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
        // Medido sobre el MISMO `Value` ya parseado (nunca un segundo parseo):
        // `None` solo si el body no parseó (`parsed` es `None`), coherente con
        // el contrato de `Outgoing::tool_search`.
        let tool_search = parsed.as_ref().and_then(|v| self.tool_search(v));
        let tools_flattened = parsed.as_ref().and_then(|v| self.tools_flattened(v));

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
            skills,
            instructions,
            // Sin medir en este dialecto: ver `Outgoing::hooks`.
            hooks: None,
            effort_forced: None,
            tools_by_server: by_server,
            tools_overhead_bytes: overhead,
            // Ídem Chat Completions: `effort`/`speed` son dialecto exclusivo
            // de Anthropic, no aplica acá (ver esa nota para el contrato
            // completo).
            requested_effort: None,
            requested_speed: None,
            tool_search,
            tools_flattened,
        }
    }

    fn extract_usage(&self, value: &Value, usage: &mut Usage) {
        extract_openai_usage(value, usage);
    }

    /// OpenAI NO publica invocaciones en esta fila. Mismo criterio que en
    /// Gemini: su dialecto las manda como `tool_calls` (Chat Completions) o
    /// como items `function_call` (Responses API), formas distintas de la de
    /// Anthropic que no se capturaron todavía contra tráfico real.
    ///
    /// Listas vacías significan "no se reconoció ninguna invocación", nunca
    /// "el modelo no invocó nada".
    fn extract_tool_use(&self, _value: &Value, _calls: &mut ToolCalls) {}

    /// Dialecto no capturado todavia: la fila publica `None`, no listas
    /// vacias. Ver [`Provider::captura_invocaciones`].
    /// Dialecto SSE: el JSON va tras `data:`. Se declara explícitamente
    /// porque el trait no da default — ver `Provider::payload_de_linea`.
    fn payload_de_linea<'a>(&self, linea: &'a str) -> Option<&'a str> {
        super::payload_sse(linea)
    }

    fn captura_invocaciones(&self) -> bool {
        false
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
            measured_bytes: system_bytes
                + tools_bytes
                + history_bytes
                + last_turn_bytes
                + other_bytes,
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

    /// Mide la carga diferida de herramientas (`tool_search`) recorriendo
    /// `input[]`. Ver [`ToolSearchSignal`] para el porqué de mirar acá y no en
    /// `tools[]`.
    ///
    /// Recorre los items de `input[]` buscando `type == "tool_search_call"` o
    /// `"tool_search_output"`: cualquiera de los dos marca `used = true` (el
    /// mecanismo LAZY se ejercitó). Solo `tool_search_output` aporta a
    /// `deferred_loaded`, contando dentro de su `tools[]` las que traen
    /// `defer_loading: true` — la marca real de una herramienta diferida
    /// (leída con la misma clave que [`group_tools_by_server`]).
    ///
    /// Nunca es `None` cuando el body parseó: un request Responses/Codex SIN
    /// items `tool_search_*` devuelve `Some { used: false, deferred_loaded: 0 }`
    /// — EAGER confirmado, no ausencia de dato. `input` ausente, string, o
    /// no-array cae en ese mismo caso (no hay items que puedan diferir).
    fn tool_search(&self, body: &Value) -> Option<ToolSearchSignal> {
        let items = body
            .as_object()
            .and_then(|o| o.get("input"))
            .and_then(Value::as_array);

        let Some(items) = items else {
            return Some(ToolSearchSignal {
                used: false,
                deferred_loaded: 0,
            });
        };

        let mut used = false;
        let mut deferred_loaded = 0;
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("tool_search_call") => used = true,
                Some("tool_search_output") => {
                    used = true;
                    if let Some(tools) = item.get("tools").and_then(Value::as_array) {
                        deferred_loaded += tools
                            .iter()
                            .filter(|t| {
                                t.get("defer_loading")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false)
                            })
                            .count();
                    }
                }
                _ => {}
            }
        }

        Some(ToolSearchSignal {
            used,
            deferred_loaded,
        })
    }

    /// Mide si el `(native)` de esta petición es verificable (ver
    /// [`Outgoing::tools_flattened`] y el default del trait para el contrato).
    ///
    /// Reutiliza [`Self::tool_entries`] (mismo `tools[]` que alimenta
    /// `tools_by_server`) y [`super::classify`]: `None` si no hay tools;
    /// `Some(false)` si alguna clasifica como [`super::ToolServerKind::Mcp`]
    /// (namespacing `mcp__` presente y fiable); `Some(true)` si hay tools pero
    /// ninguna — el caso medido de `pi`/`opencode`, cuyo `(native)` puede
    /// ocultar MCP aplanado. NO inspecciona nombres crudos ni intenta adivinar
    /// servidores: solo comprueba la PRESENCIA del separador inequívoco `mcp__`.
    fn tools_flattened(&self, body: &Value) -> Option<bool> {
        let entries = self.tool_entries(body)?;
        if entries.is_empty() {
            return None;
        }
        let any_mcp = entries
            .iter()
            .any(|(name, _)| matches!(classify(name).0, ToolServerKind::Mcp));
        Some(!any_mcp)
    }
}

impl Provider for OpenAiCodexResponses {
    /// Mismo nombre corto que el resto de la familia OpenAI en la métrica de
    /// upstream, pero DISTINTO: `"codex"`, no `"openai"`. Decisión deliberada,
    /// no descuido — este proveedor pega a un backend enteramente distinto
    /// (`chatgpt.com/backend-api/codex`, autenticado con sesión de ChatGPT,
    /// con su propio sistema de cuota — ver `telemetry::CodexQuota`), así que
    /// agregarlo bajo `"openai"` mezclaría dos backends con límites y
    /// facturación propios en la misma fila de `/stats`.
    fn name(&self) -> &'static str {
        "codex"
    }

    /// Arma el request hacia `{codex}/responses`
    /// (`chatgpt.com/backend-api/codex/responses`, NO `api.openai.com`).
    /// Mismo dialecto exacto que [`OpenAiResponses::prepare`] — de hecho el
    /// cuerpo de esta función es una copia deliberada de esa, con `url`/
    /// `route` cambiados; no se factoriza en una función compartida porque
    /// eso movería la construcción de `Outgoing` (con sus 13 campos) a una
    /// firma genérica que ganaría más complejidad de la que ahorra para dos
    /// llamadores.
    ///
    /// La diferencia real de este proveedor es la de arriba, en el tipo: el
    /// cliente `pi` manda el body comprimido (`content-encoding: zstd`, a
    /// veces `gzip`). `prompt_hash`/`prompt_bytes`/`context`/`tools_by_server`
    /// se miden sobre el JSON LÓGICO descomprimido ([`maybe_decompress`]);
    /// `Outgoing::body` sigue siendo `incoming.body` CRUDO, byte-idéntico al
    /// que mandó el cliente — el forward nunca pasa por `maybe_decompress`.
    fn prepare(&self, incoming: Incoming, cfg: &AppConfig) -> Outgoing {
        let logical = maybe_decompress(&incoming.body, incoming.content_encoding.as_deref());
        let prompt_hash = fingerprint(&logical);
        let prompt_bytes = logical.len();
        let parsed = parse_body(&logical);

        let (model, stream) = parsed
            .as_ref()
            .map(model_and_stream_from_value)
            .unwrap_or((None, false));
        let context = parsed.as_ref().and_then(|v| self.decompose(v));
        let skills = parsed
            .as_ref()
            .and_then(crate::provider::skills::detect_skills_in_body);
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
        // Mismo contrato que `OpenAiResponses::prepare`: medido sobre el JSON
        // LÓGICO descomprimido, `None` solo si el body no parseó.
        let tool_search = parsed.as_ref().and_then(|v| self.tool_search(v));
        let tools_flattened = parsed.as_ref().and_then(|v| self.tools_flattened(v));

        Outgoing {
            url: format!("{}/responses", cfg.target_codex_url),
            route: "/v1/codex/responses".to_string(),
            upstream: self.name(),
            model,
            stream,
            prompt_hash,
            prompt_bytes,
            body: incoming.body,
            // Codex gestiona su propia caché del lado del backend; no aplica
            // la palanca de Anthropic.
            cache_control_forced: false,
            context,
            skills,
            instructions,
            // Sin medir en este dialecto: ver `Outgoing::hooks`.
            hooks: None,
            effort_forced: None,
            tools_by_server: by_server,
            tools_overhead_bytes: overhead,
            // `effort`/`speed` son dialecto exclusivo de Anthropic.
            requested_effort: None,
            requested_speed: None,
            tool_search,
            tools_flattened,
        }
    }

    /// DELEGA en [`OPENAI_RESPONSES`]: es exactamente el mismo evento
    /// `response.completed` con `usage` bajo `response`, mismo `input_tokens`/
    /// `output_tokens`. Nunca se duplica el parseo del dialecto.
    fn extract_usage(&self, value: &Value, usage: &mut Usage) {
        OPENAI_RESPONSES.extract_usage(value, usage);
    }

    /// Delega en `OPENAI_RESPONSES`, igual que `extract_usage`: mismo
    /// dialecto, misma ausencia declarada.
    fn extract_tool_use(&self, value: &Value, calls: &mut ToolCalls) {
        OPENAI_RESPONSES.extract_tool_use(value, calls);
    }

    /// Delega, igual que el extractor: mismo dialecto, misma ausencia.
    /// Dialecto SSE: el JSON va tras `data:`. Se declara explícitamente
    /// porque el trait no da default — ver `Provider::payload_de_linea`.
    fn payload_de_linea<'a>(&self, linea: &'a str) -> Option<&'a str> {
        super::payload_sse(linea)
    }

    fn captura_invocaciones(&self) -> bool {
        OPENAI_RESPONSES.captura_invocaciones()
    }

    /// DELEGA en [`OPENAI_RESPONSES::decompose`]: mismo dialecto exacto
    /// (`instructions`/`tools`/`input`), sin reescribir el mapeo acá.
    fn decompose(&self, body: &Value) -> Option<ContextBreakdown> {
        OPENAI_RESPONSES.decompose(body)
    }

    /// DELEGA en [`OPENAI_RESPONSES::tool_entries`]: mismo dialecto exacto
    /// (`tools[].name` plano), sin reescribir el parseo acá.
    fn tool_entries<'a>(&self, body: &'a Value) -> Option<Vec<(&'a str, &'a Value)>> {
        OPENAI_RESPONSES.tool_entries(body)
    }

    /// DELEGA en [`OPENAI_RESPONSES::tool_search`]: `pi` habla exactamente este
    /// dialecto (de hecho es su cliente principal, con `input[]` que SÍ trae
    /// items `tool_search_*`), así que la medición es idéntica. Sin esta
    /// delegación, `OpenAiCodexResponses` heredaría el default `None` del trait
    /// y quedaría ciego a la carga diferida justo del cliente que más la usa.
    fn tool_search(&self, body: &Value) -> Option<ToolSearchSignal> {
        OPENAI_RESPONSES.tool_search(body)
    }

    /// DELEGA en [`OPENAI_RESPONSES::tools_flattened`]: `pi` es el cliente que
    /// motiva esta señal (nombres de tool crudos, sin `mcp__`), así que la
    /// medición es idéntica a la del dialecto Responses base.
    fn tools_flattened(&self, body: &Value) -> Option<bool> {
        OPENAI_RESPONSES.tools_flattened(body)
    }

    /// DELEGA en [`OPENAI_RESPONSES::tools_by_server`] en vez del default del
    /// trait: ambos comparten el mismo `tool_entries`, así que el resultado
    /// sería idéntico de todas formas, pero llamar directamente evita
    /// depender de que el default del trait no cambie de comportamiento por
    /// accidente en el futuro.
    fn tools_by_server(&self, body: &Value) -> Vec<ToolServerBytes> {
        OPENAI_RESPONSES.tools_by_server(body)
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
/// `response.completed`).
///
/// **Los dos dialectos usan nombres de campo DISTINTOS para los conteos
/// principales**, y esta función tiene que reconocer ambos porque la
/// comparte `OpenAiChat` y `OpenAiResponses` (ver el trait [`Provider`] en
/// `super`):
/// - Chat Completions: `prompt_tokens` / `completion_tokens`.
/// - Responses API: `input_tokens` / `output_tokens` (confirmado contra una
///   captura real de tráfico, evento `response.completed` de
///   `chatgpt.com/backend-api/codex`, modelo gpt-5.5 — ver
///   `docs/telemetry-level-1.md` §5).
///
/// PRECEDENCIA: se prueba primero el nombre de Chat Completions y se cae al
/// de Responses con `.or_else` si el primero está ausente. En la práctica
/// ambos nunca coexisten en el mismo payload (cada API manda su propio
/// dialecto, nunca los dos a la vez), así que el orden es arbitrario en
/// cuanto a corrección; se eligió Chat-primero solo por ser el campo
/// históricamente soportado acá, para minimizar el diff. Mismo patrón que ya
/// usa la extracción de caché de abajo.
///
/// Los tokens de caché son SUBCONJUNTO del prompt/input (no se restan acá,
/// `input_tokens` se queda crudo). El nombre del campo anidado difiere por
/// variante: Chat Completions manda `prompt_tokens_details.cached_tokens`,
/// Responses manda `input_tokens_details.cached_tokens`; probamos ambos ya
/// que esta función es compartida.
///
/// Cache-write: Chat Completions no tiene este concepto. Responses SÍ lo
/// expone (`input_tokens_details.cache_write_tokens`, visible en la misma
/// captura real citada arriba) y `Usage::cache_write_tokens` ya existe como
/// campo (lo llena Anthropic), así que se popula acá cuando está presente.
///
/// GAP CONOCIDO, NO RESUELTO ACÁ: `output_tokens_details.reasoning_tokens`
/// (modelos de razonamiento, GPT-5.x) no se extrae. Los tokens de
/// razonamiento son habitualmente un SUBCONJUNTO de `output_tokens` (ya
/// contados), y `Usage` no tiene hoy un campo dedicado para desglosarlos sin
/// arriesgar doble conteo en `telemetry::pricing`. Se deja sin tocar a
/// propósito hasta confirmar la semántica exacta y decidir dónde vive ese
/// desglose.
fn extract_openai_usage(value: &Value, usage: &mut Usage) {
    let Some(u) = value
        .get("usage")
        .or_else(|| value.get("response").and_then(|r| r.get("usage")))
    else {
        return;
    };

    if let Some(v) = u
        .get("prompt_tokens")
        .or_else(|| u.get("input_tokens"))
        .and_then(Value::as_u64)
    {
        usage.input_tokens = Some(v);
    }
    if let Some(v) = u
        .get("completion_tokens")
        .or_else(|| u.get("output_tokens"))
        .and_then(Value::as_u64)
    {
        usage.output_tokens = Some(v);
    }

    let details = u
        .get("prompt_tokens_details")
        .or_else(|| u.get("input_tokens_details"));
    if let Some(v) = details
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
    {
        usage.cache_read_tokens = Some(v);
    }
    if let Some(v) = details
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(Value::as_u64)
    {
        usage.cache_write_tokens = Some(v);
    }
}

#[cfg(test)]
mod tests {
    use super::super::NATIVE_TOOLS_LABEL;
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

    /// REGRESIÓN (bug real, no hipotético): captura real de tráfico, evento
    /// `response.completed` de la Responses API (`chatgpt.com/backend-api/codex`,
    /// modelo gpt-5.5). El `usage` real usa `input_tokens`/`output_tokens`,
    /// NO `prompt_tokens`/`completion_tokens` — antes del fix, el extractor
    /// solo reconocía el nombre de Chat Completions y este payload real
    /// devolvía `input_tokens`/`output_tokens` en `None` pese a un 200 OK.
    /// Este test FALLA contra el código pre-fix y pasa con el `.or_else`
    /// agregado a `extract_openai_usage`.
    #[test]
    fn extracts_usage_from_real_responses_completed_event_input_output_tokens() {
        let mut usage = Usage::default();
        let value: Value = serde_json::from_str(
            r#"{
                "type": "response.completed",
                "response": {
                    "usage": {
                        "input_tokens": 13,
                        "input_tokens_details": {"cache_write_tokens": 0, "cached_tokens": 0},
                        "output_tokens": 6,
                        "output_tokens_details": {"reasoning_tokens": 0},
                        "total_tokens": 19
                    }
                }
            }"#,
        )
        .unwrap();

        OPENAI_RESPONSES.extract_usage(&value, &mut usage);

        assert_eq!(usage.input_tokens, Some(13));
        assert_eq!(usage.output_tokens, Some(6));
        assert_eq!(usage.cache_read_tokens, Some(0));
        assert_eq!(usage.cache_write_tokens, Some(0));
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
            bd.system_bytes
                + bd.tools_bytes
                + bd.history_bytes
                + bd.last_turn_bytes
                + bd.other_bytes
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
            bd.system_bytes
                + bd.tools_bytes
                + bd.history_bytes
                + bd.last_turn_bytes
                + bd.other_bytes
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
            bd.system_bytes
                + bd.tools_bytes
                + bd.history_bytes
                + bd.last_turn_bytes
                + bd.other_bytes
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
        }
    }

    fn incoming_with_body(body: &str) -> Incoming {
        Incoming {
            path: "/v1/chat/completions".to_string(),
            query: None,
            body: body.as_bytes().to_vec(),
            content_encoding: None,
        }
    }

    /// Variante de `incoming_with_body` para fixtures con `content-encoding`
    /// explícito (los tests de Codex/zstd la usan; el resto sigue con
    /// `content_encoding: None` vía `incoming_with_body`).
    fn incoming_with_encoded_body(body: Vec<u8>, content_encoding: &str) -> Incoming {
        Incoming {
            path: "/v1/codex/responses".to_string(),
            query: None,
            body,
            content_encoding: Some(content_encoding.to_string()),
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
        let incoming = incoming_with_body(r#"{"model":"gpt-4o","stream":true,"input":"hola"}"#);
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

        // `defer_loading` no existe en el dialecto de OpenAI: ningún bucket
        // debe reportar diferido, aunque `group_tools_by_server` sea el mismo
        // código compartido que usa Anthropic (docs/optimizer-tool-search.md
        // §8 — la palanca es Anthropic-only).
        assert!(by_server.iter().all(|s| s.deferred_tools == 0));
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

        // Mismo motivo que en Chat Completions: `defer_loading` no existe en
        // el dialecto de OpenAI Responses.
        assert!(by_server.iter().all(|s| s.deferred_tools == 0));
    }

    // -----------------------------------------------------------------
    // tool_search — carga diferida de herramientas del dialecto Responses.
    // Verdad del terreno (verificada contra @earendil-works/pi-ai): las tools
    // diferidas NO viven en `tools[]` (siempre eager) sino en items
    // `tool_search_output` dentro de `input[]`. Ver `ToolSearchSignal`.
    // -----------------------------------------------------------------

    /// Un request Responses con `input[]` de mensajes normales (sin ningún
    /// item `tool_search_*`) es EAGER CONFIRMADO: `Some { used: false }`, no
    /// `None`. `None` está reservado a "no pude ni mirar" (body sin parsear).
    #[test]
    fn responses_tool_search_eager_sin_items() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "gpt-5.5",
                "tools": [{"type": "function", "name": "Read", "parameters": {}}],
                "input": [
                    {"type": "message", "role": "user", "content": "hola"},
                    {"type": "message", "role": "assistant", "content": "buenas"}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(
            OPENAI_RESPONSES.tool_search(&body),
            Some(ToolSearchSignal {
                used: false,
                deferred_loaded: 0
            })
        );
    }

    /// El caso LAZY completo: `tool_search_call` + `tool_search_output` con dos
    /// tools `defer_loading: true`. `used == true` y `deferred_loaded == 2`.
    #[test]
    fn responses_tool_search_lazy_cuenta_tools_diferidas() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "gpt-5.5",
                "input": [
                    {"type": "message", "role": "user", "content": "usa las tools nuevas"},
                    {"type": "tool_search_call", "call_id": "pi_tool_load_x", "execution": "client", "status": "completed"},
                    {"type": "tool_search_output", "call_id": "pi_tool_load_x", "execution": "client", "status": "completed",
                     "tools": [
                        {"type": "function", "name": "mcp__srv__a", "parameters": {}, "defer_loading": true},
                        {"type": "function", "name": "mcp__srv__b", "parameters": {}, "defer_loading": true}
                     ]}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(
            OPENAI_RESPONSES.tool_search(&body),
            Some(ToolSearchSignal {
                used: true,
                deferred_loaded: 2
            })
        );
    }

    /// Un `tool_search_call` SIN su `tool_search_output` (búsqueda que no
    /// cargó nada) sigue marcando `used == true` — el mecanismo LAZY se
    /// ejercitó — pero `deferred_loaded == 0`.
    #[test]
    fn responses_tool_search_call_sin_output_es_lazy_sin_carga() {
        let body: Value = serde_json::from_str(
            r#"{
                "input": [
                    {"type": "tool_search_call", "call_id": "x", "execution": "client", "status": "completed"}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(
            OPENAI_RESPONSES.tool_search(&body),
            Some(ToolSearchSignal {
                used: true,
                deferred_loaded: 0
            })
        );
    }

    /// Dentro de un `tool_search_output`, solo cuentan las tools con
    /// `defer_loading: true`: una tool sin la marca NO suma a `deferred_loaded`
    /// (misma lectura por-tool que `group_tools_by_server`).
    #[test]
    fn responses_tool_search_output_ignora_tools_sin_marca() {
        let body: Value = serde_json::from_str(
            r#"{
                "input": [
                    {"type": "tool_search_output", "call_id": "x", "execution": "client", "status": "completed",
                     "tools": [
                        {"type": "function", "name": "mcp__srv__a", "parameters": {}, "defer_loading": true},
                        {"type": "function", "name": "mcp__srv__b", "parameters": {}}
                     ]}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(
            OPENAI_RESPONSES.tool_search(&body),
            Some(ToolSearchSignal {
                used: true,
                deferred_loaded: 1
            })
        );
    }

    /// `input` como STRING plano (un solo turno de texto) no puede contener
    /// items diferidos: EAGER confirmado, mismo caso que `input` ausente o
    /// no-array. Nunca `None` mientras el body haya parseado.
    #[test]
    fn responses_tool_search_input_string_es_eager() {
        let body: Value =
            serde_json::from_str(r#"{"model": "gpt-5.5", "input": "explica el patron builder"}"#)
                .unwrap();

        assert_eq!(
            OPENAI_RESPONSES.tool_search(&body),
            Some(ToolSearchSignal {
                used: false,
                deferred_loaded: 0
            })
        );
    }

    /// El dialecto Chat Completions NO tiene `tool_search`: hereda el default
    /// `None` del trait (contrato de "no aplica"), aun con `input` presente.
    #[test]
    fn chat_tool_search_es_none() {
        let body: Value =
            serde_json::from_str(r#"{"model": "gpt-4o", "input": [{"type": "tool_search_call"}]}"#)
                .unwrap();

        assert_eq!(OPENAI_CHAT.tool_search(&body), None);
    }

    /// `prepare` de Codex cablea `tool_search` (delegando en Responses): un
    /// body zstd LAZY debe medirse sobre el JSON descomprimido y reportar la
    /// carga diferida, no quedar ciego.
    #[test]
    fn codex_prepare_cablea_tool_search_lazy_desde_zstd() {
        let cfg = test_config();
        let logical = r#"{
            "model": "gpt-5.5",
            "input": [
                {"type": "tool_search_call", "call_id": "x", "execution": "client", "status": "completed"},
                {"type": "tool_search_output", "call_id": "x", "execution": "client", "status": "completed",
                 "tools": [{"type": "function", "name": "mcp__srv__a", "parameters": {}, "defer_loading": true}]}
            ]
        }"#
        .as_bytes();
        let comprimido = zstd::encode_all(logical, 0).expect("zstd comprime el fixture");
        let incoming = incoming_with_encoded_body(comprimido, "zstd");

        let out = OPENAI_CODEX_RESPONSES.prepare(incoming, &cfg);

        assert_eq!(
            out.tool_search,
            Some(ToolSearchSignal {
                used: true,
                deferred_loaded: 1
            })
        );
    }

    /// Body no-JSON: `tool_search` queda en `None` ("no pude ni mirar"),
    /// mismo contrato que `context`.
    #[test]
    fn responses_prepare_body_no_json_tool_search_none() {
        let cfg = test_config();
        let incoming = incoming_with_body("esto no es JSON");

        let out = OPENAI_RESPONSES.prepare(incoming, &cfg);

        assert_eq!(out.tool_search, None);
    }

    // -----------------------------------------------------------------
    // tools_flattened — honestidad de la atribución de tools_by_server.
    // Medido en tráfico real: pi manda nombres crudos (read, bash…) y opencode
    // usa `<server>_<tool>` (context7_query-docs, engram_mem_search) con `_`
    // AMBIGUO — ninguno usa el `mcp__` inequívoco. Ver ToolSearchSignal doc.
    // -----------------------------------------------------------------

    /// Reproduce el set REAL medido de opencode: nombres nativos (`read`,
    /// `apply_patch`) y MCP aplanados con `_` (`context7_query-docs`,
    /// `engram_mem_search`), NINGUNO con `mcp__`. `(native)` no verificable ⇒
    /// `Some(true)`.
    #[test]
    fn responses_tools_flattened_true_cuando_ninguna_usa_mcp() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "gpt-5.5",
                "tools": [
                    {"type": "function", "name": "read", "parameters": {}},
                    {"type": "function", "name": "apply_patch", "parameters": {}},
                    {"type": "function", "name": "context7_query-docs", "parameters": {}},
                    {"type": "function", "name": "engram_mem_search", "parameters": {}}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(OPENAI_RESPONSES.tools_flattened(&body), Some(true));
    }

    /// Si AL MENOS una tool usa el namespacing inequívoco `mcp__server__tool`,
    /// el `(native)` es de fiar ⇒ `Some(false)`. (Poco común en este dialecto,
    /// pero el contrato debe ser honesto si aparece.)
    #[test]
    fn responses_tools_flattened_false_cuando_alguna_usa_mcp() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "gpt-5.5",
                "tools": [
                    {"type": "function", "name": "read", "parameters": {}},
                    {"type": "function", "name": "mcp__claude_ai_Gmail__search", "parameters": {}}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(OPENAI_RESPONSES.tools_flattened(&body), Some(false));
    }

    /// Sin `tools` (clave ausente) ⇒ `None`: no hay nada que juzgar, no una
    /// afirmación de "verificable" ni de "aplanado".
    #[test]
    fn responses_tools_flattened_none_sin_tools() {
        let body: Value = serde_json::from_str(r#"{"model": "gpt-5.5", "input": "hola"}"#).unwrap();

        assert_eq!(OPENAI_RESPONSES.tools_flattened(&body), None);
    }

    /// `tools: []` (declaró herramientas, son cero) ⇒ `None`: sin elementos no
    /// se puede decir si el `(native)` estaría o no verificado.
    #[test]
    fn responses_tools_flattened_none_tools_vacio() {
        let body: Value = serde_json::from_str(r#"{"model": "gpt-5.5", "tools": []}"#).unwrap();

        assert_eq!(OPENAI_RESPONSES.tools_flattened(&body), None);
    }

    /// El nombre nativo con `_` interno NO se confunde con `mcp__`: `apply_patch`
    /// o `read_mcp_resource` (que contiene `mcp` pero NO el patrón `mcp__x__y`)
    /// no cuentan como namespaced. Guarda contra un `contains("mcp")` ingenuo.
    #[test]
    fn responses_tools_flattened_native_con_mcp_en_el_nombre_no_cuenta() {
        let body: Value = serde_json::from_str(
            r#"{
                "model": "gpt-5.5",
                "tools": [
                    {"type": "function", "name": "read_mcp_resource", "parameters": {}},
                    {"type": "function", "name": "list_mcp_resources", "parameters": {}}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(OPENAI_RESPONSES.tools_flattened(&body), Some(true));
    }

    /// El dialecto Chat Completions NO aplana (o al menos no se mide acá):
    /// hereda el default `None` del trait.
    #[test]
    fn chat_tools_flattened_es_none() {
        let body: Value = serde_json::from_str(
            r#"{"model": "gpt-4o", "tools": [{"type": "function", "function": {"name": "read"}}]}"#,
        )
        .unwrap();

        assert_eq!(OPENAI_CHAT.tools_flattened(&body), None);
    }

    /// `prepare` de Codex cablea `tools_flattened` (delegando en Responses):
    /// un body con tools aplanadas debe reportar `Some(true)`.
    #[test]
    fn codex_prepare_cablea_tools_flattened() {
        let cfg = test_config();
        let incoming = incoming_with_body(
            r#"{"model":"gpt-5.5","tools":[{"type":"function","name":"engram_mem_search","parameters":{}}]}"#,
        );

        let out = OPENAI_CODEX_RESPONSES.prepare(incoming, &cfg);

        assert_eq!(out.tools_flattened, Some(true));
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

    // -----------------------------------------------------------------
    // OPENAI_CODEX_RESPONSES — Responses API de Codex (`/v1/codex/responses`,
    // reenviada a `{codex}/responses`). `pi` manda el body comprimido en
    // zstd (`content-encoding: zstd`): estos tests verifican el contrato
    // completo de esa ruta.
    // -----------------------------------------------------------------

    /// `prepare` con un body zstd debe: (a) medir telemetría (`context` y
    /// `tools_by_server` NO deben quedar en `None`/vacíos: el body SÍ
    /// parseó, solo que había que descomprimirlo primero) sobre el JSON
    /// LÓGICO descomprimido, y (b) reenviar el body CRUDO comprimido,
    /// byte-idéntico al original — el forward nunca pasa por
    /// `maybe_decompress` (ver `Outgoing::body`).
    #[test]
    fn codex_prepare_mide_sobre_zstd_pero_reenvia_crudo() {
        let cfg = test_config();
        let logical = r#"{
            "model": "gpt-5.5",
            "instructions": "eres un asistente util",
            "input": "hola",
            "tools": [{"type": "function", "name": "mcp__claude_ai_Gmail__search_threads", "parameters": {}}]
        }"#
        .as_bytes();
        let comprimido = zstd::encode_all(logical, 0).expect("zstd comprime el fixture");
        let incoming = incoming_with_encoded_body(comprimido.clone(), "zstd");

        let out = OPENAI_CODEX_RESPONSES.prepare(incoming, &cfg);

        assert!(
            out.context.is_some(),
            "el body zstd debe medirse, no quedar en None"
        );
        assert!(
            !out.tools_by_server.is_empty(),
            "las tools del body zstd deben desglosarse por servidor"
        );
        assert_eq!(
            out.body, comprimido,
            "el body reenviado debe ser byte-idéntico al comprimido original (forward intacto)"
        );
        assert_eq!(out.url, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(out.route, "/v1/codex/responses");
    }

    /// Mismo contrato con un body SIN comprimir (`content_encoding: None`):
    /// `maybe_decompress` debe ser transparente y el resultado debe coincidir
    /// exactamente con el de `OPENAI_RESPONSES` sobre el mismo body, salvo
    /// `url`/`route` (que son los de Codex).
    #[test]
    fn codex_prepare_sin_compresion_delega_en_la_misma_logica_que_responses() {
        let cfg = test_config();
        let incoming = incoming_with_body(
            r#"{"model":"gpt-5.5","instructions":"be helpful","input":"explain the builder pattern"}"#,
        );

        let out = OPENAI_CODEX_RESPONSES.prepare(incoming, &cfg);
        let bd = out.context.expect("body válido debe producir contexto");

        assert_eq!(bd.system_bytes, 12);
        assert_eq!(bd.last_turn_bytes, 29);
        assert_eq!(out.url, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(out.route, "/v1/codex/responses");
    }

    /// Body no-JSON en Codex: `prepare` no debe romper, reenvía intacto y
    /// deja `context` en `None` (mismo contrato que el resto de proveedores).
    #[test]
    fn codex_prepare_body_no_json_no_panica() {
        let cfg = test_config();
        let incoming = incoming_with_body("esto no es JSON");
        let original_body = incoming.body.clone();

        let out = OPENAI_CODEX_RESPONSES.prepare(incoming, &cfg);

        assert_eq!(out.body, original_body);
        assert_eq!(out.context, None);
    }
}
