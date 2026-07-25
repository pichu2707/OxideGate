# Skills y `AGENTS.md` — qué cuestan de verdad en el cable

> Dos ejes que se pedían a menudo y nunca se habían medido. Uno resultó ser la
> superficie más barata del harness; el otro no existe en el cable. Ambos
> medidos sobre tráfico real de Claude Code 2.1.220, no estimados.

---

## 1. La respuesta primero

| Superficie | Coste medido | Dónde viaja |
|---|---|---|
| **Una skill declarada** (listado) | **138 B**, en CADA petición | `context_last_turn_bytes` |
| 11 skills de usuario | 1.520 B por petición | idem |
| Skills integradas del harness (8) | 3.411 B por petición | idem — se pagan siempre |
| **Invocar una skill** (§7) | **el cuerpo del `SKILL.md` menos el frontmatter, +~300 B** | historial, y ahí se queda |
| **`AGENTS.md`** | **0 B — no se manda** | en ninguna parte |

Las dos primeras filas y la cuarta son monedas distintas y conviene no
confundirlas: **declarar** una skill cuesta 138 B repetidos para siempre;
**invocarla** cuesta miles de bytes una vez, que luego el historial reenvía en
cada turno.

Y la cifra que cambia la intuición:

> Las skills pesan **200.601 B en disco** y mandan **~1,5 kB al cable**. Un
> factor de **132:1**. Son genuinamente perezosas: solo viaja el listado
> (nombre + descripción); el cuerpo del `SKILL.md` se queda en disco hasta que
> se invoca.

Compárese con los esquemas MCP, que viajan **enteros y en cada petición**
(159.874 B medidos en `docs/context-tax.md` §4.1). Por unidad de capacidad
declarada, una skill cuesta tres órdenes de magnitud menos que un servidor MCP.

---

## 2. El método

Dos instrumentos, porque uno solo no bastaba.

**A/B diferencial en el cable** (mismo método que `docs/optimizer-claude-md.md`
§2): sondas idénticas cambiando **solo** la variable bajo estudio,
`--strict-mcp-config` para congelar las herramientas, prompt `"Responde solo:
ok"`, y el delta leído en los buckets `context_*` de `GET /requests`.

**Captura del body**: un servidor local efímero que recibe la petición, la
guarda y responde algo mínimo. Nunca llama al proveedor, así que **no gasta
cuota** — y a cambio permite *leer* los bytes en vez de inferirlos de un delta.

El sandbox aisló `CLAUDE_CONFIG_DIR` a una copia mínima (credenciales, un
`CLAUDE.md` trivial y el directorio `skills/`). Se excluyó `settings.json` a
propósito: sus hooks inyectan un volcado de memoria que habría metido ruido en
todos los buckets. Al terminar se verificó que el `~/.claude/CLAUDE.md` real
seguía intacto (mismo `sha256`), que las 22 skills reales seguían ahí y que el
repositorio no había mutado (mismo `HEAD`).

---

## 3. Las skills: dónde viajan, y por qué importa

**No están en `system` ni en `tools`.** Viajan dentro del **último mensaje**,
inyectadas como `<system-reminder>`:

| bucket | 11 skills | 0 skills | delta |
|---|---|---|---|
| `context_system_bytes` | 9.603 | 9.603 | 0 |
| `context_tools_bytes` | 84.012 | 84.012 | 0 |
| **`context_last_turn_bytes`** | **8.246** | **6.745** | **−1.501** |

Buscarlas en `system` habría dado delta cero y la conclusión falsa de que las
skills son gratis. Es exactamente la misma trampa que documenta
`optimizer-claude-md.md` §2 con el `CLAUDE.md` global, que tampoco está donde
la intuición lo pone.

**El formato es un listado plano**, una línea por skill:

