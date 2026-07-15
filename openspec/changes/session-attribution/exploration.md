# Exploración — Eje de atribución de sesiones/interacciones

Investigación previa a la propuesta de un **eje de atribución** que permita a
OxideGate separar el tráfico concurrente por su origen: distinguir varias
sesiones a la vez —incluso varias del **mismo** harness (p. ej. dos Claude
Code)— además de mezclas Claude / Gemini / OpenCode. No se crea contrato ni
diseño aquí; se fija el problema, se descartan los caminos falsos y se deja
verificado el mecanismo sobre el que se construirá.

## El problema

Hoy OxideGate mide cada petición y la agrega por `(upstream, modelo)`, pero
**no puede decir de qué sesión vino**. El único campo que identifica el origen
es `client` (`src/middleware/proxy.rs::client_of`), leído del header
`User-Agent`. Eso da el **tipo de harness** (`claude-cli/1.2.3` vs
`opencode/…`), no la sesión: dos sesiones de Claude Code, o dos agentes con la
misma tecnología, llegan indistinguibles y se funden en el agregado.

El caso de uso que lo motiva: comparar interacciones concurrentes que pueden
tener **distintos conjuntos de MCP conectados** (X servidores en una, Y en
otra), y a futuro un escenario Cloud multi-tenant con muchos hilos midiéndose a
la vez.

## La restricción dura (lo que cierra el diseño)

Querer separar **dos sesiones del mismo harness** elimina de raíz todo
identificador implícito. Ninguno discrimina:

| Candidato implícito | Por qué colisiona |
|---|---|
| `User-Agent` (`client`) | Mismo harness → misma cadena. |
| Credencial (API key / OAuth) | Misma cuenta (p. ej. un Claude Max) → misma credencial. Además **prohibido loguearla** (ver invariante de privacidad). |
| `tools_by_server` (huella de MCPs) | Mismos MCPs → misma huella. Una huella no es una identidad. |
| `prompt_hash` | Cambia en cada turno (el body crece); no es estable por conversación. |

**Conclusión central: la identidad de sesión no se puede inferir, se tiene que
asignar de forma explícita al lanzar.** Y eso está bien: es honesto y encaja
con el principio del proyecto de no fabricar datos.

**Invariante de privacidad (no negociable):** nunca se loguean secretos (API
keys, tokens OAuth) en la telemetría, coherente con
`docs/telemetry-per-request.md`. La clave de atribución es una etiqueta o un
identificador opaco, jamás una credencial cruda.

## Opciones evaluadas

### Opción A — la etiqueta es del proceso proxy

Cada instancia de `oxidegate` se lanza con una etiqueta (p. ej. vía env var) y
la estampa en cada fila.

- **A favor:** cero cambios en el cliente; harness-agnóstico (funciona con
  cualquier tecnología, incluso una que no sepa mandar headers); transparencia
  total intacta.
- **En contra:** hay que correr **un proxy por sesión**; el agregado de
  `/stats` vive en RAM **por proceso**, así que un solo monitor no ve todas las
  sesiones juntas sin trabajo extra de unificación (el `telemetry.jsonl` en
  disco sí se une gratis, el `/stats` en vivo no).

### Opción B — la etiqueta viaja en un header del request

Un **único** proxy; la sesión entra como una **dimensión** más de la métrica,
leída de un header que el cliente estampa por lanzamiento.

- **A favor:** un solo proxy ve todo; encaja con el patrón del repo (campo nuevo
  en `RequestMetric` + `group-by` en `/stats` y `/requests`); no rompe la
  transparencia para quien no etiquete (se mide agregado, como hoy).
- **En contra:** depende de que **cada harness** sepa inyectar un header custom
  por sesión — hecho que había que **verificar**, no suponer.

## Verificación del mecanismo (Opción B)

Se verificó, harness por harness, si cada uno permite estampar un header HTTP
custom **por lanzamiento** (una sesión = un proceso lanzado con su valor). Los
tres pueden. **La Opción B queda confirmada como viable.**

| Harness | Mecanismo | Por lanzamiento | En el cable | Nota |
|---|---|---|---|---|
| **Claude Code** | `ANTHROPIC_CUSTOM_HEADERS="X-OxideGate-Session: claude-1"` | ✅ env var | ✅ confirmado en docs oficiales | + señal nativa, abajo |
| **Gemini CLI** | `GEMINI_CLI_CUSTOM_HEADERS="X-OxideGate-Session: gemini-1"` | ✅ env var | ✅ bajo `AuthType.GATEWAY` (lo activa `GOOGLE_GEMINI_BASE_URL`) | vive en el código (v0.50.0), **caído de los docs renderizados** |
| **OpenCode** | `provider.*.options.headers` con interpolación `{env:OXIDEGATE_SESSION}` en `opencode.json` | ✅ por lanzamiento (no por-conversación intra-proceso) | ⚠️ arquitectónicamente sí, **con historial de bugs** | validar en vivo |

