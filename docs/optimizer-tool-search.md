# Optimizador · Palanca C — carga diferida de tools MCP (`tool search`)

> Estado: **no implementada, y reencuadrada**. La primera versión de este doc
> prometía **bytes**. Medido en el cable con grupo de control, la palanca
> ahorra **tokens de contexto** — y OxideGate mide bytes. Esa corrección
> gobierna todo lo que sigue.
> Anthropic-only por construcción (ver §8).

---

## 0. Resumen

Los esquemas de tools MCP viajan **enteros en cada request**. Con 4 servidores
MCP conectados eso son decenas de kB re-enviados turno tras turno, se usen o no.

Esta palanca hace que OxideGate reescriba el body saliente para marcar esos
esquemas con `defer_loading: true` y declarar la tool de búsqueda de Anthropic.
El modelo recibe solo los nombres en su contexto, y pide el esquema completo del
servidor que necesita, cuando lo necesita.

Lo crítico, y lo que la primera versión de este doc dijo mal:

- **Lo que se ahorra es contexto, no cable.** `tool_search_tool_bm25` es una
  **server-side tool**: busca sobre las tools **declaradas en el request**. Para
  que el servidor las encuentre, **tienen que estar en el body**. El proxy puede
  *marcarlas*; no puede *retenerlas*. Los bytes siguen viajando.
- **La caché sobrevive.** La API **añade** los esquemas descubiertos al final del
  prompt, **no reescribe** el array `tools`. Eso es lo que mata al diseño ingenuo
  (§1) y deja viva a esta palanca.

**Consecuencia inmediata:** el instrumento principal de OxideGate (`n_bytes`) es
**ciego a esta palanca** — peor: medido, marcar una tool **suma 21 bytes** al
cable (§2.3). La métrica de éxito es `input_tokens`, nunca `n_bytes`. Un
experimento que espere ver bajar los bytes va a concluir, erróneamente, que la
palanca no funciona; y uno que vea subir `n_bytes` va a tomar por anomalía lo que
es el comportamiento correcto (§7).

---

## 1. El diseño ingenuo, y por qué está muerto

La primera idea, la obvia, es tratar el proxy como una válvula: **tachar** los
esquemas MCP de `body.tools`, y **volver a dejarlos pasar** cuando el modelo los
necesite.

No hay que construirlo. Este repo **ya midió por qué**.

El orden de render de Anthropic es `tools → system → messages`
(`docs/optimizer-prompt-cache.md` §8). El array `tools` ocupa la **posición 0**
del prefijo. La caché es un *prefix match*: **cualquier byte que cambie en el
prefijo invalida todo lo que viene detrás.**

Tocar `tools` no invalida "el bloque de tools". Invalida **el system prompt y la
conversación entera**. Reconstrucción total.

La tabla oficial de invalidación:

| Cambio | Caché de tools | Caché de system | Caché de messages |
|---|:---:|:---:|:---:|
| Definiciones de tools (añadir / quitar / reordenar) | ❌ | ❌ | ❌ |
| Cambio de modelo | ❌ | ❌ | ❌ |
| Contenido del system prompt | ✅ | ❌ | ❌ |
| Contenido de los mensajes | ✅ | ✅ | ❌ |

Y la medición propia (`docs/findings.md` §E):

> `cache_read` de 54.247, luego **0** con `cache_write` de **76.356**, y de
> vuelta 54.247 al restaurar la configuración anterior.

Documentación y cable coinciden. La economía remata el caso:

- **cache read: ≈0,1×** la tarifa de input.
- **cache write: 1,25×** (TTL 5 min) / **2×** (TTL 1 h).

Una válvula que destapa un servidor en el turno 20 **cambia una lectura barata
del bloque de tools (0,1×) por una reescritura cara del prefijo completo de la
conversación (1,25×)**. Un solo destape a mitad de sesión puede costar más que
decenas de peticiones de tools cacheados. **El ahorro en bytes queda enterrado
bajo el coste de los cache-miss.**

Conclusión: cualquier palanca que **mute el array `tools` a mitad de sesión** es
una trampa. La caché tiene que sobrevivir al descubrimiento, o no hay palanca.

---

## 2. El mecanismo que sí sirve: `defer_loading` + `tool search`

