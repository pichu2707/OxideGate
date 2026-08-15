# Skills entre herramientas — la misma convención, cuatro precios distintos

> `SKILL.md` se ha convertido en una convención compartida: cuatro de las cinco
> herramientas medidas lo usan, en el mismo sitio del disco y con el mismo
> formato de fichero. **Lo que no comparten es cómo lo mandan al cable.** Cada
> una elige un sitio distinto del body y un formato distinto, y la diferencia
> de precio por skill llega a ser de **2,8×**.

Medido con captura de body, cero cuota: un servidor local recibe la petición y
responde sin llamar a ningún proveedor.

---

## 1. La tabla

| Herramienta | Dónde viaja | Formato | Skills | Bloque | **Por skill** |
|---|---|---|---:|---:|---:|
| **Claude Code** 2.1.220 | último turno (`system-reminder`) | lista plana `- nombre: desc` | 19 | 4.931 B | **138 B** |
| **Gemini CLI** 0.49.0 | `systemInstruction` | XML `<available_skills>` | 23 | 6.637 B | **288 B** |
| **opencode** 1.18.5 | `messages` (bloque de sistema) | XML `<available_skills>` | 23 | 7.433 B | **323 B** |
| **Codex** 0.142.5 | `input` (dialecto Responses) | `<skills_instructions>` | 43 | 16.798 B | **390 B** |
| **pi** 0.80.10 | — | no tiene el mecanismo | — | — | — |

Ese listado se paga **en cada petición**, se invoque una skill o no.

---

## 2. Por qué Claude Code cuesta menos de la mitad

No es una diferencia de eficiencia genérica: es **una decisión concreta de
formato**. Las otras tres mandan la **ruta absoluta en disco** de cada
`SKILL.md`; Claude Code no manda ninguna.

| Herramienta | Bytes de rutas | % del bloque |
|---|---:|---:|
| Claude Code | 0 | **0%** |
| Codex | 3.793 B | 23% |
| Gemini CLI | 1.761 B | 26% |
| opencode | 2.445 B | **33%** |

En opencode, **un tercio del listado de skills son rutas del sistema de
ficheros**. No aportan capacidad: solo le dicen al modelo dónde está un fichero
que él no va a abrir por sí mismo.

El resto de la diferencia es el envoltorio. XML (`<skill><name>…</name>
<description>…</description><location>…</location></skill>`) cuesta ~60 B de
etiquetas por entrada que la lista plana de Claude Code no paga.

---

## 3. Los cuatro sitios

Que el listado esté en un bucket u otro no es cosmético: determina **qué campo
de `/requests` lo contiene** y, por tanto, dónde hay que buscarlo.

| Herramienta | Bucket | En `GET /requests` cae en |
|---|---|---|
| Claude Code | último mensaje | `context_last_turn_bytes` |
| Gemini CLI | `systemInstruction` | `context_system_bytes` |
| opencode | mensaje de sistema dentro de `messages` | `context_system_bytes` o `context_history_bytes` |
| Codex | `input[]` | `context_history_bytes` / `context_last_turn_bytes` |

**Un detector único habría fallado.** Buscar el patrón de Claude Code en el
`system` de Gemini da cero, y la conclusión falsa sería "Gemini no manda
skills" — cuando manda más del doble por skill.

---

## 4. El método, y sus dos trampas

Servidor de captura local por herramienta: recibe la petición, la guarda y
responde. Nunca llama al proveedor, así que **no gasta cuota**.

**Trampa 1 — la primera petición no es la del agente.** Gemini CLI abre con una
llamada a un modelo `flash-lite` cuyo prompt de sistema empieza por *"You are a
specialized Task Routing AI"*: puntúa la complejidad de la tarea antes de
enrutarla. Medir esa da 7.090 B y cero skills. La del agente son 82.267 B y sí
las lleva. Claude Code hace algo equivalente con llamadas auxiliares de ~2 kB.

**Trampa 2 — hay que contestar algo válido para llegar a la buena.** Devolver
un error mata la conversación en la primera llamada. Hasta que el servidor no
respondió una puntuación de complejidad plausible, Gemini CLI nunca llegó a
emitir la petición del agente.

