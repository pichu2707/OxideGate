# Delta para telemetría de cuota Codex — Rebanada 1: captura cruda en `/requests`

## Propósito

Capa 1 ("Estado") del eje de coste por cuota: capturar las doce cabeceras
`x-codex-*` de la respuesta del backend de Codex (OAuth, suscripción) y
exponerlas crudas, sin derivar, en `GET /requests`. Las capas 2 y 3, la
agregación en `/stats` y el indicador del monitor TUI son rebanadas
posteriores, fuera de este contrato.

## Fuera de alcance (rebanada 1)

- Agregación en `/stats` (rebanada 2).
- Vista/indicador en `oxidegate-monitor` (rebanada 3).
- Coste nocional a precios de API (rebanada 4).
- Atribución marginal por delta entre filas consecutivas (rebanada 5).

La ausencia de estos puntos en los requisitos siguientes es intencional.

## Campos capturados

| Cabecera | Campo | Tipo | Saneo |
|---|---|---|---|
| `x-codex-plan-type` | `plan_type` | string | vacío → `None` |
| `x-codex-active-limit` | `active_limit` | string | vacío → `None` |
| `x-codex-primary-used-percent` | `primary_used_percent` | numérico | vacío/no numérico → `None` |
| `x-codex-secondary-used-percent` | `secondary_used_percent` | numérico | ídem |
| `x-codex-primary-window-minutes` | `primary_window_minutes` | numérico | ídem |
| `x-codex-secondary-window-minutes` | `secondary_window_minutes` | numérico | ídem |
| `x-codex-primary-reset-after-seconds` | `primary_reset_after_seconds` | numérico | ídem |
| `x-codex-primary-reset-at` | `primary_reset_at` | numérico (unix ts) | ídem |
| `x-codex-secondary-reset-at` | `secondary_reset_at` | numérico (unix ts) | ídem — confirmado vacío en captura real |
| `x-codex-credits-has-credits` | `credits_has_credits` | booleano | solo `"True"`/`"False"` → bool; otro → `None` |
| `x-codex-credits-balance` | `credits_balance` | numérico | vacío/no numérico → `None` |
| `x-codex-credits-unlimited` | `credits_unlimited` | booleano | ídem que `has_credits` |

## ADDED Requirements

### Requirement: Captura de cabeceras de cuota en tráfico de suscripción

El sistema DEBE (MUST) parsear las doce cabeceras `x-codex-*` de la
respuesta del upstream y exponer los campos resultantes en la fila
correspondiente de `GET /requests`.

#### Scenario: Respuesta de Codex con las doce cabeceras presentes

- GIVEN una petición enrutada al backend de Codex vía OAuth
- WHEN la respuesta del upstream incluye las doce cabeceras `x-codex-*` con valores
- THEN la fila de `GET /requests` lleva los doce campos de cuota parseados según la tabla de campos capturados

### Requirement: Ausencia de cabeceras como señal discriminadora

El sistema DEBE (MUST) dejar todos los campos de cuota en `None` si la
respuesta no incluye ninguna cabecera `x-codex-*`. Su presencia es la
ÚNICA señal discriminadora; el sistema NO DEBE (MUST NOT) inferir cuota
desde `upstream`, el slug del modelo ni ninguna otra señal.

#### Scenario: Tráfico sin cabeceras de cuota

- GIVEN una petición a Anthropic, Gemini, o a OpenAI vía API key (`api.openai.com`, mismo slug que podría llegar por Codex, p. ej. `gpt-5.5`)
- WHEN la respuesta del upstream no incluye cabeceras `x-codex-*`
- THEN los doce campos de cuota en la fila de `GET /requests` son `None`

### Requirement: Saneo de valores de cabecera con string vacío

El sistema DEBE (MUST) mapear cualquier cabecera `x-codex-*` presente pero
con valor vacío a `None`, nunca a `Some("")` ni a un valor fabricado.

#### Scenario: x-codex-secondary-reset-at vacío

- GIVEN una respuesta de Codex donde `x-codex-secondary-reset-at` está presente pero vacía (confirmado en captura real)
- WHEN se parsea la fila de cuota
- THEN `secondary_reset_at` es `None`, no `Some("")` ni `Some(0)`

### Requirement: Parseo seguro de campos numéricos

El sistema DEBE (MUST) parsear a numérico las cabeceras marcadas como tal
en la tabla (porcentaje, minutos, segundos, timestamp, balance) cuando el
valor sea válido. Ausente, vacío o no numérico DEBE (MUST) resultar en
`None`; NUNCA DEBE (MUST NEVER) hacer panic ni fabricar un `0` u otro
valor por defecto.

#### Scenario: Valor numérico válido

- GIVEN una cabecera numérica con valor válido (p. ej. `x-codex-primary-used-percent: 4`)
- WHEN se parsea la fila de cuota
- THEN el campo correspondiente contiene el número parseado

#### Scenario: Cabecera numérica malformada o ausente

- GIVEN una cabecera numérica ausente, o presente con un valor no parseable como número
- WHEN se parsea la fila de cuota
- THEN el campo correspondiente es `None` y el proceso de captura no hace panic

### Requirement: Parseo de campos booleanos

El sistema DEBE (MUST) parsear `x-codex-credits-has-credits` y
`x-codex-credits-unlimited` como booleanos solo cuando el valor sea
exactamente `"True"` o `"False"` (capitalizado, confirmado en captura
real). Cualquier otro valor, incluido vacío, DEBE (MUST) mapear a `None`.

#### Scenario: Valores booleanos reconocidos

- GIVEN `x-codex-credits-has-credits: False` y `x-codex-credits-unlimited: False`
- WHEN se parsea la fila de cuota
- THEN ambos campos son `Some(false)`

#### Scenario: Valor booleano no reconocido

- GIVEN una cabecera booleana con valor distinto de `"True"`/`"False"` (minúsculas, `"1"`, vacío)
- WHEN se parsea la fila de cuota
- THEN el campo correspondiente es `None`

### Requirement: Comportamiento en la rama de error del upstream

El sistema DEBE (MUST) dejar en `None` los campos de cuota si el upstream
responde con código distinto de 200 y sin cabeceras `x-codex-*`. Si alguna
cabecera está presente en una respuesta no-200, DEBE (MUST) parsearla con
las mismas reglas que en 200, sin bifurcación especial. El comportamiento
real en respuestas de error (p. ej. `429`) queda declarado supuesto
abierto, no verificado, per la propuesta.

#### Scenario: Respuesta de error, con o sin cabeceras de cuota

- GIVEN una respuesta del upstream con código distinto de 200
- WHEN se parsea la fila de cuota
- THEN cada cabecera `x-codex-*` presente se parsea con las mismas reglas de esta especificación, y las ausentes quedan en `None`

### Requirement: Separación estricta entre cuota y cost_estimate_usd

El sistema NUNCA DEBE (MUST NEVER) mezclar, sumar ni derivar
`cost_estimate_usd` a partir de los campos de cuota, ni viceversa. Son dos
monedas independientes en la misma fila.

#### Scenario: Cuota presente, cost_estimate_usd ausente

- GIVEN una petición de suscripción con cabeceras `x-codex-*` presentes
- WHEN se construye la fila de `GET /requests`
- THEN los campos de cuota llevan valores parseados y `cost_estimate_usd` es `None`, sin influencia mutua

#### Scenario: cost_estimate_usd presente, cuota ausente

- GIVEN una petición con API key donde `cost_estimate_usd` tiene un valor calculado
- WHEN se construye la fila de `GET /requests`
- THEN los doce campos de cuota son `None`, sin influencia del valor de `cost_estimate_usd`
