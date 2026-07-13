# Propuesta — Eje de coste por cuota para tráfico de suscripción (Codex OAuth)

Esta propuesta añade a OxideGate un **eje de telemetría de cuota** para el
tráfico que llega por suscripción de ChatGPT (backend de Codex, OAuth). Ese
tráfico no se factura por token, sino como una cuota plana con ventanas de uso,
así que hoy su coste real es **invisible**: `cost_estimate_usd` calcula dólares
por token y devuelve `null` con acierto para estas peticiones. El eje de cuota
es una **moneda nueva y separada** (porcentaje de ventana consumido), que NUNCA
se mezcla con `cost_estimate_usd`. Se entrega de forma incremental: la primera
rebanada captura las cabeceras `x-codex-*` de la respuesta y las expone crudas
en `/requests`, sentando la base de datos reales sobre la que se diseñarán las
rebanadas siguientes.

## Por qué (el problema)

- El tráfico de suscripción tiene un coste real —consume cuota— pero ese coste
  no aparece en ninguna superficie de OxideGate porque no es dólares por token.
- El instrumento actual (`cost_estimate_usd`) es correcto para tráfico con API
  key, pero es el **instrumento equivocado** para la suscripción: devolver
  `null` es honesto, pero deja al usuario sin ninguna señal de consumo.
- El backend de Codex YA expone la verdad de la cuota en las cabeceras de la
  **respuesta** (`x-codex-*`). Hoy OxideGate mide solo cabeceras del *request*
  y el *body* de la respuesta; esa señal se descarta sin capturarse.

## Qué (la visión completa del eje)

Un eje de coste por cuota, **estrictamente separado** de `cost_estimate_usd`,
con tres capas y presente en dos superficies.

### Las tres capas

| Capa | Qué mide | Fuente | Naturaleza |
|------|----------|--------|-----------|
| 1. Estado | % consumido de cada ventana (semanal primaria + corta secundaria), momento de reinicio, tipo de plan | Verdad cruda del backend en las cabeceras `x-codex-*` de la respuesta | Acumulado a nivel de cuenta en ese instante |
| 2. Atribución | Consumo marginal de cuota por petición (delta de `used-percent` entre filas consecutivas de la misma cuenta y ventana) | Derivada de la capa 1 | Estimación con salvedad de honestidad explícita (ver Riesgos) |
| 3. Comparación nocional | "Cuánto costaría a precios públicos de API" | `pricing.rs` con precios reales de la familia GPT-5 | SIEMPRE etiquetada como estimación, NUNCA como dinero pagado |

### Las dos superficies

- **Endpoints JSON**: `/requests` (detalle por petición) y `/stats` (agregado).
- **Monitor TUI** (`oxidegate-monitor`): vista/indicador dedicado de cuota.

### Señal autoritativa (decisión ya fijada, no se reabre)

La **presencia de las cabeceras `x-codex-*`** en la respuesta es la señal
autoritativa **por petición** que distingue el tráfico de suscripción del
tráfico con API key. El mismo slug (p. ej. `gpt-5.5`) puede llegar por el
backend de Codex o por `api.openai.com`; en ambos casos `upstream` es
`"openai"` (`Provider::name()` lo fija), así que el upstream por sí solo no
discrimina. Las cabeceras de respuesta sí lo hacen, verificado en el cable.

## Alcance y rebanadas

Se entrega el **eje completo como meta**, pero de forma **incremental** en
rebanadas encadenadas, cada una ≤ 400 líneas cambiadas (presupuesto de
revisión), con PRs encadenados (estrategia de entrega: `ask-on-risk`). Cada
rebanada deja datos reales que informan el diseño de la siguiente.

### Rebanada 1 (esta entrega): captura cruda en `/requests`

**Objetivo mínimo**: capturar las cabeceras `x-codex-*` de la respuesta y
exponerlas crudas en `/requests`, sin derivar nada.

Mecánica de la captura (verificada contra el código):

- En `src/middleware/proxy.rs::send_and_meter`, `resp.headers()` ya se lee de
  forma síncrona (bucle que copia las cabeceras del upstream a la respuesta
  saliente) **antes** de que `resp.bytes_stream()` consuma `resp`. Las
  `x-codex-*` se capturan justo ahí: **cero buffering, cero riesgo para el
  stream SSE**.
- El campo nuevo se hila por `MetricBase` (`src/telemetry/metered.rs`) hasta
  `RequestMetric` (`src/telemetry/logger.rs`) y de ahí a `RecentRequest`
  (`src/telemetry/recent.rs`) para `/requests`.