> Corolario para cualquier medición futura: **discriminar la petición del
> agente por tamaño**, y hablar lo justo del dialecto para que la herramienta
> siga adelante.

---

## 5. Una corrección

Un recuento inicial en disco dio **657 skills en Codex**. Es falso: incluía
`node_modules` y directorios `.tmp/marketplaces` anidados. **El cable dice 43.**

El disco cuenta ficheros; el cable cuenta lo que de verdad se paga. Cuando no
coincidan, manda el cable.

---

## 6. `AGENTS.md`: tres de cuatro lo mandan, y a precios distintos

`AGENTS.md` de **74 B** en el directorio del proyecto, medido por delta con y
sin el fichero:

| Herramienta | Dónde cae | Coste | Sobrecoste sobre el fichero |
|---|---|---:|---:|
| **Claude Code** 2.1.220 | — | **0 B — no lo manda** | — |
| **Codex** 0.142.5 | `input` | **+159 B** | +85 B |
| **opencode** 1.18.5 | `messages` | **+160 B** | +86 B |
| **pi** 0.80.10 | `system` | **+200 B** | +126 B |

Las tres que lo leen lo envuelven con un marcador y **la ruta absoluta del
fichero**. Es el mismo patrón que encarece sus listados de skills (§2): pagan
por decirle al modelo dónde está un fichero que el modelo no va a abrir.

> **Las marcas de esta tabla se recapturaron después, y una no existía.** Aquí
> se documentaba que Codex usa `--- project-doc ---` dentro de
> `<INSTRUCTIONS>`: `grep` sobre la captura real de 0.142.5 devuelve **cero**.
> La de verdad es `# AGENTS.md instructions for <ruta>`. Las marcas verificadas,
> con la versión con la que se capturó cada una, están en
> [`telemetry-per-request.md`](telemetry-per-request.md) §4.13.

El sobrecoste **no es fijo**, aunque estas tres cifras lo parezcan. Recapturado
con un `AGENTS.md` de 202 B, el envoltorio de las tres está **dominado por la
ruta absoluta** —65% en Codex, 86% en opencode, 69% en `pi`— así que los +159,
+160 y +200 B de arriba son en realidad `62 B + ruta`, `21 B + ruta` y
`55 B + ruta`. Salen de las rutas de ESTA máquina. Un proyecto en `/home/u/p`
paga bastante menos que uno en un directorio profundo, y **ninguna de estas
cifras se puede comparar entre máquinas sin decir la ruta**. Ver
[`banco-de-captura.md`](banco-de-captura.md) §5.

Lo que sí es cierto es que el sobrecoste no es proporcional al contenido: en un
`AGENTS.md` real de varios kB se amortiza. Sobre uno de 74 B, más que dobla el
fichero.

> **Para quien use `AGENTS.md` como fuente única entre herramientas:** en
> Claude Code el fichero es gratis porque **se ignora** —hay que convertirlo a
> `CLAUDE.md` o no se aplica— y en las otras tres se paga en cada petición.
> El mismo fichero, cuatro comportamientos.

### `pi` comprime —pero solo contra un backend— y eso cambia cómo leer la cifra

Re-medido con captura de body a cero cuota (la primera medida fue por delta a
través del proxy, y no podía ver esto): **`pi` mandó su body comprimido con
zstd.**

> **Corrección (2026-08-15): no es una propiedad de `pi`.** Aquí se escribió que
> es «el único de los cuatro que lo hace», y eso generaliza de más. `zstd`
> aparece en un solo sitio de su código —`pi-ai/dist/api/openai-codex-responses.js`—
> con este comentario: *«The Codex backend accepts zstd-compressed request
> bodies on the SSE responses endpoint (the same endpoint the official Codex
> client compresses against)»*. O sea: comprime **solo cuando habla la API
> `openai-codex-responses`**, solo en la ruta SSE, y porque lo hace el cliente
> oficial de Codex contra ese endpoint. El transporte WebSocket manda JSON sin
> comprimir incluso ahí.
>
> Capturado el 2026-08-15 contra un proveedor `openai-completions`, `pi` 0.80.10
> manda **JSON plano**. La compresión es del **endpoint**, no del harness.

