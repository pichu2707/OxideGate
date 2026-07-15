# Propuesta — Eje de atribución de sesiones para tráfico concurrente

Esta propuesta añade a OxideGate un **eje de atribución de sesiones** que separa
el tráfico concurrente por su origen real: distinguir varias sesiones a la vez
—incluso varias del **mismo** harness (p. ej. dos Claude Code)— además de
mezclas Claude / Gemini / OpenCode. Hoy OxideGate solo sabe el **tipo** de
harness (`client`, leído del `User-Agent`), no la **sesión**: dos sesiones de la
misma tecnología se funden e indistinguibles en el agregado. El eje es una
**dimensión nueva** de la métrica —una clave de sesión opaca, jamás una
credencial— que se **asigna** explícitamente al lanzar, nunca se infiere. Se
entrega de forma incremental: la primera rebanada captura la clave cruda por
petición y la expone en `/requests` y en `telemetry.jsonl`, sentando la base de
datos reales sobre la que se diseñarán las rebanadas siguientes.

## Por qué (el problema)

- OxideGate mide cada petición y la agrega por `(upstream, modelo)`, pero **no
  puede decir de qué sesión vino**. El único identificador de origen es `client`
  (`src/middleware/proxy.rs::client_of`), leído del `User-Agent`: da el tipo de
  harness (`claude-cli/1.2.3` vs `opencode/…`), no la sesión.
- El caso de uso que lo motiva: comparar interacciones concurrentes que pueden
  tener **distintos conjuntos de MCP conectados** (X servidores en una, Y en
  otra), y a futuro un escenario Cloud multi-tenant con muchos hilos midiéndose a
  la vez.
- La identidad de sesión **no se puede inferir**: querer separar dos sesiones del
  mismo harness elimina de raíz todo identificador implícito (`User-Agent`,
  credencial, huella de MCPs, `prompt_hash`) porque todos colisionan para
  sesiones de la misma tecnología. La identidad se **asigna** de forma explícita,
  lo cual es honesto y encaja con el principio del proyecto de no fabricar datos.

## Qué (la visión completa del eje)

Un eje de atribución de sesiones, **estrictamente etiqueta opaca** (jamás una
credencial), presente en tres superficies. El eje completo es la meta; se
entrega en rebanadas encadenadas.

### La clave de sesión y su precedencia (decisión ya fijada, no se reabre)

La clave se resuelve por precedencia, de la señal más humana al fallback más
honesto. **La identidad se asigna, nunca se infiere.**

| Orden | Fuente | Naturaleza |
|-------|--------|-----------|
| 1 | **`X-OxideGate-Session`** — etiqueta explícita del usuario (`claude-1`, `gemini`, `opencode`) | Gana siempre. Es la intención declarada del humano. |
| 2 | **`x-claude-code-session-id`** — header de sesión nativo de Claude Code cuando está presente | Atribución automática si no se etiquetó. Único por sesión, funciona con cualquier auth. |
| 3 | **Fallback** — `User-Agent` + bucket explícito **"sin atribuir"** | Honesto: no inventa una identidad que no existe. |

### Las tres superficies

- **`/requests`** (detalle por petición): la clave de sesión cruda.
- **`/stats`** (agregado): agrupación por sesión (group-by).
- **Monitor TUI** (`oxidegate-monitor`): panel/columna de sesión.

### Invariante de privacidad (no negociable)

Nunca se loguean secretos (API keys, tokens OAuth) en la telemetría, coherente
con `docs/telemetry-per-request.md`. La clave de atribución es una **etiqueta o
identificador opaco**, jamás una credencial cruda. Los valores de header con
**string vacío** mapean al fallback/`None`, nunca a `Some("")`.

## Alcance y rebanadas

Se entrega el **eje completo como meta**, pero de forma **incremental** en
rebanadas encadenadas, cada una ≤ 400 líneas cambiadas (presupuesto de
revisión), con PRs encadenados (estrategia de entrega: `ask-on-risk`). Cada
rebanada deja datos reales que informan el diseño de la siguiente, replicando el
patrón "capturar crudo primero, derivar después" del eje de cuota.

### Rebanada 1 (esta entrega): captura cruda en `/requests` + `telemetry.jsonl`

**Objetivo mínimo**: resolver la clave de sesión por precedencia y exponerla
cruda por petición en `/requests` y en `telemetry.jsonl`, sin agregar ni
derivar nada.

Mecánica de la captura (verificada contra el código, réplica del eje de cuota):

- En `src/middleware/proxy.rs::send_and_meter`, las **cabeceras del request** ya
  están disponibles de forma síncrona. La clave se resuelve justo ahí, aplicando
  la precedencia: **cero buffering, cero riesgo para el stream SSE**.
- El campo nuevo se hila por `MetricBase` (`src/telemetry/metered.rs`) hasta
  `RequestMetric` y de ahí a la superficie `/requests`.
- La rama de **error de upstream** deja el campo en el fallback honesto de forma
  natural, consistente con el principio de honestidad.
- **Frontera del trait Provider respetada**: el transporte en
  `middleware/proxy.rs` permanece agnóstico del proveedor; ninguna lógica
  específica de proveedor se filtra. La resolución de la clave es transporte
  puro sobre headers, no lógica de proveedor.
- Regla de saneo explícita: valores de header con **string vacío** mapean al
  fallback/`None`, nunca a `Some("")`.

