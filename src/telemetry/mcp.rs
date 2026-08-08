//! Cruce entre lo que cada servidor MCP CUESTA y lo que se USA.
//!
//! Es la única pregunta que nadie más puede contestar. Los bytes por servidor
//! viven en la petición (`tools_by_server`) y las invocaciones en la
//! respuesta (`tool_calls`); OxideGate es el punto por el que pasan las dos, y
//! cruzarlas sobre el histórico es lo que permite afirmar:
//!
//! > Pagas 12.400 B por petición por el servidor `context7` y no has
//! > invocado ninguna de sus 8 herramientas en 200 peticiones.
//!
//! `mcp-lean.json` es la palanca más grande del catálogo publicado
//! (−55.098 B/petición) y una de las cuatro que el proxy NO puede aplicar en
//! el cable sin borrarle herramientas al agente. Este módulo es la tercera
//! vía: ni aplicarla ni callarse, sino decirle al operador cuál sobra y que
//! decida él.
//!
//! # Por qué el veredicto va acompañado de sus descartes
//!
//! Recomendar quitar un servidor que sí se usa es el peor fallo posible aquí:
//! el usuario pierde una integración que funciona, por consejo del medidor. Y
//! una fila puede mentir de tres formas distintas, así que **una petición solo
//! cuenta como prueba de NO-uso si supera las tres**:
//!
//! 1. **Tiene extractor.** `tool_calls: None` significa "este proveedor no
//!    mide invocaciones", no "el modelo no invocó nada". Hoy solo Anthropic
//!    lo tiene, y las filas anteriores al campo también son `None`.
//! 2. **Se escaneó entera.** `complete: false` es un turno abortado o un
//!    stream roto: la lista es un PREFIJO. No se puede concluir nada de una
//!    ausencia en una lista incompleta.
//! 3. **No está truncada.** Si el cupo recortó la lista, una invocación pudo
//!    quedarse fuera.
//!
//! Las tres se cuentan y se publican ([`McpServerRow::descartes`]). Un
//! veredicto apoyado en pocas peticiones concluyentes se declara como tal en
//! vez de disfrazarse de conclusión.
//!
//! # Solo los servidores MCP reciben veredicto
//!
//! `(native)` es el cubo de las herramientas del propio agente y `(others)`
//! el de desborde del cupo: ninguno de los dos es algo que el usuario pueda
//! "quitar de su configuración", así que se publican con sus números y con
//! [`Verdict::NotApplicable`]. Recomendar quitar las herramientas nativas no
//! sería una recomendación agresiva, sería una incoherente.
use crate::provider::{ToolServerKind, classify};
use crate::telemetry::RequestMetric;
use serde::Serialize;
use std::collections::HashMap;

/// Peticiones CONCLUYENTES que hacen falta antes de afirmar que un servidor
/// no se usa.
///
/// El número es un juicio, no una medida, y por eso viaja en la respuesta
/// ([`McpSnapshot::umbral`]): quien no esté de acuerdo tiene los conteos
/// crudos para recalcular con el suyo.
///
/// La forma del error importa más que su tamaño. Decir "no lo has usado en 3
/// peticiones" es ruido —cualquier sesión corta lo produce— mientras que
/// decirlo sobre 50 ya describe un hábito. Se eligió 50 y no 200 porque 200
/// tarda días en acumularse en un uso normal y el consejo llegaría tarde para
/// ser útil.
pub const MIN_PETICIONES_CONCLUYENTES: u64 = 50;

/// Por qué una petición no pudo usarse como prueba de no-uso.
///
/// Se publican los tres conteos por separado, y no un total, porque cada uno
/// se arregla de forma distinta: `sin_extractor` con un extractor nuevo,
/// `incompletas` no se arregla (es tráfico real abortado), y `truncadas`
/// subiendo el cupo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Descartes {
    /// El proveedor de esa fila no mide invocaciones (`tool_calls: null`), o
    /// la fila se escribió antes de que el campo existiera.
    pub sin_extractor: u64,
    /// El escaneo no llegó al final: turno abortado o stream roto. La lista
    /// era un prefijo.
    pub incompletas: u64,
    /// La lista de invocaciones se recortó por el cupo, así que una llamada
    /// pudo quedarse fuera.
    pub truncadas: u64,
}