La API de Anthropic tiene un primitivo nativo para esto, y su propiedad decisiva
está documentada así:

> *"Tool definitions are **appended, not swapped** — preserves cache."*

**Añadidos, no intercambiados.** Los esquemas descubiertos se agregan **al
final**, sin tocar la cabeza del prefijo. La caché sobrevive.

Son dos piezas en el body, y **hacen falta las dos**:

1. `defer_loading: true` en cada definición de tool que se quiere diferir.
2. Declarar la tool de búsqueda en el array `tools`:

```json
{"type": "tool_search_tool_bm25_20251119", "name": "tool_search_tool_bm25"}
```

(variante alternativa: `tool_search_tool_regex_20251119` /
`tool_search_tool_regex`).

**Declarar la tool de búsqueda sin marcar ninguna tool con `defer_loading` no es
un error: es un no-op.** No hay nada diferido. Cualquier detector que trate una
sola de las dos señales como "el cliente difiere" **miente** — ver §6, regla 5.

### 2.1. Por qué esto se puede hacer desde el proxy, y una válvula no

`tool_search` es una **server-side tool**: corre en la infraestructura de
Anthropic. El `tool_use` de descubrimiento **no vuelve nunca al harness**.

Esto es lo que desbloquea todo. Un proxy puede **quitar** cosas del body, pero no
puede **servir** una tool de descubrimiento: ese `tool_use` volvería al cliente,
que no la tiene registrada, y fallaría. Al proxy le falta la mitad del bucle.

Con `tool_search` esa mitad **no hace falta**: la pone el servidor de Anthropic.
OxideGate solo tiene que **reescribir el body de salida**. No necesita plugin en
el harness, ni interceptar la respuesta, ni mantener estado de sesión.

**La palanca completa es una mutación del body saliente. Nada más.**

### 2.2. Y por eso mismo los bytes NO se van

La misma propiedad que hace posible la palanca le pone el techo.

`tool_search` corre en el servidor y **busca sobre las tools declaradas en el
request**. Para que el servidor las encuentre y las inyecte a demanda, **las
definiciones tienen que estar en el body**. `defer_loading: true` es un flag
*sobre la definición*, no un reemplazo de la definición.

El proxy marca los esquemas, los esquemas **viajan igual**, y lo que se evita es
que ocupen **contexto y tokens** por adelantado.

**La Palanca C ahorra tokens. No ahorra bytes.**

### 2.3. Medido en el cable, con precisión de byte

Durante buena parte de la vida de este doc, lo de arriba fue una **derivación**
de la documentación de Anthropic, marcada explícitamente como *no medida*. Era la
afirmación más frágil del proyecto, y el propio doc prohibía declarar la palanca
viva sin cerrarla.

**Está cerrada.** El experimento del servidor sonda (§3.1), forzando al modelo a
usar la tool retenida, produce el mismo esquema en las dos ramas:

| | `deferred_tools` | bytes de la sonda en el cable |
|---|---:|---:|
| `ENABLE_TOOL_SEARCH=1` (aparece marcada) | **1 de 1** | **2.219 B** |
| control (aparece sin marcar) | **0 de 1** | **2.198 B** |

La resta es la prueba:

```
2.219 − 2.198 = 21 bytes
len(',"defer_loading":true') = 21
```

**Marcar una tool con `defer_loading` cuesta 21 bytes y quita CERO.** El esquema
viaja completo, marcado o no. El delta es exactamente la longitud de la clave que
se agregó — ni un byte más, ni uno menos.

No es una estimación ni una lectura de la documentación: es una resta de dos
mediciones que difieren en la única cosa que las distingue.

**La Palanca C ahorra tokens de contexto. En el cable, CUESTA 21 bytes por tool
marcada.** Esa es la afirmación completa, y ahora tiene número.

### 2.4. Corrección de récord: dos conclusiones anteriores eran falsas

Este doc afirmó, en distintos momentos, dos cosas que la medición desmintió. Se
dejan escritas en vez de borrarlas, porque el error tiene más valor didáctico que
la conclusión:

- **"Las tools nativas siguen presentes, solo que más livianas."** Falso. La
  CUENTA cae de 29 a 11: 18 tools nativas están tan **ausentes** como las MCP.
  Los bytes *por tool* incluso **suben** (3.567 → 4.998 B). No se achicaron: se
  fueron. Es el mismo mecanismo de retención, aplicado por igual a nativas y MCP.

