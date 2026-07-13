# El `CLAUDE.md` lean — el byte es real, el comportamiento es otra cosa

> Estado: **byte medido en el cable**, comportamiento **parcialmente medido y
> parcialmente no medible con este instrumento**. Todo lo que sigue sale de
> sondas reales a `claude-opus-4-8` capturadas por OxideGate (puerto 8899) y de
> un A/B de comportamiento observado vía `claude -p --output-format stream-json`,
> el 2026-07-10. Donde algo no se pudo medir, se dice por qué, no se rellena con
> una impresión.

---

## 1. La respuesta primero

Un `CLAUDE.md` global de 34.922 B se puede reducir a 5.738 B (−83,6% en disco)
conservando las reglas de persona y reemplazando los tres protocolos largos
—Engram, Agent Teams, SDD— por punteros de una línea a sus skills. En el cable,
eso son **−29.509 bytes por petición**, en cada turno de cada sesión.

Pero el `CLAUDE.md` no se paga en bytes: se paga en **comportamiento**. La
pregunta que decide si esta palanca se puede accionar no es "¿cuánto pesa?" sino
"¿el agente sigue obedeciendo las reglas cuando el texto que las describe ya no
está?". Esa pregunta se midió en parte:

- **Delegación: sobrevivió al lean.** Con el protocolo Agent Teams entero
  borrado y sustituido por una línea, el agente delegó igual (2/2 frente a 2/2).
- **Guardado proactivo en memoria: no medible con este instrumento.** Ni
  siquiera el `CLAUDE.md` completo dispara el `mem_save` proactivo en modo
  `-p` de un solo turno (0/3). Sin un baseline que dispare la conducta, el A/B
  no puede decir si el lean la pierde.

---

## 2. El byte, medido en el cable

Dos sondas idénticas (`claude -p "Responde solo: ok"`, `--strict-mcp-config`
para congelar las herramientas), cambiando **solo** el `CLAUDE.md` del entorno
(ver §3.1 sobre el sandbox que evita tocar el `~/.claude` real):

| componente | full (34.922 B) | lean (5.738 B) | delta |
|---|---|---|---|
| `context_system_bytes` | 7.019 | 7.019 | 0 |
| `context_history_bytes` (donde se inyecta el `CLAUDE.md`) | 71.458 | 41.949 | **−29.509** |
| `context_tools_bytes` | 86.198 | 86.198 | 0 |
| `context_last_turn_bytes` | 17.781 | 17.781 | 0 |
| **`prompt_bytes` (body total)** | **182.940** | **153.431** | **−29.509 (−16,1%)** |

**Hallazgo 1 — el ahorro es real y vive donde debe.** Los −29.509 B caen
enteros en `context_history_bytes`, que es donde el `CLAUDE.md` global viaja:
envuelto en un `<system-reminder>` dentro de `messages[0]`, no en el bloque
`system` (ver `docs/context-tax.md` §4.1). Buscarlo en `system` habría dado
delta cero y una conclusión falsa.

**Hallazgo 2 — confirma la estimación previa, ahora en el cable.** Una medición
anterior estimó el ahorro por análisis de contenido en −29.867 B (el 85,1% del
archivo era flujo, no regla). La sonda del cable da −29.509 B: coinciden dentro
del error de re-serialización. Lo que era estimación es ahora medición.

> **Sobre el porcentaje.** Aquí el ahorro es −16,1% porque la sonda va sin MCP
> (body de 182.940 B). Contra un body con los servidores MCP cargados (~225 kB,
> ver `docs/context-tax.md` §4.1) el mismo ahorro absoluto es ~−13%. El byte
> ahorrado no cambia; el denominador sí. Citar el porcentaje sin el body de
> referencia es citar media cifra.

---

## 3. El comportamiento: un A/B observando los `tool_use`

El riesgo del lean no es el byte, es perder reglas en silencio. Para medirlo
hace falta **observar si el agente dispara las conductas** que los protocolos
borrados gobiernan. `claude -p --output-format stream-json` emite cada
`tool_use` del agente, así que la conducta se vuelve observable y binaria por
corrida: ¿llamó a la herramienta o no?

Tres conductas, cada una con su señal de disparo:

| conducta | protocolo que la gobierna | ¿lo conserva el lean? | señal observable |
|---|---|---|---|
| Delegar exploración de 4+ archivos | Agent Teams (borrado, sustituido por 1 línea) | puntero terso | `tool_use` = `Agent`/`Task` |
| Guardar en memoria de forma proactiva | Engram (borrado, sustituido por 1 línea) | puntero terso | `tool_use` = `mem_save` |
| Cargar la skill que matchea la tarea | Contextual Skill Loading (**conservado** en el bloque persona) | sí, verbatim | `tool_use` = `Skill` |

La tercera es un **control**: la regla se conserva idéntica en ambas variantes,
así que su propósito es detectar si el lean rompe algo que no debería, no
discriminar.

### 3.1. Por qué un sandbox, y por qué no se tocó `~/.claude`

El A/B exige alternar el `CLAUDE.md` entre full y lean. Hacerlo sobre el
`~/.claude/CLAUDE.md` real arriesga exactamente lo que esta palanca teme:
perder reglas si algo sale mal. En su lugar se copió `~/.claude` a un `HOME`
temporal y se alternó **su** `CLAUDE.md`. Se verificó con un centinela que el
agente carga el `CLAUDE.md` del sandbox (respondió el codeword inyectado), y al
terminar, que el `~/.claude/CLAUDE.md` real seguía intacto (mismo `sha256`, sin
centinela) y que el repositorio no había mutado (mismo `HEAD`). El entorno real
nunca se tocó.

