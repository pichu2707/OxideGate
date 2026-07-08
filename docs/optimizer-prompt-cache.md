# Optimizador — Palanca A: forzar prompt caching de Anthropic

> Estado: implementado y cubierto por tests unitarios (`src/provider/anthropic.rs`).
> Apagado por defecto (`OXIDEGATE_FORCE_CACHE`). Anthropic-only por ahora.

---

## 1. Qué hace

Cuando un cliente pega a `/v1/messages` **sin gestionar su propio prompt
caching**, OxideGate le inyecta un breakpoint de `cache_control` para que
Anthropic cachee el prefijo estable del request (`tools` + `system`). Las
llamadas repetidas sobre ese mismo prefijo pagan `cache_read` (≈0.1x la
tarifa de input) en vez de tarifa plena.

Es la primera palanca del **optimizador**: el Nivel 1 (`docs/telemetry-level-1.md`)
mide; esta palanca es la primera decisión automática que toma OxideGate a
partir de esa medición.

## 2. Por qué

El benchmark documentado en `docs/benchmark.md` §5.1 midió un **overhead de
harness de ~7 368 tokens** (system prompt + definiciones de tools) que se
repite en **cada** request de una sesión de agente. Si ese prefijo no se
cachea, se repaga entero a tarifa plena en cada turno. Cachearlo es la
optimización de coste con mejor relación esfuerzo/impacto: no cambia el
output generado (es pura cuestión de billing/latencia en el proveedor), y el
prefijo estable es exactamente lo que más se repite turno a turno.

## 3. Cómo se activa

Variable de entorno `OXIDEGATE_FORCE_CACHE`:

```bash
export OXIDEGATE_FORCE_CACHE=true   # o "1"
```

**Default: `false` (apagado).** OxideGate es ante todo un **medidor
transparente**: por diseño, no muta requests salvo excepciones explícitas
(la otra es `stream_options.include_usage` de OpenAI, ver
`docs/provider-adapters.md`). Forzar `cache_control` es una mutación real del
body saliente, así que arranca apagada hasta que se decida prender la
palanca a propósito.

## 4. La regla: solo si el cliente no cachea ya

Antes de inyectar, `Anthropic::prepare` busca recursivamente la clave
`cache_control` en TODO el body (raíz, `system`, `tools`, `messages`, y
cualquier nivel anidado dentro de esos). Si encuentra **una sola**
ocurrencia, no toca nada.

Dos razones para esta regla, ambas duras:

- **Respetar los breakpoints del cliente.** Si el cliente ya cachea (p. ej.
  cachea su propio bloque `system` o mensajes largos del historial), pisar
  eso con un breakpoint propio no suma nada y puede degradar una estrategia
  de caching más fina que la nuestra (genérica, "todo el prefijo").
- **No superar el máximo de 4 breakpoints por request.** La API de Anthropic
  responde `400` si se pasa. Sumar un breakpoint propio arriba de los que ya
  puso el cliente es la forma más directa de pisar ese límite.

Cuando SÍ inyecta, lo hace de la forma más simple posible: escribe
`cache_control: {"type": "ephemeral"}` a **nivel raíz** del body. Anthropic
hace *prefix match* (el orden de render es `tools → system → messages`) y
**auto-coloca** ese `cache_control` en el último bloque cacheable — no hace
falta localizar a mano el bloque de `system` o el último `tool`.

> Nota sobre el mínimo cacheable: Anthropic no cachea bloques por debajo de
> ~1024-4096 tokens (según modelo), pero tampoco da error — la inyección es
> inocua en prefijos chicos, simplemente no logra nada.

## 5. El matiz: Claude Code ya cachea

Clientes como **Claude Code** gestionan su propio prompt caching (ya ponen
`cache_control` donde corresponde). Contra esos clientes, la detección de la
sección 4 hace que esta palanca **no haga nada** — que es exactamente lo
correcto: no hay overhead que evitar, y forzar un breakpoint propio arriba
del suyo solo arriesgaría el límite de 4.

Esta palanca es una **red de seguridad**, no un reemplazo: existe para los
clientes que hablan `/v1/messages` sin implementar caching propio (SDKs
livianos, scripts, integraciones caseras), no para mejorar lo que un cliente
ya cuidadoso hace bien.

## 6. Cómo verificarlo

Cada fila de `~/.config/oxidegate/telemetry.jsonl` trae:

- `cache_control_forced` (`bool`): `true` si OxideGate inyectó el breakpoint
  en ESE request. Nace en `Outgoing` (`provider/anthropic.rs::prepare`) y
  viaja sin tocar hasta `RequestMetric` (`MetricBase` → `MeteredBody::emit`,
  ver `src/middleware/proxy.rs` y `src/telemetry/metered.rs`).
- `cache_read_tokens` (`Option<u64>`): tokens que el proveedor sirvió desde
  caché en ESE request, tal como los reporta `cache_read_input_tokens`.

Correlacionando ambos campos se verifica el ciclo completo
**medir → optimizar → medir**:

1. Primer request de una sesión con `cache_control_forced: true`:
   `cache_read_tokens` debería ser `null` o bajo (nada cacheado todavía; ese
   request paga la escritura, ver `cache_write_tokens`).
2. Requests siguientes sobre el mismo prefijo, también con
   `cache_control_forced: true`: `cache_read_tokens` debería subir a niveles
   cercanos al tamaño del prefijo estable (`tools` + `system`), confirmando
   que el breakpoint inyectado efectivamente sirvió de caché.

Si `cache_control_forced` es siempre `false` con la palanca prendida, hay dos
explicaciones esperables (no un bug): el cliente ya trae su propio
`cache_control`, o el body no es JSON válido.

## 7. Por qué es Anthropic-only (por ahora)

- **OpenAI** cachea el prefijo de forma **automática** (sin ningún campo
  explícito que inyectar): no hay nada que forzar de este lado.
- **Gemini** gestiona su caché **aparte**, vía `cachedContentTokenCount`
  (implícita) o `cachedContent` explícito (un recurso separado, no un campo
  dentro del request): no encaja en el mismo patrón de mutación in-line.

Por eso `OpenAiChat`, `OpenAiResponses` y `Gemini` setean
`cache_control_forced: false` de forma fija en su `Outgoing` — la palanca
simplemente no aplica a esos dialectos hoy.