### Señal nativa de Claude Code (hallazgo que simplifica)

Claude Code **ya manda de fábrica** un header `x-claude-code-session-id` en cada
petición —único por sesión, pensado exactamente para que un gateway agregue el
tráfico de una sesión sin parsear el body— más `x-claude-code-agent-id` /
`x-claude-code-parent-agent-id` para distinguir subagentes. Es decir: para
Claude Code hay atribución **sin configuración**, y de yapa se separan los
subagentes.

### Fuentes

- Claude Code: `ANTHROPIC_CUSTOM_HEADERS` (docs `code.claude.com`, env-vars y
  `llm-gateway-protocol` "Request headers"); `x-claude-code-session-id` listado
  como header nativo por request en el mismo protocolo de gateway.
- Gemini CLI: PR `google-gemini/gemini-cli#11893` (merged 2025-11-26, issue
  #10088), implementación en `contentGenerator.ts` / `customHeaderUtils.ts`;
  live en `@google/gemini-cli@0.50.0`.
- OpenCode: `opencode.ai/docs/providers` (`options.headers`), interpolación
  `{env:VAR}` en `packages/opencode/src/config/variable.ts`; repo movido
  `sst/opencode` → `anomalyco/opencode`. Historial de fragilidad: issues
  #11789 (fixed por PR #11788), #15306, #22608 (cerrados por bot de staleness,
  **sin** confirmación del mantenedor).

## Decisión de dirección

- **Local primero, Cloud después.** El slice inicial resuelve el caso local
  (varias sesiones concurrentes en la misma máquina); el multi-tenant Cloud —
  donde la clave nace de la auth propia del gateway (principal/tenant)— se
  aborda cuando esto esté asegurado. OxideGate hoy **no tiene capa de auth
  propia**: solo reenvía headers intactos.
- **Opción B (un solo proxy, atribución por header).** Elegida sobre la A por
  la unificación en un solo `/stats` y por encajar con el patrón del repo, ya
  que la verificación confirmó que los tres harnesses pueden estampar el header.

### Precedencia de la clave de sesión (de más humana a fallback honesto)

1. **`X-OxideGate-Session`** — etiqueta explícita del usuario (`claude-1`,
   `gemini`, `opencode`). Gana siempre.
2. **Header de sesión nativo** — `x-claude-code-session-id` cuando está
   presente. Atribución automática si no se etiquetó.
3. **Fallback** — `User-Agent` + bucket **"sin atribuir"**. Honesto, no inventa
   una identidad que no existe.

## Encaje con el patrón del repo

Réplica del eje de cuota (`codex-quota-telemetry`): un campo nuevo hilado desde
la captura en `send_and_meter` hasta `RequestMetric` y las superficies, entrega
**incremental en rebanadas encadenadas ≤400 líneas**, y el patrón "capturar
crudo primero, derivar después" para que cada rebanada deje datos reales que
informen la siguiente. Superficies candidatas del eje completo: `/requests`
(detalle), `/stats` (agregado por sesión) y un indicador en el monitor TUI.

## Riesgos y supuestos abiertos (no bloqueantes del diseño)

| Tema | Detalle | Tratamiento |
|---|---|---|
| OpenCode frágil | Historial de bugs donde los headers de config no llegan al `fetch`; cierres por bot sin confirmación. | Validar **en vivo contra el propio proxy** dentro del slice, no como bloqueante previo. El diseño no depende de OpenCode: Claude Code y Gemini ya son sólidos. |
| Gemini fuera de docs | El feature existe en el código pero no en los docs renderizados. | Documentar el mecanismo en el repo (aquí) para no depender de la doc oficial. |
| Claude Code + OAuth | `ANTHROPIC_CUSTOM_HEADERS` no está doc-confirmado bajo OAuth (Max). | Cubierto: el `x-claude-code-session-id` nativo funciona con cualquier auth. |
| Granularidad OpenCode | La interpolación `{env:}` se resuelve una vez, al arrancar el proceso: por-lanzamiento, no por-conversación intra-proceso. | Aceptado: "sesión" = una invocación del harness. Se declara explícito. |

## Siguiente paso

Fijar en `proposal.md` la **frontera de la rebanada 1** (superficie mínima) y la
cadena de rebanadas siguientes, replicando la estructura del eje de cuota.