- **"Claude Code nunca usa `defer_loading`."** Falso, y el error es
  instructivo: salió de leer `deferred_tools = 0` en filas grabadas **antes de
  que ese campo existiera**, con un `.get(campo, 0)` que fabricó el cero. Un
  **ausente leído como cero** — exactamente el defecto que este proyecto persiguió
  durante siete rondas de revisión, cometido en el análisis que iba a corregirlo.
  Claude Code **sí** usa `defer_loading`: marca las tools de los servidores que
  saca a la luz bajo demanda (§3.1).

La regla que queda, y que gobierna todo consumidor de esta telemetría:
**AUSENTE nunca es CERO.**

---

## 3. La ironía, ahora medida con grupo de control y servidor sonda

Claude Code **ya difiere sus tools por defecto** (`MCP tool search`, controlado
por `ENABLE_TOOL_SEARCH`). Pero la documentación oficial dice que ese default:

> *"falls back to upfront loading on GCP Agent Platform or **non-first-party
> `ANTHROPIC_BASE_URL`**"*

**OxideGate es, por definición, un `ANTHROPIC_BASE_URL` no-first-party.**

En el momento en que se enruta Claude Code a través de OxideGate para medir el
impuesto de contexto, Claude Code **deja de diferir y carga todo de golpe** —
creando el impuesto que se venía a medir. Efecto observador puro: el instrumento
produce el fenómeno.

Hasta aquí era una cita. Ahora es un experimento.

### 3.1. El A/B con servidor sonda — de "ausente" a "retenido", con prueba

Todas las mediciones anteriores de este documento tenían un hueco: "0 tools /
0 B de MCP en el cable" es una observación **ambigua**. Es indistinguible de
"el usuario no tenía ningún servidor MCP configurado". Una medición anterior
de este mismo tráfico descartaba ese confundidor apoyándose en un servidor MCP
**preexistente** del usuario (`plugin_engram_engram`, presente en el control) —
mejor que nada, pero no es un instrumento diseñado para la pregunta: depende
de que el usuario tuviera *algo* configurado de antemano.

Este experimento sí lo es: mismo `claude -p`, mismo modelo, mismo proxy,
**única variable `ENABLE_TOOL_SEARCH`**, con un servidor MCP **sonda,
construido a propósito** para este experimento y registrado vía `claude mcp
add`. `claude mcp list` confirmó que la sonda se conectó **en los dos
brazos**. Eso convierte su existencia en **ground truth**: si sus bytes no
llegan, no es porque el usuario no tenía nada configurado — es porque el
harness los retuvo, y hay una conexión confirmada que lo prueba.

| | `ENABLE_TOOL_SEARCH=1` | control (default) |
|---|---:|---:|
| Tools nativas en el cable | **11 tools / 54.981 B** | **29 tools / 103.439 B** |
| Servidores MCP en el cable | **0** | **2+ (incl. la sonda, 2.198 B)** |
| Servidor sonda | **AUSENTE** | **PRESENTE** |
| `deferred_tools` (nativas) | **1** | **0** |
| `client_defer_loading` (AND de las dos señales) | **false** | **false** |

**Tres conclusiones medidas, no inferidas:**

1. **Claude Code RETIENE los esquemas MCP por completo — probado, no
   inferido.** La sonda estaba demostrablemente configurada y conectada, y sus
   bytes no llegaron. También retiene 18 de sus 29 tools nativas: la cuenta
   cae, no solo el peso.
2. **Claude Code nunca declara una `tool_search_tool_*` en el body.** Prueba
   aritmética, no interpretación: `deferred_tools (nativas) = 1` (alguna tool
   SÍ está marcada), y sin embargo `client_defer_loading` — el AND de "alguna
   tool diferida" Y "tool de búsqueda declarada" — da `false`. Si el primer
   término del AND es verdadero y el resultado es falso, el segundo término
   tiene que ser falso: la tool de búsqueda no está.
3. **Exactamente UNA tool nativa trae `defer_loading: true` en el brazo con
   diferido activo.** Esto está **sin explicar**. No se teoriza aquí por qué:
   se deja como hecho medido y abierto. Una futura ejecución que aísle esa tool
   específica podría explicarlo; hasta entonces, este documento prefiere una
   pregunta abierta honesta a una historia plausible sin evidencia.

