# Diseño: Eje de coste por cuota (Codex OAuth) — Rebanada 1

## Enfoque técnico

La rebanada 1 captura las cabeceras `x-codex-*` de la respuesta del backend de
Codex y las expone crudas en `/requests`, sin derivar nada. La captura es
puramente aditiva y se apoya en un hallazgo ya verificado: en
`send_and_meter`, `resp.headers()` está vivo justo antes de que
`resp.bytes_stream()` consuma `resp`. Se introduce **un único tipo de acarreo**,
`CodexQuota`, en un módulo dedicado, que viaja como `Option<CodexQuota>` por la
misma cadena que ya recorren `tools_by_server` y los campos `context_*`:
`MetricBase` → `RequestMetric` → `RecentRequest` → JSON de `/requests`.

Se diseña la **arquitectura del eje completo** (para que las rebanadas 2‑5 no
fuercen un rediseño), pero solo la rebanada 1 se especifica al detalle.

## Decisiones de arquitectura

### Decisión: forma anidada `Option<CodexQuota>`, no campos planos

**Elección**: un struct `CodexQuota` con sus ~12 campos, acarreado como
`Option<CodexQuota>` (un solo campo nuevo por estructura).

**Alternativas consideradas**: ~12 campos planos replicados en `MetricBase`,
`RequestMetric` y `RecentRequest`.

**Justificación**:
- **Convención de documentación total**: plano = 12 `///` × 3 structs ≈ 36
  doc‑comments + 12 copias por sitio de traslado (4 sitios). Anidado = un solo
  `///` por estructura y la documentación por campo se concentra **una vez** en
  el módulo nuevo. Es el factor decisivo para el presupuesto de 400 líneas.
- **Separación estructural** (invariante innegociable): un tipo aparte, en un
  módulo aparte, SIN ningún campo en USD, es incapaz por construcción de
  contaminar `cost_estimate_usd`.
- **Envejece mejor**: cada rebanada derivada (agregación, TUI, nocional, delta
  marginal) trata "la lectura de cuota" como una unidad direccionable. La
  comparación de filas consecutivas de la rebanada 5 lee `row.codex_quota` como
  un bloque; 12 campos dispersos lo harían frágil.
- **Precedente**: `tools_by_server` ya es anidado (`Option<Vec<ToolServerBytes>>`).

**Consecuencia de honestidad documental**: el doc‑comment de
`RequestMetric::tools_by_server` afirma hoy ser "EL ÚNICO CAMPO NO‑PLANO". Con
`codex_quota` deja de serlo — ese comentario DEBE actualizarse en esta rebanada.

### Decisión: parser dedicado por presencia de cabeceras, no método del trait `Provider`

**Elección**: módulo nuevo `src/telemetry/codex_quota.rs` con
`CodexQuota::from_headers(&HeaderMap) -> Option<CodexQuota>`, disparado por la
**presencia** de cabeceras `x-codex-*`.

**Alternativas consideradas**:
1. Método `Provider::extract_quota` (dispatch por proveedor).
2. Parseo inline en `proxy.rs`.

**Justificación**:
- La señal autoritativa fijada en la propuesta es **presencia de cabeceras por
  petición, NO identidad de upstream**. `api.openai.com` (API key) comparte el
  proveedor `openai` pero NO trae `x-codex-*`: un dispatch por proveedor
  representaría mal la señal y cargaría a Anthropic/Gemini con un default muerto.
  Un parser por presencia devuelve `None` para TODO tráfico sin cabeceras
  (Anthropic, Gemini y `api.openai.com`) sin dispatch alguno.
- Parsear inline en `proxy.rs` metería el dialecto Codex en el transporte
  genérico, violando el límite adaptador/transporte (convención 6). Un módulo
  dedicado mantiene `proxy.rs` fino: una sola llamada.
- Mismo criterio que `pricing.rs` (la semántica vive pegada al tipo): las reglas
  de saneo de cuota viven junto al tipo `CodexQuota`, no pueden derivar.

### Decisión: saneo y tipado con hueco honesto

| Cabecera | Tipo destino | Regla |
|----------|--------------|-------|
| `primary/secondary-used-percent`, `*-window-minutes`, `primary-reset-after-seconds` | `Option<u64>` | parse entero; fallo → `None` |
| `primary/secondary-reset-at` | `Option<i64>` (unix ts) | parse entero; fallo → `None` |
| `plan-type`, `active-limit`, `credits-balance` | `Option<String>` | crudo; vacío → `None` |
| `credits-has-credits`, `credits-unlimited` | `Option<bool>` | `True`/`False` → bool; otro → `None` |

Reglas transversales: **string vacío → `None`** (nunca `Some("")`); **malformado
→ `None`**, jamás `panic`, jamás un `0` fabricado. `from_headers` devuelve `None`
completo si NINGUNA cabecera `x-codex-*` está presente.

## Flujo de datos

    resp.headers()  ── CodexQuota::from_headers ──▶  MetricBase.codex_quota
    (proxy.rs ~209)                                        │
                                                           ▼  (emit, metered.rs)
                                          RequestMetric.codex_quota
                                                           │  (From, recent.rs)
                                                           ▼
                                          RecentRequest.codex_quota ──▶ /requests JSON

Rama de error de upstream (proxy.rs ~161): no existe `resp` → `codex_quota:
None`, honesto por construcción.

## Punto de captura (exacto)

`src/middleware/proxy.rs`, en la construcción del literal `MetricBase { … }`
(líneas ~209‑227), se añade el campo:

    codex_quota: CodexQuota::from_headers(resp.headers()),

