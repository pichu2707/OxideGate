# Tasks: Atribución de sesiones — Rebanada 1 (captura cruda en `/requests` + `telemetry.jsonl`)

### Unidades de trabajo sugeridas

| Unidad | Objetivo | PR probable | Notas |
|--------|----------|--------------|-------|
| 1 | Tipos `SessionAttribution`/`SessionSource` + resolver `session_of` en `proxy.rs` + tests unitarios de precedencia/saneo | PR 1 | Autocontenido: `session_of` es una función pura `&HeaderMap -> SessionAttribution`, testeable sin tocar la cadena de métricas. Base = main. ~220-240 líneas. |
| 2 | Hilado por `MetricBase` → `RequestMetric` → `RecentRequest` → `/requests`/`telemetry.jsonl`, doc "DOS→TRES campos no-planos", tests de proyección/round-trip en `recent.rs`, validación en vivo | PR 2 | Depende de PR 1. ~140-160 líneas. Base = main (stacked) o rama de PR 1 (feature-branch-chain), según la estrategia de cadena elegida. |

## Phase 1: Fundamento — tipos y resolver

- [x] 1.1 Crear `src/telemetry/session.rs` con doc `//!` de módulo: explica el contrato de honestidad (`source`+`key` inseparables, el fallback es un valor real no una ausencia), la precedencia de tres niveles y que la resolución es transporte puro (mismo `&HeaderMap` que `client_of`).
- [x] 1.2 Definir `pub enum SessionSource { Explicit, Native, Unattributed }` con `#[derive(Debug, Clone, PartialEq, Serialize)]` y `#[serde(rename_all = "snake_case")]` (→ `"explicit"`/`"native"`/`"unattributed"`); doc `///` de una línea por variante indicando qué señal ganó la precedencia en cada caso.
- [x] 1.3 Definir `pub struct SessionAttribution { pub source: SessionSource, pub key: String }` con `#[derive(Debug, Clone, PartialEq, Serialize)]`; doc `///` por campo explicando que `key` sin `source` no es interpretable (una `key` de `User-Agent` bajo `Unattributed` NO es una identidad).
- [x] 1.4 `src/telemetry/mod.rs`: añadir `pub mod session;` y `pub use session::{SessionAttribution, SessionSource};` (mismo patrón que `codex_quota`, `mod.rs:2/9`).
- [x] 1.5 `src/middleware/proxy.rs`: implementar `fn session_of(headers: &HeaderMap) -> SessionAttribution`, adyacente a `client_of` (~línea 36). Precedencia: `X-OxideGate-Session` (no vacío tras `trim`) → `Explicit`; si no, `x-claude-code-session-id` (no vacío) → `Native`; si no, fallback → `Unattributed` con `key` = `User-Agent` crudo (reusando el criterio de tope de longitud/UTF-8 válido de `client_of`) o la constante `"unattributed"` si el `User-Agent` falta o no es UTF-8 válido. Un header presente pero vacío se trata como ausente (nunca produce `Some("")`). El resolver lee EXCLUSIVAMENTE esas tres cabeceras, nunca `Authorization`/API keys.
- [x] 1.6 Doc `///` de `session_of`: documentar la tabla de precedencia y la invariante de privacidad (no lee credenciales) directamente sobre la función.

## Phase 2: Tests unitarios de `session_of` (precedencia y saneo)

Crear módulo `#[cfg(test)] mod tests` en `src/middleware/proxy.rs` (no existe aún) con `HeaderMap` sintéticos:

- [x] 2.1 Test: `X-OxideGate-Session` presente y no vacío → `Explicit` con esa clave, independientemente de si `x-claude-code-session-id` también está presente.
- [x] 2.2 Test: `X-OxideGate-Session` ausente, `x-claude-code-session-id` presente y no vacío → `Native` con esa clave.
- [x] 2.3 Test: ambos headers de atribución ausentes, `User-Agent` presente → `Unattributed` con `key` = valor del `User-Agent`.
- [x] 2.4 Test: `X-OxideGate-Session` presente pero vacío (`""`), `x-claude-code-session-id` presente y no vacío → `Native` con esa clave, nunca el string vacío (saneo, cae de nivel).
- [x] 2.5 Test: ambos headers de atribución presentes pero vacíos → `Unattributed` con el `User-Agent` como valor, nunca un string vacío.
- [x] 2.6 Test: fallback sin ningún `User-Agent` presente → `key` = constante `"unattributed"` (no el string vacío, no `None`).
- [x] 2.7 Test de invariante de privacidad: petición con header de credencial (`Authorization` o API key) y `X-OxideGate-Session: claude-1` simultáneos → `key` es `claude-1`, ningún campo del resultado contiene el valor de la credencial.
- [x] 2.8 Test explícito de la invariante "nunca `Some("")`": recorrer los casos de saneo (2.4, 2.5) y afirmar además que `key` nunca es `String::new()` en ninguna rama.