### 3.1.4. La retención es PEREZOSA — y el esquema aparece marcado

El experimento anterior mide la **ausencia**. Este mide qué pasa cuando el
servidor retenido **hace falta**: mismo `claude -p`, misma sonda, pero con un
prompt que **obliga** al modelo a invocar `probe_marker`.

| | petición #1 | petición #2 | petición #3 |
|---|---|---|---|
| `ENABLE_TOOL_SEARCH=1` | sonda **ausente** | **1 tool, 2.219 B, `deferred_tools=1`** | ídem |
| control | **1 tool, 2.198 B, `deferred_tools=0`** | ídem | — |

**Tres hechos más:**

1. **La retención es perezosa, no permanente.** La sonda no está en la petición
   #1 y **aparece en la #2**, exactamente cuando la tarea la necesita. Los demás
   servidores MCP siguen ausentes: sale a la luz **solo el que hace falta**.
2. **Lo que sale a la luz, sale MARCADO.** `deferred_tools = 1 de 1` en el brazo
   con diferido; `0 de 1` en el control. Claude Code **sí** usa `defer_loading`
   — sobre los servidores que decide surfacear.
3. **Y sus bytes viajan igual: 2.219 B.** De ahí sale la resta de 21 bytes que
   cierra el §2.3.

**Distinguir retención de latencia de conexión.** Un servidor puede faltar en el
cable por dos razones muy distintas, y este experimento las separa: un servidor
que **aparece cuando se necesita** estaba **retenido**; uno que aparece **solo
con el paso de los turnos** (los conectores HTTP remotos tardan en conectar)
estaba **conectando**. Medido: en una ejecución de control, `claude_ai_*` no está
en la petición #1 y sí en la #3, siete segundos después, sin que nadie los pida.
**Ausencia no implica retención**, y ningún consumidor de esta telemetría puede
afirmar la causa de una ausencia desde una sola petición.

### 3.1.5. Tensión abierta

La conclusión 2 de §3.1 (*"nunca declara una `tool_search_tool_*`"*) **no encaja
del todo** con §3.1.4: si las tools de la sonda llegan **todas** marcadas con
`defer_loading` y no hay tool de búsqueda declarada, el modelo no debería poder
descubrirlas — y sin embargo **la invocó**.

Las dos observaciones son medidas y ninguna se descarta. La tensión queda
**anotada, no resuelta**: la prueba aritmética de la conclusión 2 se apoya en un
campo (`client_defer_loading`) que después se eliminó, así que no puede
re-verificarse sin volver a instrumentar. Antes de construir nada que dependa de
"Claude Code no declara la tool de búsqueda", **hay que volver a medirlo.**

**El confundidor queda descartado por diseño, no por argumento.** No hace
falta apelar a "el control prueba que sí carga MCP porque trae
`plugin_engram_engram`" (válido, pero indirecto): la sonda fue configurada
para ESTE experimento, se confirmó conectada con `claude mcp list` en ambos
brazos, y aun así está ausente del cable en el brazo con diferido. Es la
prueba más fuerte que tiene este proyecto sobre la retención de Claude Code.

#### 3.1.1. Corrección: las tools nativas no son "más livianas" — son menos

Una versión anterior de este documento (§2.2) afirmaba que las tools nativas
*"siguen presentes en el body, solo que más livianas"* cuando el diferido está
activo. **Es falso**, y esta medición lo corrige con números concretos:

- La CANTIDAD cae: 29 → 11 tools nativas. No es la misma cuenta con menos
  peso cada una.
- Bytes por tool **sube**, no baja: 103.439 / 29 ≈ **3.567 B/tool** en control,
  frente a 54.981 / 11 ≈ **4.998 B/tool** con diferido activo.

Si las 29 tools siguieran presentes "marcadas pero más livianas", el
bytes-por-tool tendría que bajar o quedar igual. Sube, porque lo que queda es
un subconjunto de las tools más pesadas — las 18 que faltan simplemente **no
están**, mismo mecanismo de retención que ya se probó para los servidores MCP
(conclusión 1), no un mecanismo distinto de marcado más liviano.