/// Qué se puede afirmar de un servidor con la evidencia disponible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Verdict {
    /// Se invocó al menos una vez. **Es la única conclusión que una fila
    /// incompleta también puede sostener**: ver una llamada prueba el uso,
    /// no verla no prueba el no-uso.
    Used {
        /// Invocaciones contadas, incluidas las de filas no concluyentes.
        invocations: u64,
    },
    /// Cero invocaciones sobre suficientes peticiones concluyentes. Es el
    /// veredicto que sostiene la recomendación de quitarlo.
    Unused {
        /// Peticiones que superaron los tres filtros y no trajeron ninguna
        /// invocación de este servidor.
        conclusive_requests: u64,
        /// Bytes por petición que se dejarían de pagar al quitarlo.
        bytes_per_request: u64,
    },
    /// Cero invocaciones, pero sin peticiones concluyentes suficientes.
    /// **No es lo mismo que `Unused`** y por eso no se colapsan: aquí la
    /// respuesta honesta es "todavía no lo sé".
    InsufficientData {
        conclusive_requests: u64,
        /// Cuántas harían falta ([`MIN_PETICIONES_CONCLUYENTES`]).
        needed: u64,
    },
    /// No es un servidor que el usuario pueda quitar: el cubo `(native)` de
    /// herramientas del agente, o el `(others)` de desborde del cupo.
    NotApplicable,
}

/// Una fila del cruce: un servidor, lo que cuesta y lo que se usa.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpServerRow {
    /// Etiqueta de display del servidor. Ver `provider::ToolServerBytes`.
    pub server: String,
    /// Naturaleza del cubo. Solo `mcp` recibe veredicto accionable.
    pub kind: ToolServerKind,
    /// Peticiones en las que este servidor viajó declarado en `tools[]`.
    pub declared_in_requests: u64,
    /// Bytes por petición, promediados sobre `declared_in_requests`. Es lo
    /// que se dejaría de pagar por petición si se quitara.
    pub bytes_per_request: u64,
    /// Herramientas que declaraba la ÚLTIMA vez que se vio. No se promedia:
    /// una media de "número de herramientas" no significa nada, y lo que le
    /// importa a quien lee es la configuración actual.
    pub tools_declared: usize,
    /// Invocaciones de herramientas de este servidor en todo el histórico.
    pub invocations: u64,
    /// Peticiones que superaron los tres filtros de honestidad.
    pub conclusive_requests: u64,
    /// Por qué se descartaron las demás.
    pub descartes: Descartes,
    /// Qué se puede afirmar con esta evidencia.
    pub verdict: Verdict,
}

/// Vista serializable del cruce completo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpSnapshot {
    /// Peticiones concluyentes que exige un veredicto `unused`. Viaja en la
    /// respuesta a propósito: es un juicio, no una medida, y quien no esté de
    /// acuerdo tiene los conteos crudos para aplicar el suyo.
    pub umbral: u64,
    /// Filas, ordenadas por bytes por petición descendente: lo más caro
    /// primero, que es el orden en el que interesa mirarlas.
    pub servers: Vec<McpServerRow>,
}

/// Acumulador interno por servidor.
#[derive(Debug, Default)]
struct McpAccumulator {
    kind: Option<ToolServerKind>,
    bytes_total: u64,
    declared_in_requests: u64,
    tools_declared: usize,
    invocations: u64,
    conclusive_requests: u64,
    descartes: Descartes,
}

