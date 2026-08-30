# El modelo del nivel 1 — el mismo modelo, con el razonamiento apagado

> Medido el **2026-08-25** contra `ollama` 0.30.10, `n=30`. Coste: **cero**.
> Desbloquea la opción A de [#121](https://github.com/pichu2707/OxideGate/issues/121).

**El modelo del nivel 1 es `qwen3:14b-nothink`**, un derivado local de
`qwen3:14b` con el razonamiento apagado dentro del modelo. Se deriva con
`cargo run --example derivar-nothink`. Este documento dice por qué hizo falta,
qué se midió, y qué es lo que esto **no** demuestra.

---

## 1. La segunda condición, la que faltaba

[`banco-de-tareas.md`](banco-de-tareas.md) comprueba que el modelo local
**sepa resolver la tarea**: un turno, sin herramientas, el fichero en la mano.
`qwen2.5:7b` lo pasa 4/10.

Eso es necesario y **no es suficiente**. El nivel 1 pone al modelo a conducir un
harness, y conducir un harness es otra capacidad:

| condición | qué la comprueba |
|---|---|
| sabe **resolver** la tarea | `examples/calibrar.rs` |
| sabe **operar herramientas** | `examples/sonda-herramientas.rs` |
| **entrega** lo que ha averiguado | esto — y casi cuesta el issue |

La tercera se dio por supuesta, y es la que estuvo a punto de matar #121.

---

## 2. El dato

`qwen3:14b` emite llamadas a herramientas impecablemente. Lo que no hace es
**contestar**: deja el resultado leído en el campo `thinking` y entrega
`content` vacío. Un harness consume `content`, así que recibe nada.

Con el razonamiento apagado desaparece. Turno encadenado del techo de la sonda,
por `/v1/chat/completions` —la ruta real del harness a través de OxideGate—, sin
tocar la petición:

| modelo | emite `tool_calls` | **entrega `content`** | centinela | tiempo |
|---|---|---|---|---|
| `qwen3:14b` | 30/30 | **17/30** | 17/30 | 107 s |
| `qwen3:14b-nothink` | 30/30 | **30/30** | 30/30 | 34-36 s |

El tiempo es reloj de pared sobre las 30 repeticiones y se publica como rango
porque se midió tres veces y se movió: 34 s, 36 s, 34 s. Las tasas no se
movieron ni una casilla.

Apagarlo sale gratis en capacidad y **paga en tiempo**: tres veces más rápido
sobre las mismas 30 repeticiones. Con `n` alto, eso decide cuánto experimento
cabe en una tarde.

### Lo que cuesta apagarlo, y hay que declararlo

Apagar el razonamiento **cuesta iniciativa**. Batería completa de la sonda sobre
`qwen3:14b-nothink`, `n=30`:

| redacción | emite |
|---|---|
| techo (se le nombra la herramienta) | 30/30 |
| «**averigua** cuál es el error» | 30/30 |
| «arréglalo» (con contexto) | 0/30 |
| «arregla el fichero, que sus tests fallan» | 0/30 |
| «los tests de `tarifa.py` fallan» | 0/30 |
| **suelo** (un saludo, sin tarea) | **0/30** |

Con el razonamiento encendido, el uso de herramientas de este modelo se movía
con la redacción. Apagado se vuelve **binario**: o se le nombra la herramienta o
se le dice «averigua», o no llama.

Para el nivel 1 eso no estorba —los cuatro harnesses nombran sus herramientas en
el system prompt, y todos comparten el mismo modelo—, pero **es un confundidor
más y viaja declarado**. Un harness cuyo prompt de sistema se parezca más a
«averigua» que a «arregla» parte con ventaja, y ese es precisamente el efecto que
[#29](https://github.com/pichu2707/OxideGate/issues/29) quiere medir en vez de
sufrir.

De paso, este perfil destapó un fallo de la guarda del suelo: la sonda descartaba
al modelo **por tener el suelo limpio**. Ver
[`fe-de-erratas.md`](fe-de-erratas.md), E-012.

---

### El otro candidato, y por qué no hay tercero

`qwen2.5-coder:14b` está **descartado**: produce el JSON de la llamada perfecto
pero sin las etiquetas `<tool_call>`, así que la plantilla nunca lo convierte y
ollama entrega `null`. 30/30 pseudollamadas. No es reparable desde fuera.

---

## 3. Dónde se apaga, y por qué en el modelo

Hay tres sitios. Dos son trampas, y las dos parecen razonables:

| dónde | funciona | por qué NO |
|---|---|---|
| en la **petición** (`think:false`, `reasoning_effort:"none"`) | sí | un harness no manda ese campo; hacérselo inyectar a OxideGate mete al instrumento de medida dentro del experimento |
| en la **config de cada harness** | a veces | el modo de razonamiento pasaría a depender del harness — **el confundidor exacto que el nivel 1 existe para quitar** |
| en el **modelo** | sí | es constante para los cuatro, se declara en el informe, y ninguno de los cuatro sabe que existe |

`PARAMETER think false` **no existe** en el Modelfile de ollama 0.30.10
(`Error: unknown parameter 'think'`). Así que se hace parcheando la plantilla:
`/no_think` incondicional en el último mensaje de usuario, y el prefill
`<think></think>` incondicional en el turno del asistente, y el bloque que
reinyecta el `thinking` de turnos anteriores, apagado. **Tres ediciones.**

La tercera faltaba en el primer intento —«con el razonamiento apagado esa rama no
llega a renderizar»— y la cazó la revisión: era cierto por el camino normal, pero
dejaba la decisión en manos de la petición. Un derivado que se llama `-nothink` no
puede tener un `if` que dependa de lo que le manden.

### La ventana de contexto va en el mismo sitio, y por lo mismo

`PARAMETER num_ctx 32768`, también en el modelo. No es un ajuste de
rendimiento: **es una condición para que el nivel 1 mida algo.**

`qwen3:14b` declara 40960 de contexto, pero un modelo sin `num_ctx` recibe el
defecto del servidor —**4096** en ollama 0.30.10— y el prompt **se corta en
silencio**: la petición sale `200`, no hay aviso, y el modelo contesta a lo que
le quedó. El prompt de un harness real no cabe ahí:

| | `input_tokens` de Codex |
|---|---|
| `num_ctx` por defecto | **4095** |
| `num_ctx` 32768 | **6485** |

Codex manda ~6500 tokens —system, encargo y 20 KB de declaraciones de
herramientas—, así que se tiraba el 37% del estímulo con las herramientas
dentro. La primera corrida del corredor dio 0/3 por esto. Ver
[`fe-de-erratas.md`](fe-de-erratas.md), E-013.

La tabla de arriba se aplica igual, palabra por palabra: en la petición no —un
harness no manda `num_ctx`—; en el servidor tampoco —`OLLAMA_CONTEXT_LENGTH`
depende de cómo arranque ollama cada quien, y eso es un confundidor que viaja
sin declarar—; en el modelo sí.

Tras re-derivar, la sonda se volvió a pasar a `n=30` y dio **exactamente lo
mismo** que la tabla de §2: techo 30/30, `averigua` 30/30, el resto 0/30, suelo
0/30. Ampliar la ventana no mueve nada de lo que la sonda mide.

### Es un confundidor declarado, no una trampa

El derivado **no es** `qwen3:14b`. Es `qwen3:14b` en una configuración concreta,
y el informe del nivel 1 tiene que decirlo así. Lo que lo hace legítimo es que
esa configuración es **idéntica para los cuatro harnesses**: sigue siendo el
modelo la constante y el harness la única variable, que es la propiedad entera
del nivel 1.

Lo que NO sería legítimo es lo contrario —dejar que cada harness negociara su
propio modo de razonamiento— y es justo lo que pasa si no se fija en el modelo.

---

## 4. Cómo se deriva

```sh
cargo run --example derivar-nothink
# NOTHINK_BASE=qwen3:14b  NOTHINK_DESTINO=qwen3:14b-nothink
```

**Se parchea por anclas, no por número de línea, y aborta si un ancla no aparece
exactamente una vez.** Una plantilla que cambió bajo los pies —otra versión de
ollama, otro modelo base— tiene que detener la derivación, no producir en
silencio un modelo distinto del que se midió. Mismo criterio que las guardas de
`calibrar.rs` y de `sonda-herramientas.rs`.

Comprobarlo no es trabajo del derivador. Lo dictamina la sonda:

```sh
SONDA_MODELOS=qwen3:14b-nothink SONDA_N=30 cargo run --example sonda-herramientas
```

---

## 5. Lo que esto NO demuestra

**Sigue siendo la criba barata.** Un turno, una herramienta simulada, y la
redacción en la que se le NOMBRA la herramienta. Lo que hundió el primer intento
de #121 fue `pi` con SU system prompt y SUS cuatro herramientas durante seis
turnos, y eso no se sintetiza.

Esto abre la puerta; **no la cruza**. La puerta sigue siendo el corredor real,
como dejó escrito el propio issue.

---

## Ver también

- [`banco-de-tareas.md`](banco-de-tareas.md) — la otra condición: que el modelo
  sepa resolver la tarea
- [`fe-de-erratas.md`](fe-de-erratas.md) — E-004 y E-011, las dos veces que el
  `content` vacío de este modelo se leyó mal
- [`banco-de-captura.md`](banco-de-captura.md) — las recetas de aislamiento por
  harness, que son las que hacen posible el nivel 1