#### 3.1.2. Lo que esto implica para §2

El primitivo `defer_loading` + `tool_search` que describe §2 es de la **API**
de Anthropic, y sigue siendo válido tal cual está documentado ahí — pero esta
medición confirma que **Claude Code no lo está usando** para su propio
diferido de tools (conclusión 2: nunca declara la tool de búsqueda). El
diferido de Claude Code es un mecanismo enteramente distinto, propio del
harness, que decide qué mandar en el body ANTES de que salga — no una
instancia del primitivo de §2 corriendo del lado del cliente. La única marca
`defer_loading` medida (conclusión 3) es un dato aislado, no evidencia de que
el mecanismo de §2 esté en juego aquí.

### 3.2. Los dos mecanismos NO son el mismo

Esta es la corrección que reordena el doc entero. Se llaman igual y hacen cosas
distintas:

| | Dónde corre | Qué hace con los esquemas | ¿Ahorra bytes? |
|---|---|---|:---:|
| **Diferido de Claude Code** (`ENABLE_TOOL_SEARCH`) | En el **harness** | **No los manda** | **Sí** |
| **Palanca C** (`defer_loading` + `tool_search`) | En la **API** | Los manda **marcados** | **No** |

Claude Code **retiene**. La palanca **marca**.

El experimento con sonda (§3.1) agrega una precisión a esta tabla: Claude Code
no solo retiene por un mecanismo DISTINTO al de la fila de abajo — nunca
siquiera ACTIVA el primitivo de la fila de abajo (no declara la tool de
búsqueda). Son dos filas de la tabla porque son dos cosas que no se tocan, no
porque Claude Code use una versión reducida de la segunda.

Por eso: **OxideGate rompe el diferido por existir en el camino, y no puede
devolverlo igual.** Puede devolver el ahorro de **contexto**; no puede devolver
el ahorro de **cable**. El que rompió el vidrio pone uno parecido, en otra
dimensión.

Esto **no mata la palanca** — el ahorro de tokens es real y es dinero. Mata la
forma en que este doc la vendía.

### 3.3. La deuda de honestidad del reporte