| | con `AGENTS.md` | sin | delta |
|---|---:|---:|---:|
| Lógico (JSON) | 138.655 B | 138.461 B | **+194 B** |
| **Cable (zstd)** | 43.379 B | 43.306 B | **+73 B** |

Sobre un fichero de 67 B. Concuerda con la primera medida —74 B daban +200 B—:
~127 B de envoltorio.

> **Ese ~127 B no es una constante, y las dos medidas no lo prueban.** Se
> tomaron en la MISMA máquina, con la MISMA ruta de proyecto, así que coincidir
> era lo esperable. La recaptura de 0.80.10 con ruta larga da 175 B de
> envoltorio, de los cuales **120 son la ruta**: el fijo real de `pi` son 55 B.
> Ver §6 y [`banco-de-captura.md`](banco-de-captura.md) §5.

> **Cuándo son LÓGICAS las cifras de la tabla de arriba.** Solo cuando `pi` va
> contra el backend de Codex, que es donde comprime: ahí el cable es ~1/3 del
> lógico. Contra cualquier otro proveedor `pi` manda JSON plano y las dos cifras
> coinciden, igual que en las otras tres herramientas. Comparar el +200 B de
> `pi` con el +159 B de Codex **sin decir contra qué backend iba** lo penaliza
> por partida doble: se le cuenta el bloque descomprimido y se le ignora una
> compresión que además no siempre ocurre.

Lo lógico sigue siendo lo que se factura —el proveedor tokeniza el JSON, no el
zstd— así que la tabla no está mal. Pero si lo que se mira es **ancho de banda**
y no tokens, `pi` **contra el backend de Codex** es el más barato de los cuatro,
no el más caro. Contra otros proveedores, viaja como todos.

El envoltorio es `<project_instructions path="/ruta/absoluta/AGENTS.md">` dentro
de un bloque `<project_context>`, en el bucket `instructions`.

### Un indicio que resultó falso

El binario de `pi` no contiene **ninguna** mención de `AGENTS.md` —cero, frente
a las 7 del binario de Claude Code— y aun así **es la herramienta que más caro
lo cobra**. Buscar cadenas en un binario empaquetado no prueba una ausencia:
prueba que el empaquetador las escondió. Si esa señal se hubiera tomado por
buena, la conclusión habría sido exactamente la contraria a la medida.

---

## 7. Invocar una skill: cuatro mecanismos, y dos que no existen

Declarar una skill cuesta entre 138 B y 390 B por petición (§1). **Invocarla es
otra cosa — y no siempre es posible por la vía que la propia herramienta
anuncia:** Codex no tiene mecanismo, y Gemini solo declara el suyo en modo
interactivo.

| Herramienta | Mecanismo de invocación | Coste medido |
|---|---|---|
| **Claude Code** | Herramienta `Skill` dedicada | **2.998 B** |
| **opencode** | Herramienta `skill` dedicada | **3.335 B** |
| **Codex** | **Ninguna**: da un `file:` locator y el modelo lee el fichero | sin mecanismo propio |
| **Gemini CLI** | Anuncia `activate_skill`; la declara en **interactivo**, no en `-p` | **depende del modo** |

Misma skill en las dos que sí funcionan (`judgment-day`, 2.846 B en disco):

| | Claude Code | opencode |
|---|---:|---:|
| Texto inyectado | 2.703 B | 3.073 B |
| Delta total del body | **2.998 B** | **3.335 B** |
| ¿Reenvía el frontmatter? | no | no |

**Ninguna reenvía el frontmatter** — ya se pagó en el listado. La diferencia
está en el envoltorio: opencode envuelve con `<skill_content name="…">` y
repite el nombre como cabecera, ~470 B frente a los ~100 B de ruta de Claude
Code. Un 11% más cara por la misma capacidad.

### Gemini CLI: la puerta existe, pero solo en interactivo

> **Corrección del 2026-08-08.** La versión anterior de esta sección decía
> *"se paga el listado y no hay puerta"*, sin más. Era cierta **solo del modo
> que se había probado**, y el propio texto lo declaraba en un aviso de
> alcance. Ese aviso se cobró: medido el modo interactivo, la herramienta **sí
> se declara**. Se corrige aquí en vez de reescribir el pasado, igual que la
> retractación de `docs/optimizer-skills.md` §5.