/// Registro en memoria del cruce coste-vs-uso por servidor MCP.
///
/// Mismo contrato que [`StatsRegistry`](crate::telemetry::StatsRegistry):
/// vive tras un `Arc<RwLock<_>>`, no sabe nada de axum ni de locks, y se
/// rehidrata del `telemetry.jsonl` al arrancar.
#[derive(Debug, Default)]
pub struct McpRegistry {
    accumulators: HashMap<String, McpAccumulator>,
}

impl McpRegistry {
    /// Incorpora una métrica al cruce.
    ///
    /// El orden importa: primero se decide si la fila es CONCLUYENTE (una
    /// propiedad de la fila entera, no de cada servidor), y solo después se
    /// reparten declaraciones e invocaciones. Decidirlo por servidor
    /// permitiría que una misma petición contase como concluyente para uno e
    /// inconcluyente para otro, que no significa nada.
    pub fn ingest(&mut self, m: &RequestMetric) {
        let Some(declarados) = m.tools_by_server.as_ref() else {
            // Sin desglose de la petición no hay coste que atribuir, así que
            // no hay cruce que hacer. No es un descarte: es que esta fila no
            // habla de servidores en absoluto.
            return;
        };

        let motivo = motivo_de_descarte(m);

        // Las invocaciones se atribuyen ANTES de mirar si la fila es
        // concluyente: ver una llamada prueba el uso incluso en un turno
        // abortado. Es la asimetría central de este módulo — la ausencia de
        // prueba no es prueba de ausencia, pero la presencia sí es presencia.
        if let Some(calls) = m.tool_calls.as_ref() {
            for nombre in &calls.invoked {
                let (_, servidor) = classify(nombre);
                self.accumulators
                    .entry(servidor.to_string())
                    .or_default()
                    .invocations += 1;
            }
        }

        for fila in declarados {
            let acc = self.accumulators.entry(fila.server.clone()).or_default();
            acc.kind = Some(fila.kind);
            acc.bytes_total += fila.bytes as u64;
            acc.declared_in_requests += 1;
            acc.tools_declared = fila.tools;

            match motivo {
                Some(Motivo::SinExtractor) => acc.descartes.sin_extractor += 1,
                Some(Motivo::Incompleta) => acc.descartes.incompletas += 1,
                Some(Motivo::Truncada) => acc.descartes.truncadas += 1,
                None => acc.conclusive_requests += 1,
            }
        }
    }

    /// Vista serializable del cruce, ordenada por coste por petición.
    pub fn snapshot(&self) -> McpSnapshot {
        let mut servers: Vec<McpServerRow> = self
            .accumulators
            .iter()
            .filter(|(_, acc)| acc.declared_in_requests > 0)
            .map(|(server, acc)| {
                let bytes_per_request = acc.bytes_total / acc.declared_in_requests;
                let kind = acc.kind.unwrap_or(ToolServerKind::Mcp);
                McpServerRow {
                    server: server.clone(),
                    kind,
                    declared_in_requests: acc.declared_in_requests,
                    bytes_per_request,
                    tools_declared: acc.tools_declared,
                    invocations: acc.invocations,
                    conclusive_requests: acc.conclusive_requests,
                    descartes: acc.descartes.clone(),
                    verdict: veredicto(
                        kind,
                        acc.invocations,
                        acc.conclusive_requests,
                        bytes_per_request,
                    ),
                }
            })
            .collect();

        // Lo más caro primero: es el orden en el que se mira una lista así.
        // Desempate por nombre para que la salida sea estable entre llamadas
        // (el HashMap no lo es) y los tests puedan afirmar sobre ella.
        servers.sort_by(|a, b| {
            b.bytes_per_request
                .cmp(&a.bytes_per_request)
                .then_with(|| a.server.cmp(&b.server))
        });

        McpSnapshot {
            umbral: MIN_PETICIONES_CONCLUYENTES,
            servers,
        }
    }
}

/// Por qué una fila no sirve como prueba de no-uso. `None` = sí sirve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Motivo {
    SinExtractor,
    Incompleta,
    Truncada,
}