> **Un límite del sandbox, dicho de frente.** Los plugins de Engram usan rutas
> **absolutas** al `~/.claude` real, así que el `HOME` cambiado aísla el
> `CLAUDE.md` y las skills, pero no el almacén de Engram. Los `mem_save` de
> prueba, con `project` sin resolver bajo el sandbox, no llegaron a persistir en
> ningún proyecto del store (se comprobó después: no hay proyecto de prueba en
> Engram). Aislamiento suficiente para el A/B del `CLAUDE.md`, que es lo que se
> estaba midiendo.

### 3.2. Resultados

| conducta | FULL | LEAN | lectura |
|---|---|---|---|
| Delegar (`Agent`) | **2/2** | **2/2** | el lean delegó igual, pese a borrar todo Agent Teams |
| Guardar (`mem_save`) | 0/3 | 1/3 | ninguna de las dos dispara de forma fiable en `-p` |
| Cargar skill (`Skill`) | 1/3 | 0/3 | inconcluso: ni el full la carga de forma fiable |

**Hallazgo 3 — la delegación sobrevive al lean, y eso refuta la parte medible
del miedo.** El protocolo Agent Teams entero —la tabla de delegación, los
triggers, el contrato de coordinador— se redujo a una línea: *"sos un
COORDINADOR: delegá la exploración de 4+ archivos… vía la herramienta Agent"*.
Con esa línea, el agente delegó en las dos corridas, igual que con el protocolo
completo. Para esta conducta, "adoptar el lean deja de delegar" es **falso**: el
puntero terso bastó.

**Hallazgo 4 — el guardado proactivo no se pudo medir, y la razón importa.** El
`CLAUDE.md` completo, con el protocolo Engram marcado como "MANDATORY y ALWAYS
ACTIVE", **no disparó `mem_save` en ninguna de sus tres corridas**. No es que el
lean lo pierda: es que el baseline tampoco lo tiene. El modo `-p` de un solo
turno no ejercita la disciplina proactiva, que está pensada para sesiones
interactivas multi-turno. Sin un baseline que dispare la conducta, el A/B no
tiene contra qué comparar. Es el mismo patrón que ya apareció dos veces en este
proyecto: **una palanca sobre una tarea que no la ejercita no mide nada** (ver
el experimento fallido de prosa en `docs/speed.md` §3.1, y el efecto techo de
§3.2).

**Hallazgo 5 — la carga de skill es ruido a este n.** El control cargó la skill
1 de 3 veces con el full y 0 de 3 con el lean; las demás corridas crearon la
skill a mano (Bash/Write) sin invocar la herramienta `Skill`. Con la regla
conservada en ambas variantes y un disparo tan intermitente, la diferencia
1-vs-0 sobre n=3 no sostiene ninguna afirmación.

> **Confusor declarado.** Las 16 corridas registraron un `rate_limit_event` cada
> una. La presión de rate limit puede acortar el comportamiento del agente
> (menos herramientas, respuestas más directas) y contamina sobre todo las
> conductas intermitentes (save, skill). La delegación, que disparó 4/4, es la
> menos expuesta a ese ruido.

---

## 4. Lo que queda sin medir, y con qué instrumento se mediría

La conducta que el lean más amenaza —el guardado proactivo en memoria— sigue
sin medir, no por falta de intento sino porque el instrumento es el equivocado.
Verificarla exige lo que `claude -p` no da: una **sesión interactiva
multi-turno** donde la disciplina proactiva se active, con el mismo trabajo real
hecho una vez con el `CLAUDE.md` full y otra con el lean, contando los
`mem_save` de cada una. Eso no se automatiza con una sonda de un disparo, y por
eso no está en esta página.

Tampoco se midió la calidad de la **obediencia**, solo su disparo: que el agente
llame a `Agent` no prueba que delegue *bien*, solo que delega. La distinción
entre "dispara la conducta" y "la ejecuta con criterio" queda fuera de lo que un
contador de `tool_use` puede ver.

---

## 5. Veredicto práctico

- **El byte es una palanca real y accionable:** −29.509 B por petición,
  reproducibles en el cable, sin ambigüedad.
- **No es gratis, pero es menos cara de lo que se temía.** De las dos conductas
  de riesgo, una —delegar— sobrevivió al puntero terso. La otra —guardar en
  memoria— no se pudo refutar ni confirmar con este método.
- **Adoptarla con los ojos abiertos, no a ciegas.** El lean conserva las reglas
  de persona y la carga contextual de skills (el mecanismo que hace que los
  punteros funcionen). Antes de adoptarlo en serio faltaría el test que este no
  pudo hacer: una sesión interactiva que confirme que el guardado proactivo y
  las demás disciplinas de sesión no se caen en silencio.

La conclusión honesta no es "el lean es seguro" ni "el lean rompe reglas": es
"el lean ahorra 29 kB por turno, no rompió la delegación, y la conducta que más
lo pondría a prueba necesita un banco de pruebas que una sonda de un turno no
es".

---

## Ver también

- `docs/context-tax.md` §4.1 — dónde vive el `CLAUDE.md` dentro del body (por qué en `messages[0]`, no en `system`)
- `docs/context-tax.md` §6 — la memoria persistente como inyección, no como compresión
- `docs/findings.md` §D — la fila de esta palanca, resumida junto al resto
- `docs/speed.md` §3.1-§3.2 — el patrón "una palanca necesita una tarea que la ejercite", medido dos veces antes