El ahorro que `oxidegate-savings` canta bajo Claude Code (*"ahorrarías X
desconectando N servidores MCP"*) es, en ese harness, **un artefacto de la
presencia del proxy** — ahora confirmado con control.

El número no está mal; **la línea base sí**. Desconectar los MCP con el proxy
puesto sí baja los bytes — pero **quitar el proxy los baja igual, y encima
conservas los servidores**. Un usuario podría amputar capacidades reales para
"ahorrar" bytes que introdujo el medidor.

Lo mismo vale para las filas `native`: la lente afirma que *"no se quitan
desconectando nada"*. Bajo Claude Code, **≈48 kB de esos bytes nativos también
son artefacto del proxy**.

> Nota: en harnesses con **carga eager real** (OpenCode y la mayoría de los ~18
> auditados en `oxidegate-lens/docs/COMPATIBILIDAD-HARNESSES.md`), el impuesto
> **no** es artefacto: existe con proxy o sin él. Ahí el reporte de ahorro es
> honesto tal cual está.
>
> Ese doc **decía lo contrario** hasta este cambio: su matriz cerraba el caso de
> Claude Code con *"Ya resuelto — nada que mostrar"*. Corregido junto con esta
> medición — su fila ahora dice que Claude Code difiere por defecto **pero cae a
> eager detrás de un `ANTHROPIC_BASE_URL` no-first-party**.

### 3.4. Lo que NO es decidible solo con datos de cable

El experimento con sonda (§3.1) prueba retención en UN caso: el servidor que
este proyecto configuró a propósito para medir. Eso demuestra el mecanismo,
pero no lo generaliza a cualquier request que OxideGate vea en producción.

**OxideGate, mirando solo el body de un request, no puede saber cuántos
servidores MCP tiene configurados el cliente.** Ve lo que llegó al cable —
`tools_by_server` con `tools == 0` para un servidor, o directamente ningún
servidor MCP en el body — y esa observación es estructuralmente ambigua entre
dos mundos:

- el cliente tiene el servidor conectado y el harness lo retuvo (lo que probó
  la sonda), o
- el cliente nunca tuvo ese servidor configurado.

Ambos mundos producen el **mismo body saliente**. Ningún campo de
`RequestMetric` ni de `GET /requests` puede distinguirlos por sí solo, porque
ninguno de los dos tiene acceso a la configuración del cliente — solo al
tráfico que ese cliente decidió mandar.

**Por eso la sonda fue necesaria para PROBAR retención, y por eso ese mismo
truco no escala a tráfico real.** No se le puede pedir a cada usuario que
registre un servidor sonda para que OxideGate sepa si le están reteniendo
tools. Cerrar esta brecha en tráfico real necesita una fuente de verdad
adicional que sí conozca la configuración del cliente — algo que corra en la
misma máquina que el harness, no algo que solo vea bytes en tránsito.

**Esa es la resolución arquitectónica: `oxidegate-lens`.** Corre localmente,
puede leer la configuración de MCP del cliente (los servidores que el usuario
efectivamente registró) y compararla contra lo que `tools_by_server` reporta
que llegó al cable. Declarado-vs-llegado es exactamente la comparación que el
proxy no puede hacer por sí solo — el proxy mide el cable; `oxidegate-lens`
conoce el otro lado. Esta sección documenta el límite; no lo resuelve aquí.

---

## 4. Dónde vive

Misma puerta que la Palanca A: `Anthropic::prepare` en
`src/provider/anthropic.rs`, junto a `force_cache_control`. Es el único punto
donde el proyecto ya muta el body saliente de Anthropic, y ya tiene el patrón
resuelto (detectar → decidir → mutar → registrar en telemetría).

Coherencia con el contrato del proyecto: **OxideGate es ante todo un medidor
transparente**; no muta requests salvo excepciones explícitas, detrás de un flag
y apagadas por defecto (`docs/optimizer-prompt-cache.md` §3). Esta palanca es una
mutación real del body, así que nace igual: **apagada**.

Variable propuesta, siguiendo la nomenclatura de `OXIDEGATE_FORCE_CACHE`:

```bash
export OXIDEGATE_DEFER_MCP_TOOLS=true   # default: false (apagado)
```

---

## 5. Lo que ya existe: `client` (y por qué `client_defer_loading` se eliminó)

Campo de **observación pura** (sin mutar nada), ya en `RequestMetric` y
servido por `GET /requests`:

- **`client`** — el `User-Agent` del request entrante. Claude Code se identifica
  como `claude-cli/2.1.207 (external, sdk-cli)`. Antes de esto la telemetría no
  registraba **nada** del cliente: "ese tráfico era Claude Code" era una
  inferencia por los nombres de los servidores MCP, no un hecho. Ahora es un
  campo. **No verificable**: es contenido que manda el cliente, no algo que el
  proxy calcule (`docs/telemetry-per-request.md` §4.3) — se documenta
  honestamente como tal, no como una garantía.

**`client_defer_loading` existió y se eliminó.** Era el booleano body-wide de
las dos señales (regla 2, más abajo): `true` solo si ALGUNA tool traía
`defer_loading: true` Y el body declaraba una `tool_search_tool_*`. El
experimento con sonda (§3.1) lo midió en **`false` en los DOS brazos** del A/B
— tratado y control. Un campo que da el mismo valor sin importar la variable
que se está probando no discrimina nada: es peso muerto. Peor, cada consumidor
que alguna vez razonó a partir de él (§2.2, la lente en §9) produjo una
afirmación falsa, porque asumía implícitamente que podía dar `true` en tráfico
real de Claude Code. Nunca lo hizo. Se eliminó del proxy entero (`Outgoing`,
`RequestMetric`, `RecentRequest`, el TUI) en vez de dejarlo vivo con esta nota
al lado — un campo que miente por diseño no se arregla con un comentario.

`ToolServerBytes::deferred_tools` (por servidor, §6 regla 4) es distinto y
**se conserva**: es una observación real, per-server, de si una definición
concreta trae la marca `defer_loading`. Su dominio es **tokens de contexto**,
nunca bytes de cable — ver la guardia en su propio doc-comment
(`src/provider/mod.rs`) y en `docs/telemetry-per-request.md` §4.2.

---

## 6. Reglas duras (violarlas es un 400, o un no-op silencioso)

1. **Nunca diferir todo.** La tool de búsqueda **no** puede llevar
   `defer_loading`, y **al menos una tool debe quedar sin diferir**, o la API
   responde `400: All tools have defer_loading set`.
   → Las tools **nativas** del harness sirven justo para eso: no se pueden quitar
   de todos modos (`--strict-mcp-config` no las toca), así que son el ancla
   natural de tools no-diferidas. **Diferir solo las de `kind == "mcp"`.**

2. **No pisar a un cliente que ya difiere.** Mismo principio que la regla de la
   Palanca A (§4 de su doc): si el body entrante **ya trae** diferido, **no tocar
   nada**. Contra Claude Code con `ENABLE_TOOL_SEARCH` activo esto es un no-op —
   igual que la Palanca A es un no-op contra su `cache_control` propio.

3. **El modelo tiene que soportar bloques `tool_reference`.** Los Haiku **no**.
   Diferir contra un modelo que no lo soporta es un fallo, no un ahorro: detectar
   el modelo antes de mutar.

4. **Identificar qué tool es MCP.** El proxy ya sabe distinguirlas: el desglose
   `tools_by_server` con `kind` (`mcp` / `native`) que consume `oxidegate-lens`
   sale de aquí. Reutilizar esa clasificación, no inventar otra.

5. **Las dos señales, no una.** `defer_loading: true` en alguna tool **Y** la
   tool de búsqueda declarada. Un `OR` entre las dos produce falsos positivos: un
   body que declara `tool_search_tool_bm25` sin marcar ninguna tool **no difiere
   nada**, y afirmar que sí hace que el reporte **sub-estime un impuesto real**.
   Es la misma clase de mentira que esta palanca vino a corregir, reubicada.

---

## 7. Cómo se verifica — y por qué `n_bytes` NO sirve

El ciclo **medir → optimizar → medir** aplica igual. Pero la métrica cambia, y
este es el punto más importante del doc.

**`n_bytes` no va a bajar. Por diseño (§2.2).** Un experimento que espere ver caer
los bytes va a concluir, erróneamente, que la palanca no funciona.

Campos que ya existen en `~/.config/oxidegate/telemetry.jsonl`:

- `input_tokens` ← **la métrica de esta palanca**
- `cache_read_tokens` (`cache_read_input_tokens`)
- `cache_write_tokens` (`cache_creation_input_tokens`)
- `n_bytes` / `n` (bytes de tools por servidor) ← **debe SUBIR 21 B por tool
  marcada** (§2.3). No bajar. No quedarse igual. **Subir.**

Campo nuevo a emitir, análogo a `cache_control_forced`:

- `mcp_tools_deferred` (`bool`): `true` si OxideGate marcó tools MCP con
  `defer_loading` en ESE request.

**Las dos afirmaciones que sostienen la palanca:**

> **(a)** Con `mcp_tools_deferred = true`, `input_tokens` **baja** de forma
> proporcional a los esquemas MCP diferidos, mientras `n_bytes` **sube exactamente
> 21 B por cada tool marcada** (§2.3) — el coste de la clave `defer_loading`.
>
> **(b)** Cuando el modelo descubre y carga un servidor MCP a mitad de sesión, la
> caché **sobrevive**: `cache_read_tokens` se mantiene alto y `cache_write_tokens`
> **no** salta al tamaño del prefijo completo.

(b) es la diferencia exacta entre esta palanca y la válvula muerta de §1 — donde
`cache_read` cayó a 0 y `cache_write` saltó a 76.356. **Si en el evento de
descubrimiento se reproduce ese patrón, la palanca está muerta también y hay que
matarla, documentando por qué.**

Si (a) falla — `input_tokens` no se mueve — la API está ignorando el campo en
silencio, y la palanca es un no-op caro. Mismo precedente que la §6.2 de la
Palanca A.

### 7.1. Deuda gemela: el no-op silencioso

Si `mcp_tools_deferred == true` pero `input_tokens` no baja, la fila **afirma una
intervención cuyo efecto nunca ocurrió**. Candidato a marcador de outlier en el
monitor TUI, al lado de `TRUNC` y del `NOCACHE` pendiente.

**Ojo con el error inverso**: marcar la fila como sospechosa porque `n_bytes` no
bajó sería un falso positivo. `n_bytes` **no baja nunca** con esta palanca.

---

## 8. Por qué es Anthropic-only

`defer_loading` + `tool_search` son un primitivo **de la API de Anthropic**.
Disponibilidad por plataforma: primera parte ✅, Claude Platform on AWS ✅,
Bedrock ✅ (solo InvokeModel, no Converse), Vertex ✅, Foundry β.

**No existe equivalente en los dialectos OpenAI ni Gemini.** Igual que la Palanca
A, `OpenAiChat`, `OpenAiResponses` y `Gemini` deben emitir
`mcp_tools_deferred: false` de forma fija: la palanca no aplica a esos upstreams.

Esto la convierte en una capacidad **condicional al upstream**, no universal. Es
una limitación honesta, no un defecto.

---

## 9. Relación con `oxidegate-lens`

`oxidegate-lens` **no cambia de naturaleza**: sigue siendo capa de presentación
read-only, y esta palanca vive **entera** en OxideGate.

Pero el reporte de ahorro cambia dos veces:

- **Hoy**, con `client` ya en `GET /requests` y el experimento de §3.1
  probando la retención de Claude Code fuera de toda duda, la lente puede dejar
  de mentir: cuando el cliente es un harness que difiere, esos bytes de
  contexto **son artefacto del proxy** y no se deben cantar como ahorro por
  desconectar servidores (§3.3).
- **Con la palanca activa**, la lente **no** verá bajar los bytes (§2.2). Lo que
  puede reportar es el delta de **tokens** entre lo que el config declara y lo que
  realmente ocupó contexto. Eso ya no es una hipótesis: es un **ahorro realizado**,
  sin que nadie desconecte nada — pero es un ahorro **en tokens**, y hay que
  decirlo con esas palabras.

**Guardia contra un defecto ya encontrado una vez:** este proyecto tuvo un
booleano body-wide (`client_defer_loading`, `.any()` sobre TODAS las tools del
request) que un consumidor podía leer como "todos los servidores de esta fila
están diferidos, no hay nada que ahorrar desconectando" — falso: un solo
servidor diferido alcanzaba para poner ese campo en `true` aunque OTRO
servidor del mismo body mandara su esquema completo sin diferir nada. Se
eliminó (§5) en vez de dejarlo con esta advertencia al lado. La ÚNICA fuente
de verdad por servidor, hoy y siempre, es `tools_by_server[i].deferred_tools`
(`docs/telemetry-per-request.md` §4.2): `deferred_tools == 0` en un servidor
puntual es la señal correcta de "estos bytes son reales y desconectables".

**Lo que la lente todavía no puede hacer sola:** distinguir "el harness
retuvo este servidor" de "el usuario nunca lo configuró" — la brecha que
documenta §3.4. Cerrarla en tráfico real (no solo en el experimento con
sonda) es, precisamente, el trabajo que le corresponde a `oxidegate-lens` por
correr localmente: leer la configuración de MCP del cliente y compararla
contra `tools_by_server`, declarado contra llegado.

Ese es el cierre honesto del arco: **nadie tiene que decidir a ciegas qué MCP
amputar.** Siguen todos conectados; OxideGate los capa **del contexto** y se
destapan solos cuando hacen falta.

---

## 10. Referencias

- `docs/findings.md` §E — la medición propia que mata el diseño ingenuo.
- `docs/optimizer-prompt-cache.md` §8 — orden de render `tools → system →
  messages`, y por qué tocar `tools` invalida el prefijo completo.
- `docs/optimizer-prompt-cache.md` §6.2 — el precedente de "confirmado por
  documentación ≠ medido en el cable".
- `docs/telemetry-per-request.md` — contrato de los campos expuestos por
  `GET /requests`, incluidos `client` y `tools_by_server[].deferred_tools`
  (§4.2 — la fuente de verdad POR SERVIDOR; el booleano body-wide que antes
  conflacionaba esto, `client_defer_loading`, se eliminó — ver §5).
- `src/provider/anthropic.rs` — `prepare` / `force_cache_control`: la puerta
  donde entra esta palanca.
- `oxidegate-lens/docs/COMPATIBILIDAD-HARNESSES.md` — qué harnesses cargan eager
  (donde el ahorro es real) vs. cuáles difieren. Su entrada de Claude Code se
  corrigió junto con el §3 de este doc.
