# Exploración — Telemetría de cuota de Codex / suscripción de ChatGPT

> Fase: exploración (SDD). No es una propuesta ni una especificación: mapea el
> terreno, compara enfoques y deja las decisiones abiertas para la propuesta.

## Motivación

El tráfico por suscripción de ChatGPT (Codex, OAuth) no se factura por token,
sino como una cuota plana con ventanas de uso. Por eso `cost_estimate_usd` —un
cálculo de USD por token— es el instrumento equivocado para ese tráfico y
devuelve `null` con acierto. La señal de coste correcta es el **consumo de
cuota**, y el backend de Codex la expone en las cabeceras de la **respuesta**.

## Evidencia empírica (medida en el cable)

Una petición real de `gpt-5.5` contra `chatgpt.com/backend-api/codex/responses`
devolvió estas cabeceras de respuesta:

```
x-codex-plan-type: plus
x-codex-active-limit: premium
x-codex-primary-used-percent: 4
x-codex-secondary-used-percent: 0
x-codex-primary-window-minutes: 10080      (= 7 días, ventana semanal)
x-codex-secondary-window-minutes: 0
x-codex-primary-reset-after-seconds: 550037
x-codex-primary-reset-at: 1784489267       (unix ts)
x-codex-secondary-reset-at:
x-codex-credits-has-credits: False
x-codex-credits-balance:
x-codex-credits-unlimited: False
```

La petición se midió limpia: `200`, `input_tokens` 19.381, `output_tokens` 61,
`cache_read` 5.504 (tras el arreglo del extractor que lee los nombres de campo de
la Responses API, `input_tokens`/`output_tokens`).

Matiz semántico a validar: `used-percent` es un indicador **acumulado a nivel de
cuenta** en ese instante, NO un coste por petición. El coste marginal de UNA
petición en la moneda de la suscripción es cuánto **sube** `used-percent` entre
peticiones consecutivas (misma idea que la comparación de filas del marcador
`TRUNC` del monitor).

## Hallazgo central: la captura es limpia

En `src/middleware/proxy.rs::send_and_meter`, `resp.headers()` ya se lee de forma
síncrona (el bucle que copia las cabeceras del upstream a la respuesta saliente)
**antes** de que `resp.bytes_stream()` consuma `resp` para construir
`MeteredBody`. Las cabeceras `x-codex-*` se pueden capturar justo ahí, con cero
buffering y cero riesgo para el stream SSE; solo hay que hilarlas a `MetricBase`
(`src/telemetry/metered.rs`) y copiarlas a `RequestMetric` en
`MeteredBody::emit()`. La rama de error del upstream deja el campo nuevo en
`None` de forma natural, consistente con el principio de honestidad.

## Distinguir suscripción de API key

El mismo slug `gpt-5.5` puede llegar por el backend de Codex (suscripción,
medida por cuota) o por `api.openai.com` con API key (coste real por token). Hoy
`upstream` es `"openai"` en ambos casos (`Provider::name()` lo fija). Opciones:

1. **Presencia de cabeceras `x-codex-*`** en la respuesta — verdad del backend,
   señal implícita y por petición.
2. **`target_openai_url` (`OPENAI_API_BASE`)** contiene el host del backend de
   Codex — barato, disponible en `prepare()`, pero a nivel de instancia del
   proxy, no por petición.
3. **Cabecera de petición `originator: codex_exec`** — ya se reenvía intacta,
   pero es autoinforme del cliente, no autoritativo.

## Diseños candidatos (sin elegir ganador)

- **A — Mínimo**: capturar los `x-codex-*` crudos en
  `RequestMetric`/`RecentRequest`/espejo del monitor, sin tocar el TUI más allá
  de quizá columnas. El más seguro para el presupuesto de 400 líneas, puramente
  aditivo. Riesgo: ~11 campos crudos con la convención de documentación densa.
- **B — Eje completo**: un `Option<CodexQuota>` anidado (siguiendo el precedente
  no plano de `tools_by_server`) + una vista/indicador dedicada en el monitor +
  un `notional_api_cost_usd` claramente etiquetado que reutilice
  `pricing::estimate_cost_usd`. Más valor, claramente territorio multi-slice.
- **C — Modelo de delta marginal**: consumo de cuota por petición vía deltas de
  filas consecutivas. El mayor riesgo de honestidad: la cabecera capturada
  muestra `used-percent` como **entero** (4, 0), así que la mayoría de deltas de
  una sola petición redondean a 0 y podrían leerse como "gratis"; además necesita
  proteger el reinicio de ventana. No debería intentarse antes de que A entregue
  datos reales.

Recomendación planteada (no decidida): entregar A primero; diferir B/C a una
propuesta informada por datos reales capturados (refleja el patrón ya existente
en el proyecto de "documentado pero no observado", como `served_speed`).

## Preguntas abiertas para la propuesta

- ¿Campos planos o un struct anidado `CodexQuota`?
- ¿Qué señal de discriminación es la autoritativa (o se conservan varias)?
- ¿Aparecen las cabeceras `x-codex-*` en respuestas no-200 (p. ej. `429` al
  alcanzar el límite)? Sin verificar; cambia el valor de la feature de forma
  material si cubre el momento de agotamiento.
- ¿Entra en scope el coste nocional, dado que `gpt-5.5` aún no tiene entrada en
  `pricing.rs`?
- ¿Columna por fila o panel/indicador dedicado en el TUI?
- Regla explícita para valores de cabecera con string vacío → `None`.

## Riesgos

- La densidad de doc-comments podría acercar incluso el diseño A al presupuesto
  de 400 líneas de revisión.
- El riesgo de honestidad se concentra en el etiquetado del coste nocional
  (diseño B) y en la interpretación del delta marginal (diseño C).
- Comportamiento sin verificar de las cabeceras en respuestas de error.

## Archivos mapeados (ninguno modificado)

`src/middleware/proxy.rs`, `src/telemetry/metered.rs`, `src/telemetry/logger.rs`,
`src/telemetry/recent.rs`, `src/telemetry/stats.rs`, `src/telemetry/pricing.rs`,
`src/provider/openai.rs`, `src/provider/mod.rs`, `src/config.rs`,
`src/middleware/requests.rs`, `src/middleware/stats.rs`, `src/bin/monitor.rs`,
`docs/telemetry-level-1.md`, `README.md`.