## Phase 3: Hilado por la cadena de métricas

- [x] 3.1 `src/telemetry/metered.rs`: añadir `pub session: SessionAttribution` a `MetricBase` (junto a `client`, ~línea 48), con doc `///` breve.
- [x] 3.2 `src/telemetry/metered.rs` (`MeteredBody::emit`, ~línea 290): añadir `session: self.base.session.clone()` al literal de `RequestMetric`.
- [x] 3.3 `src/telemetry/logger.rs`: añadir `pub session: SessionAttribution` a `RequestMetric` con doc `///` (contrato de honestidad: `source` fija cómo interpretar `key`).
- [x] 3.4 `src/telemetry/logger.rs`: actualizar el doc de `tools_by_server` (~línea 160, "Uno de los DOS campos no-planos de la fila...") y el de `codex_quota` (~línea 220, "SEGUNDO campo no-plano...") para reflejar que `session` es el TERCER campo no-plano.
- [ ] 3.5 `src/telemetry/recent.rs`: añadir `pub session: SessionAttribution` a `RecentRequest` con doc `///` (misma semántica que en `RequestMetric`). **[PR2]**
- [ ] 3.6 `src/telemetry/recent.rs` (`impl From<&RequestMetric> for RecentRequest`, ~línea 205): añadir `session: m.session.clone()`. **[PR2]**
- [x] 3.7 `src/middleware/proxy.rs` (literal `base` en `send_and_meter`, ~línea 213): añadir `session: session_of(req_headers)`, calculado una vez al inicio de la función (junto a `client`, ~línea 120) y movido (no clonado) hacia `base`, siguiendo el mismo patrón que `client`.
- [x] 3.8 `src/middleware/proxy.rs` (rama de error del upstream, ~línea 161): añadir `session: <la misma variable resuelta al inicio>` al literal de `RequestMetric` de la rama de error — el fallback honesto se aplica de forma natural porque `session_of` ya se resolvió antes de invocar al upstream, sin caso especial.
- [x] 3.9 `src/middleware/proxy.rs`: verificar el análisis de flujo del compilador (rama de error hace `return` antes de la rama de éxito) para que el `move` de `session` sin clonar compile en ambos usos, igual que `client`.

**Nota de PR1**: `session` se convirtió en campo REQUERIDO (no `Option`) de `RequestMetric`, así que las fixtures `#[cfg(test)] base_metric()` de `recent.rs` y `telemetry/stats.rs` necesitaron una línea `session: SessionAttribution { source: SessionSource::Unattributed, key: "unattributed".into() }` para seguir compilando (restricción del lenguaje: literal de struct exhaustivo, no relacionada con 3.5/3.6). Ningún otro cambio tocó esos dos archivos en PR1.

## Phase 4: Tests de hilado (`recent.rs`)

