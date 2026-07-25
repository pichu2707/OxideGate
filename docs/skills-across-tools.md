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

## 6. Lo que queda sin medir

- **El coste de invocar** en Gemini, opencode y Codex. En Claude Code está
  medido (`docs/optimizer-skills.md` §7): el cuerpo del `SKILL.md` menos el
  frontmatter. Las otras tres usan una herramienta de activación propia
  (`activate_skill` en Gemini) y el mecanismo puede diferir.
- **`AGENTS.md` en estas herramientas.** opencode tiene uno en su directorio de
  configuración. Claude Code no lo manda (`optimizer-skills.md` §4); en las
  demás está sin medir.
- **Todas las cifras son de UNA instalación**: la de este equipo, con este
  conjunto de skills. El coste POR SKILL es comparable entre herramientas; el
  total depende de cuántas tenga cada una instaladas.

---

## Ver también

- `docs/optimizer-skills.md` — el eje completo en Claude Code: declarar,
  invocar, `AGENTS.md`, y la retractación que forzó la sonda de control.
- `docs/telemetry-per-request.md` §4.2 — `tools_by_server`, el precedente de
  atribuir dentro de un bucket en vez de conformarse con el total.