- La rama de error de upstream deja el campo nuevo en `None` de forma natural,
  consistente con el principio de honestidad.
- Regla de saneo explícita: los valores de cabecera con **string vacío** (la
  ventana secundaria llega en blanco en esta cuenta) mapean a `None`, nunca a
  `Some("")`.

Cabeceras a capturar (medidas en el cable): `x-codex-plan-type`,
`x-codex-active-limit`, `x-codex-primary-used-percent`,
`x-codex-secondary-used-percent`, `x-codex-primary-window-minutes`,
`x-codex-secondary-window-minutes`, `x-codex-primary-reset-after-seconds`,
`x-codex-primary-reset-at`, `x-codex-secondary-reset-at`,
`x-codex-credits-has-credits`, `x-codex-credits-balance`,
`x-codex-credits-unlimited`.

**No-objetivos de la rebanada 1 (explícitos, diferidos a rebanadas posteriores):**

- Sin vista ni indicador en el monitor TUI.
- Sin coste nocional (comparación a precios de API).
- Sin atribución marginal (deltas por petición).
- Sin agregación en `/stats`.

Se difiere a propósito para que el diseño de las capas derivadas parta de datos
reales capturados, replicando el patrón ya establecido en el proyecto de
"documentado pero aún no observado" (p. ej. `served_speed`).

### Cadena de rebanadas siguientes (orden fijado)

1. **Rebanada 1** — captura cruda de `x-codex-*` en `/requests` *(esta entrega)*.
2. **Agregación en `/stats`** — estado de cuota agregado (última lectura por
   ventana, tipo de plan).
3. **Vista/indicador en el monitor TUI** — gauge de % de ventana consumido.
4. **Coste nocional** — comparación a precios de API; requiere añadir precios
   reales de la familia GPT-5 a `pricing.rs`.
5. **Atribución marginal** — consumo de cuota por petición vía deltas de filas
   consecutivas, con la salvedad de honestidad del redondeo de enteros.

## No-objetivos (de todo el cambio)

- **No** se reelabora el modelo de coste por token (`cost_estimate_usd`): la
  cuota es una moneda separada que jamás lo toca ni lo contamina.
- **No** hay MITM ni desencriptado: solo se leen cabeceras que el backend ya
  devuelve en claro a través del proxy.
- **No** se toca el pricing de API key de `api.openai.com`, salvo lo
  estrictamente necesario en la rebanada nocional (añadir precios GPT-5 a
  `pricing.rs`).

## Riesgos y supuestos

| Tema | Detalle | Tratamiento |
|------|---------|-------------|
| Cabeceras en respuestas no-200 | Sin verificar si `x-codex-*` aparecen en respuestas de error (p. ej. `429` al agotar el límite). Si aparecen, OxideGate podría avisar en el momento de agotamiento —valor materialmente mayor. | **Supuesto a validar en una rebanada temprana, NO un bloqueante.** Se declara abierto, no se resuelve aquí. |
| Salvedad de honestidad (redondeo de enteros) | `used-percent` llega como **entero** (valores `4`, `0`), así que el delta marginal de una sola petición suele redondear a `0`. NUNCA debe renderizarse como "gratis": es una limitación conocida que se declara con claridad, no se oculta. | Se documenta como limitación explícita; condiciona el diseño de la capa 2 (atribución marginal). |
| Densidad de doc-comments vs. presupuesto de 400 líneas | La convención de documentación total del proyecto (`//!` por módulo, `///` por campo) sobre 12 campos crudos puede acercar incluso la rebanada 1 al presupuesto de revisión. | Se vigila el conteo de líneas; si se acerca al tope, se prioriza la captura y se recorta documentación redundante manteniendo el contrato por campo. |

Principio transversal (no negociable): lo desconocido es `null`/`None`, nunca
un número inventado. La cuota es porcentaje de ventana, jamás dólares.

## Criterios de éxito (rebanada 1)

- Una petición real de `gpt-5.5` a través del backend de Codex produce una fila
  en `/requests` que lleva los campos de cuota parseados desde las cabeceras
  `x-codex-*`.
- El tráfico con API key, Anthropic y Gemini lleva esos campos en `None` (la
  ausencia de cabeceras `x-codex-*` es la señal autoritativa).
- Las cabeceras con string vacío se mapean a `None`, no a `Some("")`.

## Siguiente paso

Con la propuesta aprobada, avanzan en paralelo `sdd-spec` (contrato de datos de
la rebanada 1: forma y saneo de los campos capturados) y `sdd-design`
(arquitectura del hilado desde `send_and_meter` hasta `/requests`, y decisión
de campos planos vs. struct anidado `CodexQuota`).