- [ ] 4.1 `base_metric()` (fixture de tests, ~línea 250): añadir `session: SessionAttribution { source: SessionSource::Unattributed, key: "unattributed".into() }` (o equivalente) como valor por defecto. **[Parcial en PR1: el valor por defecto ya está en la fixture por la restricción del compilador citada arriba; el resto de 4.1 (uso en aserciones de proyección) queda para PR2.]**
- [ ] 4.2 Extender el test de proyección existente (`proyeccion_copia_campos_fielmente_incluyendo_none` o equivalente) para afirmar que `row.session` copia fielmente el valor del `RequestMetric` fuente. **[PR2]**
- [ ] 4.3 Nuevo test: proyección copia `session` fielmente cuando `source = SessionSource::Explicit` (fixture con clave explícita). **[PR2]**
- [ ] 4.4 Nuevo test: proyección copia `session` fielmente cuando `source = SessionSource::Native`. **[PR2]**
- [ ] 4.5 Nuevo test: round-trip serde de `RecentRequest` con `session.source = Explicit` — afirmar que el JSON serializa `"source": "explicit"` y la `key` correspondiente (patrón de `round_trip_serde_con_codex_quota_presente`). **[PR2 — round-trip equivalente ya cubierto para `RequestMetric` directamente en `logger.rs::tests::round_trip_serde_con_session_explicit`, PR1]**
- [ ] 4.6 Nuevo test: round-trip serde con `session.source = Native` — mismo patrón, afirmar `"source": "native"`. **[PR2 — equivalente en `logger.rs::tests::round_trip_serde_con_session_native`, PR1]**
- [ ] 4.7 Nuevo test: round-trip serde con `session.source = Unattributed` — afirmar `"source": "unattributed"` y que `key` lleva el `User-Agent` o la constante según el fixture. **[PR2 — equivalente en `logger.rs::tests::round_trip_serde_con_session_unattributed`, PR1]**

## Phase 5: Verificación y validación en vivo

- [ ] 5.1 Validación en vivo: confirmar que al menos un harness real estampa `X-OxideGate-Session` de punta a punta a través del proxy y que la clave aparece en la fila correspondiente de `GET /requests`. Claude Code y Gemini son los harnesses estables para esta verificación; OpenCode es conocido por ser inestable propagando headers custom — si falla con OpenCode, documentarlo como limitación conocida del harness, no como bug del resolver. **[PR2 — depende de que `/requests` exponga `session`]**
- [ ] 5.2 Validación en vivo complementaria: confirmar que una petición SIN `X-OxideGate-Session` pero con `x-claude-code-session-id` (Claude Code nativo) resuelve a `source = "native"` en `/requests`. **[PR2]**
- [ ] 5.3 Validación en vivo complementaria: confirmar que una petición sin ninguno de los dos headers de atribución resuelve a `source = "unattributed"` con el `User-Agent` real del harness como `key`. **[PR2]**
- [x] 5.4 Ejecutar `cargo test` — todo en verde, sin fallos. **[Verificado para el alcance de PR1: 133 (oxidegate) + 108 (oxidegate-monitor) + 0 (oxidegate-bench) tests, todos en verde.]**
- [x] 5.5 Ejecutar `cargo clippy --all-targets` — sin warnings. **[Verificado para el alcance de PR1, incluyendo una recompilación limpia (`cargo clean -p oxidegate`).]**
- [ ] 5.6 Ejecutar `cargo fmt` — sin diffs pendientes. **[Bloqueado por drift de versión de rustfmt preexistente en todo el repo, no introducido por PR1 — ver nota en apply-progress. Los 6 archivos tocados por PR1 están formateados de forma consistente con el resto del repo (mismo estilo que el código no tocado).]**

**Fuera de alcance (no tasked aquí):** agregación por sesión en `GET /stats` (rebanada 2), columna o panel de sesión en `oxidegate-monitor` (rebanada 3), distinción de subagentes (`x-claude-code-agent-id`/`x-claude-code-parent-agent-id`) — per `proposal.md`/`spec.md`.

## Review Workload Forecast

| Campo | Valor |
|-------|-------|
| Líneas estimadas cambiadas | 350-400 |
| Riesgo de presupuesto de 400 líneas | Medium (el módulo `session.rs` con `//!`/`///` exhaustivos, más 8 tests de precedencia en `proxy.rs` y 7 tests de hilado en `recent.rs`, infla el conteo igual que ocurrió en la rebanada 1 de `codex_quota`, pese a que `SessionAttribution` tiene solo 2 campos frente a los 12 de `CodexQuota`) |
| Chained PRs recommended | Yes |
| División sugerida | PR 1 (tipos + resolver + tests de precedencia, ~220-240 líneas) → PR 2 (hilado + docs + tests de proyección/round-trip + validación en vivo, ~140-160 líneas) |
| Delivery strategy | ask-on-risk |
| Chain strategy | pending (a decidir por el orquestador: `stacked-to-main` o `feature-branch-chain`) |
| Decision needed before apply | Yes |
