# El peaje fijo — qué cuesta pedir "ok" en cada herramienta

> Antes de hacer ningún trabajo, cada herramienta manda una cantidad de bytes
> solo por existir: esquemas de herramientas, instrucciones, memoria inyectada.
> Este documento mide ese **peaje**, con la misma tarea trivial en las cuatro.
>
> **No mide cuál es más barata para trabajo real.** Eso es otra cosa, exige
> cuota y n>1, y está declarado al final como no medido.

---

## 1. La tabla

Prompt idéntico —`"Responde solo: ok"`— en un directorio de proyecto vacío,
con la configuración **realmente instalada en esta máquina**. Bytes del body de
la petición del agente:

| Herramienta | **Total** | Esquemas de tools | Instrucciones | Mensajes |
|---|---:|---:|---:|---:|
| **Claude Code** 2.1.220 | **148.075** | 87.654 | 9.889 | 51.430 |
| **opencode** 1.18.5 | **117.125** | 49.219 | 0 | 68.905 |
| **Gemini CLI** 0.49.0 | **82.107** | 12.003 | 65.947 | 4.349 |
| **Codex** 0.142.5 | **78.030** | 21.213 | 4.788 | 51.563 |

En las cuatro, **el mensaje del usuario son 17 bytes**. Todo lo demás es
ceremonia. Es la misma conclusión que [`context-tax.md`](context-tax.md) §1,
ahora en cuatro herramientas a la vez.

---

## 2. Lo interesante no es el orden, es la forma

Cada una gasta su presupuesto en un sitio distinto, y eso dice más que el
total:

- **Claude Code** se lo gasta en **esquemas de herramientas**: 87.654 B, el
  59% del body. (Con una advertencia grande en §4.)
- **opencode** no manda **nada** en el bloque de sistema: sus 117 kB van todos
  en `messages`, incluido lo que otras ponen en `system`.
- **Gemini CLI** hace lo contrario: el **80% en `systemInstruction`** y solo el
  15% en tools. Ahí van sus skills en XML y su prompt.
- **Codex** es el más barato de los cuatro, con instrucciones y mensajes
  repartidos.

Dos herramientas con el mismo total podrían tener palancas de ahorro
completamente distintas. Por eso el desglose importa más que el ranking.

---

## 3. Determinismo

| Herramienta | Capturas del agente | Resultado |
|---|---|---|
| Gemini CLI | 4 | **idénticas al byte** |
| Codex | 6 | **idénticas al byte** |
| Claude Code | 2 | difieren en 14 B (identificador de sesión) |
| opencode | 1 | **sin comprobar** |

El peaje es reproducible: no hace falta n>1 para medirlo, a diferencia de una
tarea real. En opencode quedó con una sola captura y así se declara.

---

## 4. Tres avisos, sin los cuales esta tabla miente

**1. El medidor contamina a Claude Code, y le perjudica.** Detrás de un
`ANTHROPIC_BASE_URL` no-first-party —OxideGate lo es— Claude Code **deja de
diferir sus esquemas MCP y los manda todos de golpe**
([`optimizer-tool-search.md`](optimizer-tool-search.md) §3, medido con grupo de
control). Así que parte de esos 87.654 B **existen porque el medidor está en el
camino**. Sin él, la cifra de Claude Code sería menor y la tabla podría
ordenarse distinto.

**2. Es "tal como está instalado aquí", no una propiedad de la herramienta.**
Cada una lleva un juego distinto de skills, MCP y configuración en esta máquina
—22 skills de usuario en Claude Code, 43 entradas en Codex, `AGENTS.md` en
opencode—. **Los totales no son comparables entre instalaciones**; la
estructura de §2 sí.

**3. `pi` comprime, pero solo contra un backend.** No está en esta tabla porque
no tiene mecanismo de skills, pero al medirlo aparte se vio que mandaba su body
con **zstd**: 138.655 B lógicos viajan como 43.379 B de cable. Recapturado el
2026-08-15, eso **no es una propiedad de `pi`**: comprime solo cuando habla la
API `openai-codex-responses` en su ruta SSE, porque es lo que hace el cliente
oficial de Codex contra ese endpoint. Contra un proveedor `openai-completions`
manda JSON plano, igual que las cuatro de aquí. Si alguna vez se añade `pi` a
esta tabla, **hay que decir contra qué backend iba y cuál de los dos números se
está comparando** (ver `skills-across-tools.md` §6 y `banco-de-captura.md` §7).

**4. Modelos distintos.** Claude Code habla con Opus, Codex con `gpt-5.5`,
Gemini con `gemini-3.1-pro-preview`, opencode con lo que se le configure. La
comparación es de **bytes enviados**, no de coste en dinero ni de calidad de
respuesta.

---

## 5. Lo que este documento NO dice

- **Cuál es más barata para trabajo real.** Una tarea de verdad gasta cuota y
  **no es determinista**: dos ejecuciones difieren en turnos, tools invocadas y
  tokens. Comparar una contra otra no mide la herramienta, mide el ruido.
- Para eso haría falta: una tarea **cerrada y verificable** (correcta o
  incorrecta, no abierta), **n>1 por herramienta** con los rangos publicados, y
  declarar la contaminación del §4.
- **Publicar "la herramienta X cuesta N veces más que Y" a partir de una
  ejecución por herramienta sería la cifra más llamativa de este repositorio y
  la menos medida.** Está sin hacer, y se dice.

---

## Ver también

- [`context-tax.md`](context-tax.md) — el desglose completo de una petición
  real, y por qué el turno nuevo es el 0,03%
- [`skills-across-tools.md`](skills-across-tools.md) — el mismo ejercicio
  acotado a skills y `AGENTS.md`
- [`optimizer-tool-search.md`](optimizer-tool-search.md) §3 — la contaminación
  del §4, medida con grupo de control