El `systemInstruction` de Gemini dice, literalmente:

> *"To activate a skill and receive its detailed instructions, call the
> `activate_skill` tool with the skill's name."*

Si esa herramienta llega o no **depende del modo**, y la diferencia es enorme:

| | `gemini -p` (headless) | `gemini -i` (interactivo) |
|---|---:|---:|
| Cuerpo del agente | 82.217 / 83.315 B | 116.393 / 117.454 B |
| Tools declaradas | **11** | **36** |
| **`activate_skill` entre ellas** | **NO** (0/2) | **SÍ** (2/2) |
| Entradas `<skill>` en el listado | 23 | 23 |
| `activate_skill` en `systemInstruction` | 2 | 2 |

Dos sondas por modo, todas capturadas a coste cero (Gemini CLI 0.49.0,
`gemini-3.1-pro-preview`). Las once de headless son `update_topic`,
`list_directory`, `read_file`, `grep_search`, `glob`, `replace`, `write_file`,
`web_fetch`, `google_web_search`, `enter_plan_mode` e `invoke_agent`.

**El fallo es real, y es solo de headless.** En `gemini -p` el prompt manda
llamar una herramienta que no viaja: se pagan los **288 B por skill** y el
modelo no tiene forma de canjearlos. En interactivo la puerta está, así que ahí
el listado sí compra algo.

> **Lo que NO se pudo aislar.** El modo interactivo trae **25 tools más**, no
> una: **20 son de MCP** y las otras cinco son `activate_skill`,
> `run_shell_command`, `ask_user` y las dos de procesos en segundo plano.
>
> Las de MCP tienen explicación en el propio bundle —`if (this.interactive ||
> this.acpMode) await this.mcpInitializationPromise`—, y las otras cuatro piden
> a alguien al otro lado. Pero el registro de `activate_skill` cuelga del
> descubrimiento de skills, no de ese flag, así que encaja en el grupo por
> parecido, no por código leído.
>
> **La correlación con el modo está medida 2/2 y 2/2; el mecanismo exacto, no.**
> Se dice en vez de rellenarlo con una explicación plausible, que es justo lo
> que §5 documenta que sale caro.

**El listado es idéntico en los dos modos** (23 entradas). El precio por skill
de §1 no cambia; lo que cambia es si se puede canjear.

### Codex: no invoca, lee

Codex no tiene herramienta de skill. Su bloque entrega un **locator** por
entrada —`(file: /ruta/SKILL.md)`— y explica que los `file` locators están en
el sistema de ficheros del host. El modelo abre el fichero con sus herramientas
normales.

Eso significa que **en Codex no hay un "coste de invocar" propio**: hay el
coste de una lectura de fichero, que depende de cómo la haga el modelo (entero,
por rangos) y no de un mecanismo de skills. Compararlo con los 2.998 B de
Claude Code sería comparar dos cosas distintas.

---

## 8. Lo que queda sin medir

- ~~**`activate_skill` en el modo interactivo de Gemini**~~ — medido el
  2026-08-08: **sí la declara** (2/2 sondas), y el veredicto de §7 cambió. Queda
  sin aislar POR QUÉ: el interactivo trae 25 tools más, no una.
  ([#68](https://github.com/pichu2707/OxideGate/issues/68))
- **El coste real de leer un `SKILL.md` en Codex**, que depende de cómo lo lea
  el modelo y no de un mecanismo de skills.
- **Todas las cifras son de UNA instalación**: la de este equipo, con este
  conjunto de skills. El coste POR SKILL es comparable entre herramientas; el
  total depende de cuántas tenga cada una instaladas.

---

## Ver también

- `docs/optimizer-skills.md` — el eje completo en Claude Code: declarar,
  invocar, `AGENTS.md`, y la retractación que forzó la sonda de control.
- `docs/telemetry-per-request.md` §4.2 — `tools_by_server`, el precedente de
  atribuir dentro de un bucket en vez de conformarse con el total.