Cabeceras de entrada a leer (en orden de precedencia): `X-OxideGate-Session`,
`x-claude-code-session-id`, y el `User-Agent` existente como fallback junto al
bucket **"sin atribuir"**.

**No-objetivos de la rebanada 1 (explícitos, diferidos a rebanadas posteriores):**

- Sin agregación ni group-by en `/stats`.
- Sin panel ni columna de sesión en el monitor TUI.
- Sin distinción de subagentes (`x-claude-code-agent-id` /
  `x-claude-code-parent-agent-id`), aunque la señal nativa exista.

Se difiere a propósito para que el diseño de las superficies derivadas parta de
datos reales capturados, replicando el patrón ya establecido en el proyecto de
"capturar crudo primero, derivar después".

### Cadena de rebanadas siguientes (orden fijado)

1. **Rebanada 1** — captura cruda de la clave de sesión en `/requests` +
   `telemetry.jsonl` *(esta entrega)*.
2. **Agregación en `/stats`** — group-by por clave de sesión sobre las métricas
   ya capturadas.
3. **Panel en el monitor TUI** — vista/columna de sesión en `oxidegate-monitor`.

## No-objetivos (de todo el cambio)

- **No** se infiere identidad de ninguna huella implícita (`User-Agent`,
  credencial, `tools_by_server`, `prompt_hash`): todas colisionan para sesiones
  de la misma tecnología. La identidad se asigna, no se adivina.
- **No** se loguea ninguna credencial: la clave es una etiqueta opaca. La
  invariante de privacidad es no negociable.
- **No** se aborda el escenario Cloud multi-tenant en este eje: OxideGate hoy no
  tiene capa de auth propia (solo reenvía headers intactos). Se resuelve cuando
  el caso local esté asegurado.
- **No** se rediseña `client_of` ni el `User-Agent`: se preserva como fallback.
- **No** se depende de OpenCode: Claude Code y Gemini ya son sólidos; OpenCode se
  valida en vivo dentro del slice, no como bloqueante previo.

## Riesgos y supuestos

| Tema | Detalle | Tratamiento |
|------|---------|-------------|
| OpenCode frágil | Historial de bugs donde los headers de config no llegan al `fetch`; issues cerrados por bot sin confirmación del mantenedor. | **Validar en vivo contra el propio proxy dentro del slice, NO como bloqueante previo.** El diseño no depende de OpenCode. Es una tarea del slice, no una precondición. |
| Gemini fuera de docs | El feature (`GEMINI_CLI_CUSTOM_HEADERS`) existe en el código (v0.50.0) pero no en los docs renderizados. | Se documenta el mecanismo en el repo para no depender de la doc oficial. |
| Claude Code + OAuth | `ANTHROPIC_CUSTOM_HEADERS` no está doc-confirmado bajo OAuth (Max). | Cubierto: el `x-claude-code-session-id` nativo (precedencia 2) funciona con cualquier auth. |
| Granularidad OpenCode | La interpolación `{env:}` se resuelve una vez, al arrancar el proceso: por-lanzamiento, no por-conversación intra-proceso. | Aceptado y declarado explícito: "sesión" = una invocación del harness. |
| Colisión con `Some("")` | Un header presente pero vacío no debe crear una identidad falsa. | Regla de saneo: string vacío mapea al fallback/`None`, nunca a `Some("")`. |

Principio transversal (no negociable): lo desconocido es fallback honesto
("sin atribuir")/`None`, nunca una identidad inventada. La clave es una etiqueta
opaca, jamás una credencial.

## Plan de rollback (rebanada 1)

La captura es aditiva y de bajo riesgo, pero se declara la reversión explícita:

- El cambio añade **un campo nuevo** hilado por `MetricBase` → `RequestMetric` →
  `/requests` + `telemetry.jsonl`. No modifica el flujo del stream SSE ni el
  contrato existente de métricas.
- **Rollback**: revertir el commit de la rebanada elimina el campo; las
  superficies vuelven a su forma previa sin migración de datos. El
  `telemetry.jsonl` histórico con el campo nuevo sigue siendo válido (campo
  aditivo; los consumidores lo ignoran si no lo esperan).
- No hay estado persistente que migrar ni esquema que versionar: el riesgo de
  reversión es un `git revert` limpio del PR de la rebanada.

## Criterios de éxito (rebanada 1)

- Una petición con `X-OxideGate-Session: claude-1` produce una fila en
  `/requests` (y una línea en `telemetry.jsonl`) con la clave `claude-1`.
- Una petición de Claude Code **sin** `X-OxideGate-Session` pero **con**
  `x-claude-code-session-id` lleva ese id nativo como clave de sesión.
- Una petición sin ninguno de los dos headers cae al fallback: `User-Agent` +
  bucket **"sin atribuir"**, nunca una identidad inventada.
- Un header presente pero con string vacío mapea al fallback/`None`, no a
  `Some("")`.
- La rama de error de upstream deja la clave en el fallback honesto.
- Ninguna credencial (API key, token OAuth) aparece jamás en la telemetría.

## Siguiente paso

Con la propuesta aprobada, avanzan en paralelo `sdd-spec` (contrato de datos de
la rebanada 1: forma, precedencia y saneo de la clave de sesión capturada) y
`sdd-design` (arquitectura del hilado desde `send_and_meter` hasta `/requests` +
`telemetry.jsonl`, y decisión de nombre/forma del campo respetando la frontera
del trait Provider).