/// Los tres filtros de honestidad, en el orden en el que importan.
fn motivo_de_descarte(m: &RequestMetric) -> Option<Motivo> {
    let Some(calls) = m.tool_calls.as_ref() else {
        return Some(Motivo::SinExtractor);
    };
    if !calls.complete {
        return Some(Motivo::Incompleta);
    }
    if calls.invoked_total > calls.invoked.len()
        || calls.server_invoked_total > calls.server_invoked.len()
    {
        return Some(Motivo::Truncada);
    }
    None
}

/// Traduce la evidencia a lo que se puede afirmar con ella.
fn veredicto(
    kind: ToolServerKind,
    invocations: u64,
    conclusive_requests: u64,
    bytes_per_request: u64,
) -> Verdict {
    // Ni `(native)` ni `(others)` son algo que el usuario pueda quitar de su
    // configuración, así que no reciben un veredicto accionable ni cuando
    // llevan cero invocaciones.
    if kind != ToolServerKind::Mcp {
        return Verdict::NotApplicable;
    }

    if invocations > 0 {
        return Verdict::Used { invocations };
    }

    if conclusive_requests >= MIN_PETICIONES_CONCLUYENTES {
        Verdict::Unused {
            conclusive_requests,
            bytes_per_request,
        }
    } else {
        Verdict::InsufficientData {
            conclusive_requests,
            needed: MIN_PETICIONES_CONCLUYENTES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ToolCalls, ToolServerBytes};
    use crate::telemetry::{SessionAttribution, SessionSource};

    /// Métrica mínima válida. Los campos que este módulo NO mira se rellenan
    /// con valores neutros: lo único que importa aquí es `tools_by_server`
    /// (el coste declarado) y `tool_calls` (el uso observado).
    fn metrica_minima() -> RequestMetric {
        RequestMetric {
            timestamp: "2026-08-08T10:00:00Z".to_string(),
            route: "/v1/messages".to_string(),
            upstream: "anthropic".to_string(),
            model: Some("claude-opus-4-6".to_string()),
            prompt_hash: "0000000000000001".to_string(),
            stream: true,
            client: None,
            prompt_bytes: 100,
            input_tokens: Some(10),
            output_tokens: Some(5),
            cache_read_tokens: None,
            cache_write_tokens: None,
            cost_estimate_usd: None,
            cache_control_forced: false,
            requested_effort: None,
            requested_speed: None,
            served_speed: None,
            status: 200,
            ttft_ms: None,
            total_ms: 100.0,
            tokens_per_sec: None,
            context_system_bytes: None,
            context_tools_bytes: None,
            context_history_bytes: None,
            context_last_turn_bytes: None,
            context_other_bytes: None,
            context_measured_bytes: None,
            context_messages_count: None,
            context_tax_ratio: None,
            cache_by_section: None,
            input_share_by_section: None,
            tools_by_server: None,
            tools_overhead_bytes: None,
            tool_search: None,
            tools_flattened: None,
            skills: None,
            instructions: None,
            effort_forced: None,
            response_bytes: None,
            prepare_us: 0,
            codex_quota: None,
            session: SessionAttribution {
                source: SessionSource::Unattributed,
                key: "unattributed".to_string(),
            },
            tool_calls: None,
        }
    }

    /// Métrica mínima con un servidor declarado y unas invocaciones dadas.
    /// `calls: None` simula un proveedor sin extractor.
    fn fila(
        servidor: &str,
        kind: ToolServerKind,
        bytes: usize,
        tools: usize,
        calls: Option<ToolCalls>,
    ) -> RequestMetric {
        let mut m = metrica_minima();
        m.tools_by_server = Some(vec![ToolServerBytes {
            server: servidor.to_string(),
            kind,
            tools,
            bytes,
            tool_names: Vec::new(),
            deferred_tools: 0,
        }]);
        m.tool_calls = calls;
        m
    }

    /// Un `ToolCalls` completo, sin truncar, con los nombres dados.
    fn invocando(nombres: &[&str]) -> ToolCalls {
        let mut c = ToolCalls::default();
        for n in nombres {
            c.push_invoked(n);
        }
        c.marca_completa();
        c
    }

    /// El caso que justifica todo el módulo: un servidor que se paga en cada
    /// petición y no se invoca nunca, con evidencia suficiente.
    #[test]
    fn un_servidor_caro_y_nunca_invocado_se_declara_unused() {
        let mut reg = McpRegistry::default();
        for _ in 0..MIN_PETICIONES_CONCLUYENTES {
            reg.ingest(&fila(
                "context7",
                ToolServerKind::Mcp,
                12_400,
                8,
                Some(invocando(&["Read"])),
            ));
        }

        let snap = reg.snapshot();
        let ctx = snap
            .servers
            .iter()
            .find(|s| s.server == "context7")
            .expect("context7 debe estar");

        assert_eq!(
            ctx.verdict,
            Verdict::Unused {
                conclusive_requests: MIN_PETICIONES_CONCLUYENTES,
                bytes_per_request: 12_400,
            },
            "declarado en todas, invocado en ninguna"
        );
        assert_eq!(ctx.invocations, 0);
        assert_eq!(
            ctx.descartes,
            Descartes::default(),
            "ninguna fila se descartó"
        );
    }

    /// Una sola invocación basta para tumbar el veredicto: el módulo nunca
    /// recomienda quitar algo que se ha visto usar.
    #[test]
    fn una_sola_invocacion_impide_el_veredicto_unused() {
        let mut reg = McpRegistry::default();
        for i in 0..MIN_PETICIONES_CONCLUYENTES {
            let usa = i == 0;
            reg.ingest(&fila(
                "context7",
                ToolServerKind::Mcp,
                12_400,
                8,
                Some(invocando(if usa {
                    &["mcp__context7__get-docs"]
                } else {
                    &["Read"]
                })),
            ));
        }

        let snap = reg.snapshot();
        let ctx = snap
            .servers
            .iter()
            .find(|s| s.server == "context7")
            .unwrap();
        assert_eq!(ctx.verdict, Verdict::Used { invocations: 1 });
    }

    /// REGRESIÓN. Pocas peticiones concluyentes NO es lo mismo que no-uso.
    /// Colapsarlos haría que una sesión corta recomendase borrar todo.
    #[test]
    fn con_poca_evidencia_dice_que_no_lo_sabe() {
        let mut reg = McpRegistry::default();
        for _ in 0..3 {
            reg.ingest(&fila(
                "context7",
                ToolServerKind::Mcp,
                12_400,
                8,
                Some(invocando(&[])),
            ));
        }

        let snap = reg.snapshot();
        let ctx = snap
            .servers
            .iter()
            .find(|s| s.server == "context7")
            .unwrap();
        assert_eq!(
            ctx.verdict,
            Verdict::InsufficientData {
                conclusive_requests: 3,
                needed: MIN_PETICIONES_CONCLUYENTES,
            },
            "tres peticiones no son un habito"
        );
    }

    /// REGRESIÓN. Los tres filtros de honestidad. Una fila sin extractor,
    /// una incompleta y una truncada NO pueden contar como prueba de no-uso,
    /// y cada motivo se publica por separado.
    #[test]
    fn las_filas_que_no_prueban_nada_se_descartan_y_se_dicen() {
        let mut reg = McpRegistry::default();

        // 1. Sin extractor: el proveedor no mide invocaciones.
        reg.ingest(&fila("context7", ToolServerKind::Mcp, 100, 1, None));

        // 2. Incompleta: turno abortado, la lista es un prefijo.
        let mut a_medias = ToolCalls::default();
        a_medias.push_invoked("Read");
        reg.ingest(&fila(
            "context7",
            ToolServerKind::Mcp,
            100,
            1,
            Some(a_medias),
        ));

        // 3. Truncada: el cupo recortó, una invocación pudo quedar fuera.
        let mut truncada = ToolCalls::default();
        truncada.push_invoked("Read");
        truncada.invoked_total = 999;
        truncada.marca_completa();
        reg.ingest(&fila(
            "context7",
            ToolServerKind::Mcp,
            100,
            1,
            Some(truncada),
        ));

        let snap = reg.snapshot();
        let ctx = snap
            .servers
            .iter()
            .find(|s| s.server == "context7")
            .unwrap();

        assert_eq!(ctx.conclusive_requests, 0, "ninguna prueba nada");
        assert_eq!(ctx.descartes.sin_extractor, 1);
        assert_eq!(ctx.descartes.incompletas, 1);
        assert_eq!(ctx.descartes.truncadas, 1);
        assert_eq!(
            ctx.verdict,
            Verdict::InsufficientData {
                conclusive_requests: 0,
                needed: MIN_PETICIONES_CONCLUYENTES,
            },
            "tres filas descartadas no sostienen ningun veredicto"
        );
    }

    /// La asimetría central: ver una llamada prueba el uso aunque la fila no
    /// sirva para probar el no-uso. No contarla dejaría que un turno abortado
    /// escondiera la única prueba de que un servidor sí se usa.
    #[test]
    fn una_invocacion_en_fila_incompleta_cuenta_igual() {
        let mut reg = McpRegistry::default();

        let mut cortada = ToolCalls::default();
        cortada.push_invoked("mcp__context7__get-docs");
        // sin `marca_completa()`: el turno se abortó
        reg.ingest(&fila(
            "context7",
            ToolServerKind::Mcp,
            100,
            1,
            Some(cortada),
        ));

        let snap = reg.snapshot();
        let ctx = snap
            .servers
            .iter()
            .find(|s| s.server == "context7")
            .unwrap();

        assert_eq!(ctx.conclusive_requests, 0, "no prueba el NO-uso");
        assert_eq!(
            ctx.verdict,
            Verdict::Used { invocations: 1 },
            "pero si prueba el uso: la llamada se vio"
        );
    }

    /// `(native)` y `(others)` no son quitables, así que no reciben veredicto
    /// accionable ni con cero invocaciones y evidencia de sobra.
    #[test]
    fn los_cubos_no_mcp_nunca_reciben_recomendacion() {
        let mut reg = McpRegistry::default();
        for _ in 0..MIN_PETICIONES_CONCLUYENTES * 2 {
            reg.ingest(&fila(
                "(native)",
                ToolServerKind::Native,
                5_000,
                12,
                Some(invocando(&[])),
            ));
        }

        let snap = reg.snapshot();
        let nativo = snap
            .servers
            .iter()
            .find(|s| s.server == "(native)")
            .unwrap();
        assert_eq!(nativo.verdict, Verdict::NotApplicable);
        assert_eq!(nativo.bytes_per_request, 5_000, "el coste sí se publica");
    }

    /// El orden es estable y por coste descendente: un `HashMap` no lo es, y
    /// sin orden fijo esta salida no se podría afirmar ni leer cómodamente.
    #[test]
    fn las_filas_salen_por_coste_descendente_y_estable() {
        let mut reg = McpRegistry::default();
        for (servidor, bytes) in [("barato", 100), ("caro", 9_000), ("medio", 500)] {
            reg.ingest(&fila(
                servidor,
                ToolServerKind::Mcp,
                bytes,
                1,
                Some(invocando(&[])),
            ));
        }

        let snap = reg.snapshot();
        let orden: Vec<&str> = snap.servers.iter().map(|s| s.server.as_str()).collect();
        assert_eq!(orden, vec!["caro", "medio", "barato"]);
    }

    /// El umbral viaja en la respuesta: es un juicio, no una medida, y quien
    /// no esté de acuerdo necesita saber contra qué se comparó.
    #[test]
    fn el_umbral_se_publica_con_los_datos() {
        let snap = McpRegistry::default().snapshot();
        assert_eq!(snap.umbral, MIN_PETICIONES_CONCLUYENTES);
    }
}
