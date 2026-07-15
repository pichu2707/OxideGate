# Delta para atribución de sesiones — Rebanada 1: captura cruda en `/requests` + `telemetry.jsonl`

## Propósito

Sienta el contrato de datos de la rebanada 1 del eje de atribución de
sesiones: resolver una **clave de sesión** por precedencia de headers y
exponerla cruda, sin agregar ni derivar, en `GET /requests` y en cada línea
de `telemetry.jsonl`. Las rebanadas 2 (agregación en `/stats`) y 3 (panel en
el monitor TUI) quedan fuera de este contrato.

El nombre exacto del campo en el JSON de `/requests` y en `telemetry.jsonl`,
así como su representación interna, son decisiones de `sdd-design`. Este
documento fija la **semántica** del valor (qué se resuelve, con qué
precedencia, con qué saneo), no su identificador ni su tipo Rust.

## Fuera de alcance (rebanada 1)

- Agregación o group-by por clave de sesión en `/stats` (rebanada 2).
- Columna o panel de sesión en `oxidegate-monitor` (rebanada 3).
- Distinción de subagentes (`x-claude-code-agent-id`,
  `x-claude-code-parent-agent-id`), aunque la señal nativa exista.
- Rediseño de `client_of` o del `User-Agent` como eje independiente: el
  `User-Agent` se preserva intacto, solo se reutiliza como parte del
  fallback de esta rebanada.
- Capa de autenticación o multi-tenencia propia de OxideGate.

La ausencia de estos puntos en los requisitos siguientes es intencional.

## Precedencia de resolución de la clave de sesión

| Orden | Fuente | Condición de aplicación | Valor resultante |
|---|---|---|---|
| 1 | `X-OxideGate-Session` (header de request) | presente y no vacío | el valor crudo del header (etiqueta explícita asignada por el usuario) |
| 2 | `x-claude-code-session-id` (header de request) | precedencia 1 ausente o vacía; header presente y no vacío | el valor crudo del header (id de sesión nativo de Claude Code) |
| 3 | Fallback | ambas precedencias anteriores ausentes o vacías | el bucket **`"unattributed"`** llevando el `User-Agent` existente de la petición como valor; cuando el `User-Agent` falta o no es UTF-8 válido, el valor es la constante `"unattributed"` |

La resolución se realiza exclusivamente a partir de las **cabeceras del
request**, disponibles de forma síncrona antes de invocar al upstream. El
valor resultante nunca depende de la respuesta del upstream.

## ADDED Requirements

### Requirement: Resolución de la clave de sesión por precedencia

El sistema DEBE (MUST) resolver la clave de sesión evaluando las fuentes en
el orden fijado en la tabla de precedencia: `X-OxideGate-Session` primero,
`x-claude-code-session-id` segundo, fallback (bucket `"unattributed"` con el
`User-Agent` como valor) al final.

#### Scenario: X-OxideGate-Session presente y no vacío

- GIVEN una petición con el header `X-OxideGate-Session: claude-1`
- WHEN se resuelve la clave de sesión para esa petición
- THEN la clave resuelta es `claude-1`, independientemente de si
  `x-claude-code-session-id` también está presente

#### Scenario: X-OxideGate-Session ausente, x-claude-code-session-id presente

- GIVEN una petición de Claude Code sin el header `X-OxideGate-Session` pero
  con `x-claude-code-session-id` presente y no vacío
- WHEN se resuelve la clave de sesión para esa petición
- THEN la clave resuelta es el valor de `x-claude-code-session-id`

#### Scenario: Ambos headers ausentes

- GIVEN una petición sin `X-OxideGate-Session` y sin
  `x-claude-code-session-id`
- WHEN se resuelve la clave de sesión para esa petición
- THEN la clave resuelta es el bucket `"unattributed"` llevando el
  `User-Agent` existente como valor (la constante `"unattributed"` si no hay
  `User-Agent`), nunca una identidad inventada

### Requirement: Saneo de headers de atribución con string vacío

El sistema DEBE (MUST) tratar un header de atribución (`X-OxideGate-Session`
o `x-claude-code-session-id`) presente pero con valor de string vacío como
si estuviera ausente para efectos de precedencia: la resolución continúa
con la siguiente fuente de la tabla. El sistema NUNCA DEBE (MUST NEVER)
producir un valor equivalente a `Some("")` en ninguna etapa de la
resolución.

#### Scenario: X-OxideGate-Session presente pero vacío, x-claude-code-session-id presente

- GIVEN una petición con `X-OxideGate-Session: ""` (string vacío) y
  `x-claude-code-session-id` presente y no vacío
- WHEN se resuelve la clave de sesión para esa petición
- THEN la clave resuelta es el valor de `x-claude-code-session-id`, nunca el
  string vacío

#### Scenario: Ambos headers de atribución presentes pero vacíos

- GIVEN una petición con `X-OxideGate-Session: ""` y
  `x-claude-code-session-id: ""`