```
The following skills are available for use with the Skill tool:

- branch-pr: Create Gentle AI pull requests with issue-first checks. Trigger: …
- chained-pr: Trigger: PRs over 400 lines, stacked PRs, review slices. Split …
```

Leído directamente de la captura del body: **19 entradas y 4.931 B** con las
skills de usuario montadas, **8 entradas y 3.411 B** sin ellas. Delta: **11
skills, 1.520 B** → **138 B por skill**.

> **Las dos mediciones se corroboran.** El A/B en el cable dio −1.501 B; la
> captura del body, 1.520 B. Los ~20 B de diferencia son el identificador de
> sesión, que cambia entre invocaciones. Dos instrumentos independientes
> apuntando al mismo número.

**Consecuencia práctica.** La descripción del frontmatter **se paga en cada
petición, para siempre**, la uses o no. El cuerpo del `SKILL.md` no. Así que la
palanca no es "instala menos skills": es **escribe descripciones cortas**. Una
descripción de 400 B en una skill que nunca se invoca son 400 B por petición
durante toda la sesión.

Las 8 skills integradas del harness cuestan **3.411 B** y no se pueden quitar:
son el suelo de esta superficie.

---

## 4. `AGENTS.md`: no llega al cable

**Claude Code 2.1.220 no manda `AGENTS.md`.** Con un `AGENTS.md` de 61 B en el
directorio del proyecto, el body capturado no contiene **ni su contenido ni la
cadena `AGENTS`**: cero ocurrencias. Y el body con y sin el fichero tiene el
mismo tamaño exacto.

Que el binario mencione `AGENTS.md` —lo hace, 7 veces— no significa que lo
mande. Esas apariciones están en el **importador de configuración de Codex**,
que convierte un `AGENTS.md` de Codex en un `CLAUDE.md` de Claude Code, y en el
prompt de `/init`, que le dice al agente que lea los ficheros de otras
herramientas para *redactar* un `CLAUDE.md`. Ninguna es inyección en tiempo de
ejecución.

Para quien use `AGENTS.md` como fuente única entre herramientas: en Claude Code
hay que convertirlo a `CLAUDE.md`. El fichero no cuesta nada porque no se lee.

---

## 5. Una retractación, y por qué existe el control

La primera lectura de este eje decía que `AGENTS.md` costaba **+281 B** en
`context_system_bytes`. **Era falsa.**

La sonda 3 (con `AGENTS.md`) midió 9.884 B de `system` frente a los 9.603 B de
la sonda 2 (sin él). Un delta limpio de +281 B, con los otros cuatro buckets
idénticos al byte. Convincente.

La sonda 4 lo refutó. Era una **repetición exacta de la sonda 2** —sin
`AGENTS.md`— y midió 9.884 B: el valor de la sonda 3. Quitar el fichero no
revirtió nada, porque el fichero nunca había sido la causa. Los +281 B los
introdujo algo que se acumula entre invocaciones en el estado del proyecto, no
el `AGENTS.md`.

La captura del body lo cerró: cero ocurrencias.

> **El control no era burocracia.** Sin esa cuarta sonda, este documento
> publicaría una cifra inventada con aspecto de medición: delta limpio, otros
> buckets quietos, magnitud plausible. Toda medición diferencial necesita una
> repetición del control, porque un delta solo prueba que **algo** cambió — no
> que lo cambiara la variable que tú tocaste.

---

## 6. Lo que queda sin medir

- **Las skills de plugin.** Las 10 `sdd-*` de la instalación real no llegaron
  al cable en este experimento porque vienen de un plugin y el sandbox no copió
  `plugins/`. Que las de usuario cuesten 138 B no prueba que las de plugin
  cuesten lo mismo.
- **`AGENTS.md` en otros clientes.** Aquí se midió Claude Code. Codex, `pi` y
  OpenCode sí lo usan; cuánto cuesta ahí está sin medir.

---

## 7. Invocar una skill: la otra moneda