`resp` está vivo ahí: `resp.status()` ya se leyó en la línea 206 (préstamo
inmutable) y el bucle de copia de cabeceras a la respuesta saliente está en la
línea ~232, DESPUÉS. `from_headers` hace lookups puntuales `get("x-codex-…")`
—no recorre ni bufferiza—, así que no es un segundo pase costoso ni toca el
stream SSE. NO se piggybackea en el bucle de transporte de ~232: ese bucle es
responsabilidad de transporte y no debe conocer el dialecto Codex.

## Hilado campo a campo

| Sitio | Archivo:línea | Cambio |
|-------|---------------|--------|
| `MetricBase` | `metered.rs` ~36 | nuevo campo `codex_quota: Option<CodexQuota>` |
| Construcción `base` | `proxy.rs` ~209 | `codex_quota: CodexQuota::from_headers(resp.headers())` |
| Rama de error | `proxy.rs` ~161 | `codex_quota: None` |
| `RequestMetric` | `logger.rs` ~26 | nuevo campo `codex_quota: Option<CodexQuota>` |
| `MeteredBody::emit` | `metered.rs` ~250 | `codex_quota: self.base.codex_quota.clone()` |
| `RecentRequest` | `recent.rs` ~65 | nuevo campo `codex_quota: Option<CodexQuota>` |
| `RecentRequest::from` | `recent.rs` ~166 | `codex_quota: m.codex_quota.clone()` |
| `/requests` handler | `requests.rs` | sin cambios (serializa el snapshot tal cual) |

`CodexQuota` deriva `Debug, Clone, Serialize, PartialEq` (necesario por los
`.clone()` de traslado y por los round‑trips serde de los tests, igual que
`ToolServerBytes`). El `base_metric` de los tests de `recent.rs` necesita el
campo nuevo.

## Cambios de archivos

| Archivo | Acción | Descripción |
|---------|--------|-------------|
| `src/telemetry/codex_quota.rs` | Crear | Tipo `CodexQuota` + `from_headers` + saneo + tests unitarios |
| `src/telemetry/mod.rs` | Modificar | Declarar `pub mod codex_quota;` y re‑exportar `CodexQuota` |
| `src/telemetry/metered.rs` | Modificar | Campo en `MetricBase` + traslado en `emit` |
| `src/telemetry/logger.rs` | Modificar | Campo en `RequestMetric` |
| `src/telemetry/recent.rs` | Modificar | Campo en `RecentRequest` + `From` + `base_metric` de tests |
| `src/middleware/proxy.rs` | Modificar | Captura en `base` + `None` en rama de error |

## Invariante de honestidad (garantía estructural)

- `CodexQuota` es un tipo distinto, en un módulo distinto, SIN campo en USD: no
  puede sostener un importe en dólares.
- `pricing::estimate_cost_usd` recibe conteos de tokens y devuelve
  `cost_estimate_usd`; nunca recibe un `CodexQuota`. Las dos computaciones no
  comparten entradas ni ruta de código: en `emit`, `cost_estimate_usd` sale de
  `self.scanner.usage` y `codex_quota` de `self.base` —bloques independientes.
- La cuota es porcentaje de ventana, jamás dólares. No existe función que tome
  ambos y produzca un número fusionado.

## Estrategia de pruebas

| Capa | Qué | Cómo |
|------|-----|------|
| Unit | `from_headers`: parseo por tipo, vacío→`None`, malformado→`None`, ausencia total→`None`, `True/False`→bool | tests en `codex_quota.rs` con `HeaderMap` sintéticos |
| Unit | Proyección `RequestMetric`→`RecentRequest` copia `codex_quota` fiel (Some y None) | ampliar tests de `recent.rs` |
| Unit | Round‑trip serde de `RecentRequest` con `codex_quota` presente y `null` | patrón existente de `round_trip_serde_*` |

## Vision del eje completo (rebanadas 2‑5) — no bloqueado

- **/stats (2)**: `StatsRegistry::ingest` ya recibe `RequestMetric`; lee
  `metric.codex_quota` como unidad para el estado agregado. Sin reshape.
- **TUI (3)**: nueva vista tipo ciclo `c` del monitor sobre datos existentes
  (`codex_quota` ya presente en `/requests`/`/stats`).
- **Nocional (4)**: campo SEPARADO `notional_api_cost_usd`, reusando
  `pricing.rs` con entradas GPT‑5, etiquetado como estimación; NUNCA dentro de
  `CodexQuota` (preserva la separación).
- **Delta marginal (5)**: compara `codex_quota.primary_used_percent` entre filas
  consecutivas (patrón del detector `TRUNC`); la salvedad de redondeo de enteros
  vive en la capa de derivación, no en la captura.

La forma anidada envejece mejor porque cada rebanada trata la lectura como una
unidad.

## Presupuesto de revisión (400 líneas)

Anidado: módulo nuevo (~180‑250 líneas con doc + parser + tests) + 1 campo/1 doc
por estructura (~12) + ~4 sitios de traslado + edición del doc de
`tools_by_server` + churn de tests ≈ **250‑320 líneas**. Dentro del presupuesto
con margen. La forma plana rondaría 350‑450+ y sería mucho más densa de revisar:
el anidado es TAMBIÉN la opción segura de presupuesto. Vigilar la densidad de
doc por campo en el módulo nuevo; el contrato compartido (vacío→`None`,
malformado→`None`, nunca fabricar) se documenta una sola vez en el `//!` del
módulo para no repetirlo por campo.

## Preguntas abiertas

- [x] Recuento exacto de campos: 12 campos, confirmado por `spec.md` y por la
  implementación de `CodexQuota` (rebanada 1). Se corrigió el "~11" residual
  en `proposal.md`.
- [ ] Cabeceras `x-codex-*` en respuestas no‑200 (p. ej. `429`): supuesto a
  validar en rebanada temprana, no bloquea la rebanada 1.