- WHEN se resuelve la clave de sesión para esa petición
- THEN la clave resuelta es el bucket `"unattributed"` con el `User-Agent`
  como valor, nunca un string vacío

### Requirement: Independencia de la resolución respecto al resultado del upstream

El sistema DEBE (MUST) resolver la clave de sesión únicamente a partir de
las cabeceras de la petición entrante, antes de invocar al upstream. La
rama de error del upstream (código distinto de 200, timeout, fallo de
conexión) NO DEBE (MUST NOT) alterar ni invalidar la clave ya resuelta: la
fila de `GET /requests` y la línea de `telemetry.jsonl` correspondientes a
esa petición DEBEN (MUST) llevar la clave resuelta de las cabeceras del
request, con el fallback honesto aplicándose de forma natural cuando no
había headers explícitos, nunca un valor derivado o inventado a partir del
fallo del upstream.

#### Scenario: Error de upstream sin headers explícitos de sesión

- GIVEN una petición sin `X-OxideGate-Session` ni `x-claude-code-session-id`
- WHEN el upstream responde con un código de error o la conexión falla
- THEN la fila de `GET /requests` y la línea de `telemetry.jsonl` llevan el
  fallback honesto (bucket `"unattributed"` con el `User-Agent` como valor)
  como clave de sesión

#### Scenario: Error de upstream con header explícito de sesión presente

- GIVEN una petición con `X-OxideGate-Session: claude-1`
- WHEN el upstream responde con un código de error o la conexión falla
- THEN la fila de `GET /requests` y la línea de `telemetry.jsonl` llevan
  `claude-1` como clave de sesión, sin degradar al fallback

### Requirement: Invariante de privacidad — la clave nunca es una credencial

El sistema NUNCA DEBE (MUST NEVER) incluir en la clave de sesión, ni en
ningún campo derivado de ella, contenido proveniente de headers de
credenciales (`Authorization`, API keys, tokens OAuth). La clave DEBE (MUST)
tratarse en todo momento como una etiqueta o identificador opaco.

#### Scenario: Petición con credencial y header de sesión simultáneos

- GIVEN una petición que incluye tanto un header de credencial
  (`Authorization` o API key) como `X-OxideGate-Session: claude-1`
- WHEN se resuelve la clave de sesión y se escribe la fila de
  `GET /requests` / la línea de `telemetry.jsonl`
- THEN la clave de sesión es `claude-1` y ningún campo de la fila o línea
  contiene el valor del header de credencial

### Requirement: Superficies de exposición de la clave de sesión en esta rebanada

El sistema DEBE (MUST) exponer la clave de sesión resuelta en la fila
correspondiente de `GET /requests` y en la línea correspondiente de
`telemetry.jsonl`, cruda y sin agregar. El sistema NO DEBE (MUST NOT)
exponer la clave de sesión en la agregación de `GET /stats` ni en el
monitor TUI (`oxidegate-monitor`) en esta rebanada.

#### Scenario: Clave de sesión visible en /requests

- GIVEN una petición cuya clave de sesión se resolvió como `claude-1`
- WHEN se consulta `GET /requests`
- THEN la fila correspondiente a esa petición lleva `claude-1` como clave de
  sesión

#### Scenario: Clave de sesión visible en telemetry.jsonl

- GIVEN una petición cuya clave de sesión se resolvió como `claude-1`
- WHEN se escribe la línea correspondiente en `telemetry.jsonl`
- THEN esa línea lleva `claude-1` como clave de sesión

#### Scenario: Clave de sesión ausente de /stats y del monitor TUI

- GIVEN peticiones con distintas claves de sesión ya capturadas en
  `/requests` y `telemetry.jsonl`
- WHEN se consulta `GET /stats` o se abre el monitor TUI
- THEN ninguna de las dos superficies agrupa, agrega ni muestra columna o
  panel por clave de sesión

### Requirement: No inferencia de identidad desde huellas implícitas

El sistema NO DEBE (MUST NOT) inferir ni derivar la clave de sesión a partir
de ninguna huella implícita: `User-Agent` por sí solo (fuera del rol de
fallback ya fijado), credenciales, `tools_by_server`/huella de MCPs
conectados, o `prompt_hash`. La identidad de sesión se asigna únicamente a
través de las dos fuentes de precedencia explícita (`X-OxideGate-Session`,
`x-claude-code-session-id`); toda otra situación resuelve al fallback.

#### Scenario: Dos peticiones concurrentes del mismo harness sin headers explícitos

- GIVEN dos peticiones concurrentes con el mismo `User-Agent`, el mismo
  conjunto de MCPs conectados y el mismo `prompt_hash`, ninguna con
  `X-OxideGate-Session` ni `x-claude-code-session-id`
- WHEN se resuelve la clave de sesión para cada una
- THEN ambas resuelven al mismo fallback (bucket `"unattributed"` con el
  `User-Agent` como valor); el sistema no inventa una distinción a partir de
  las huellas implícitas