Declarar una skill cuesta 138 B en cada petición. **Invocarla cuesta otra
cosa, de otra forma.**

| skill | `SKILL.md` en disco | frontmatter | texto inyectado | delta total del body |
|---|---:|---:|---:|---:|
| `judgment-day` | 2.846 B | 245 B | 2.703 B | **2.998 B** |
| `branch-pr` | 5.498 B | 231 B | 5.366 B | **5.815 B** |

**El frontmatter no se reenvía.** Lo que entra es el CUERPO del `SKILL.md`, y
la descripción del frontmatter ya se había pagado en el listado. No hay doble
cobro — un detalle que la intuición no da por supuesto.

Al cuerpo se le antepone una línea con la ruta de la skill (`Base directory
for this skill: …`), que costó **+102 B** y **+99 B** en las dos medidas: es
constante y depende de la longitud de la ruta, no del tamaño del fichero.

El resto hasta el delta total son el bloque `tool_use` (~96 B), el
`tool_result` —que solo dice `Launching skill: <nombre>`, 29 B de texto— y el
escapado JSON del contenido.

> **Modelo completo:**
> `invocar ≈ (SKILL.md − frontmatter) + ~100 B de ruta + ~200 B de andamiaje`

**Y aquí está lo que de verdad importa.** Ese coste NO es de una vez. Entra en
el historial como un mensaje más, y el historial se reenvía **entero en cada
turno** (`docs/context-tax.md` §1: el coste de una conversación crece N², no
N). Invocar `branch-pr` en el turno 3 de una sesión de 50 turnos no cuesta
5.815 B: cuesta 5.815 B **multiplicados por los 47 turnos restantes**.

Comparado con el listado, la asimetría es brutal:

| | Coste | Frecuencia |
|---|---|---|
| Declarar `branch-pr` | 138 B | cada petición, siempre |
| Invocar `branch-pr` una vez | 5.815 B | cada petición desde ese turno |

Invocar una skill equivale a declarar **42 más**, con la diferencia de que el
listado es plano y la invocación entra en el historial que crece.

**La palanca práctica**, entonces, son dos y tiran en direcciones distintas:

- **Descripciones cortas** en el frontmatter — recortan el coste permanente de
  declarar.
- **Cuerpos concisos** en el `SKILL.md` — recortan el coste de invocar, que se
  multiplica por los turnos que queden de sesión.

Un `SKILL.md` de 30 kB invocado pronto en una sesión larga es, con diferencia,
la forma más cara de configurar un agente que se ha medido en este repositorio.

### Cómo se midió, sin gastar cuota

Forzando la invocación desde el servidor de captura: se responde a la petición
del agente con un `tool_use` fabricado de la herramienta `Skill`, Claude Code
la ejecuta **en local** y manda la siguiente petición con el contenido dentro.
Ningún modelo interviene.

Dos obstáculos que costaron un rato y conviene no reaprender:

1. **`claude -p` intercala llamadas auxiliares pequeñas (~2 kB) con la del
   agente (~135 kB).** Responder el `tool_use` a una auxiliar no hace nada.
   Hay que discriminar por tamaño.
2. **Claude Code pide SSE.** Respondiéndole JSON plano, el stream muere con
   `Stream completed without receiving message_start event`, cae al modo no
   streaming y **reintenta la petición entera** — lo que hacía parecer que el
   `tool_use` se ignoraba, cuando lo que pasaba es que nunca llegó a leerse.
   Hay que hablar el protocolo de eventos.

---

## Ver también

- `docs/optimizer-claude-md.md` — el mismo método aplicado al `CLAUDE.md`
  global, y el precedente de "el ahorro no está donde la intuición lo pone".
- `docs/optimizer-tool-search.md` — el eje contrario: por qué marcar una tool
  con `defer_loading` cuesta 21 bytes y no quita ninguno.
- `docs/context-tax.md` §4.1 — la composición completa de una petición real.
