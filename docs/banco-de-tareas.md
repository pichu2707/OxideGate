# El banco de tareas — comparar herramientas sobre la MISMA tarea

> Calibrado el **2026-08-15** contra `ollama` local. Coste: **cero**. Es la
> primera rodaja de [#29](https://github.com/pichu2707/OxideGate/issues/29).

---

## 1. Qué decide este documento

[#29](https://github.com/pichu2707/OxideGate/issues/29) quiere comparar
herramientas sobre la misma tarea. Ese experimento se hace en **dos niveles**, y
el primero solo existe si un modelo local es capaz de resolver la tarea:

- **Nivel 1** — los cuatro harnesses contra el **mismo modelo local**. El modelo
  deja de ser un confundidor y el harness es la única variable. Coste cero,
  así que `n` puede ser alto.
- **Nivel 2** — cada herramienta con **su** modelo, como se usa de verdad. Mide
  la realidad, con confundidores declarados.

Si el modelo local no resuelve la tarea, el nivel 1 **no mide nada**: mide
cuánto gasta cada harness dando vueltas antes de rendirse. Este documento es la
comprobación de que eso no pasa.

Resolver la tarea es **la primera** de las dos condiciones del nivel 1. La
segunda —que el modelo sepa además operar herramientas y **entregar** lo que
averigua— vive en [`modelo-del-nivel-1.md`](modelo-del-nivel-1.md), y estuvo a
punto de costar el issue entero.

---

## 2. La tarea

[`tareas/reparar-tarifa/`](../tareas/reparar-tarifa/). `test_tarifa.py` falla;
hay que hacer que pase. Se eligió con tres condiciones:

1. **Veredicto binario y objetivo.** El runner sale 0 o no sale 0. El camino no
   es determinista; **el veredicto sí**.
2. **Dos defectos, no uno.** Uno de escala —la división es por mil y los precios
   vienen por millón— y otro de omisión —`tokens_cache` se recibe y no se usa—.
   Con un solo defecto de un carácter, la tarea se resuelve sin leer los tests y
   deja de discriminar.
3. **ASCII puro.** El contenido viaja a modelos y lo editan agentes distintos:
   con acentos, la codificación entraría como variable en un experimento que
   mide otra cosa.

El estado inicial **tiene que fallar**. `examples/calibrar.rs` lo comprueba al
arrancar y **aborta** si pasa: una tarea que no falla no mide nada, y cualquier
tasa sacada de ella sería falsa.

---

## 3. Cómo se calibra

```sh
cargo run --example calibrar
CALIBRAR_N=10 CALIBRAR_MODELOS=qwen2.5:7b cargo run --example calibrar
```

**Un solo turno, sin herramientas, con todo el contenido en el prompt.** No hay
agente: se le entrega el fichero roto y los tests, y se le pide el corregido.

Es deliberadamente **más fácil** que la tarea real —un harness tendría que
encontrar los ficheros él solo— y esa es la propiedad que se busca: **si el
modelo no puede con el fichero en la mano, ningún harness lo va a salvar.** Un
suelo que no se pasa cierra la pregunta sin gastar un token de cuota.

Lo contrario **no** se sigue: pasar el suelo no garantiza que un harness llegue.
Eso lo dirá el corredor del nivel 1.

> No se fija la temperatura a 0 a propósito. Con muestreo determinista las N
> repeticiones darían la misma respuesta y el `n>1` que pide #29 sería
> decorativo. Lo que interesa es la **proporción** que resuelve.

---

## 4. El resultado

`n=10`, un turno, `AGENTS.md` fuera de juego:

| modelo | resuelto | fallado | sin código | mezclados |
|---|---:|---:|---:|---:|
| `llama3.2:3b` | **0/10** | 10 | 0 | 0 |
| `qwen2.5:7b` | **4/10** | 6 | 0 | 0 |

**El nivel 1 de #29 existe, con `qwen2.5:7b`.** Y la tarea está calibrada en el
sentido que importa: **discrimina**. Separa un modelo que no puede (0/10) de uno
que puede a veces (4/10), que es exactamente lo que hace falta para que después
separe harnesses.

`llama3.2:3b` queda **fuera** del nivel 1: con 0/10 no mide, satura.

### Lo que este 4/10 NO dice

Es el **suelo**. El corredor del nivel 1 pondrá un harness en medio, que además
tiene que encontrar los ficheros y decidir qué ejecutar. El corredor tiene que
contar con `n` alto — que es gratis aquí.

> [!WARNING]
> **Aquí ponía que la tasa del corredor sería «igual o menor», y es falso.**
> Un harness no solo añade dificultad: añade la capacidad de **iterar** — corre
> los tests y reintenta—, y un turno único no puede. Con `qwen3:14b-nothink`
> medido el 2026-08-30, el suelo de un turno da **30/30** y el corredor da
> **30/30** con `pi` y con `opencode`: igual, y **al techo**.
>
> El 4/10 de arriba es de `qwen2.5:7b`, **otro modelo**. Comparar la tasa del
> corredor contra un suelo medido sobre otro modelo no dice nada, y estuvo a
> punto de hacer leer un 30/30 como mérito del harness.
> Ver [`corredor-nivel-1.md`](corredor-nivel-1.md) §3.

---

## 5. Las dos veces que el instrumento mintió, y en qué dirección

Las tres medidas de esta misma tarea y este mismo modelo, en orden:

| medida | `qwen2.5:7b` | qué pasaba |
|---|---:|---|
| primera | 2/10 | el extractor pegaba los dos ficheros |
| segunda | **0/10** | arreglo a medias: seguían colándose las cabeceras |
| tercera | **4/10** | instrumento limpio |

**El error de medición era del mismo tamaño que el efecto medido.** Si se
publica la primera tabla, se publica un número que es la mitad del real y se
concluye que la tarea casi no se puede resolver.

### 5.1. El modelo devolvía los DOS ficheros en un bloque

Pedido «devuelve `tarifa.py` completo», `qwen2.5:7b` devolvía la fuente **y** los
tests pegados dentro del mismo bloque en **4 de 6** respuestas (95-103 líneas,
frente a 23-25 de una respuesta correcta).

Escribir eso como `tarifa.py` da un módulo que **se importa a sí mismo**, falla,
y la fila se contaba como `NoResuelto` — **culpando al modelo de un fallo del
instrumento**. Se detectó comparando dos respuestas: una que pasó y otra que
falló tenían el `coste_usd` byte a byte idéntico.

### 5.2. Y devolvía las cabeceras del propio prompt

La primera versión del prompt separaba los ficheros con `--- fichero ---`. El
modelo **las reproducía dentro del bloque de código**, y `--- tarifa.py ---` no
es Python: `SyntaxError`.

La causa era el formato del prompt, no el modelo. Ahora cada fichero va en **su
propio bloque cercado**, que es como un modelo espera ver código — y la
respuesta natural pasa a ser también un bloque con un solo fichero dentro.
Tras el cambio, **`mezclados` bajó de 6-7 a 0**.

### 5.3. La regla que sale de aquí

**Antes de creerse una tasa, hay que comprobar que lo que falló, falló por lo
que crees.** Un veredicto binario es cómodo justo porque no distingue *por qué*
falló — y esa comodidad es la que esconde los fallos del banco.

En el informe hay ahora dos columnas que existen para eso: `sin código` (no se
supo extraer) y `mezclados` (hubo que limpiar la respuesta). Un banco cuyo
`mezclados` sube es un banco que está midiendo su propio formato.

---

## 6. Reglas que no se saltan

1. **El estado inicial tiene que fallar.** Se comprueba y se aborta si no.
   Un banco que pasa de salida invalida todo lo medido con él.
2. **Cada ejecución parte de una copia limpia.** Sin eso, la segunda repetición
   hereda el arreglo de la primera y la tasa sale inflada.
3. **El veredicto es el código de salida**, no lo que diga por pantalla.
4. **Los fallos del banco se cuentan aparte de los del modelo** (`sin código`,
   `mezclados`). Colapsarlos en «no resuelto» esconde exactamente los errores
   de §5.
5. **La tasa de éxito es un resultado, no un filtro.** Las ejecuciones que no
   resuelven se publican. Una herramienta que resuelve 18 de 20 gastando el
   doble no es peor que una que resuelve 9 de 20 gastando la mitad.

---

## Ver también

- [`modelo-del-nivel-1.md`](modelo-del-nivel-1.md) — qué modelo conduce el
  nivel 1, y por qué su razonamiento va apagado dentro del modelo
- [`banco-de-captura.md`](banco-de-captura.md) — el otro banco, el que mide qué
  inyecta cada harness. Sus recetas de aislamiento por herramienta son las que
  hacen posible el nivel 1.
- [`floor-across-tools.md`](floor-across-tools.md) — el peaje fijo por
  herramienta, que es lo que se resta para obtener el **trabajo real**.
- [`corredor-nivel-1.md`](corredor-nivel-1.md) — el corredor que usa esta
  tarea, qué harnesses la cruzan y cuánto les cuesta.
- [`benchmark.md`](benchmark.md) — la barrida por tamaño de input, con el
  esqueleto de corredor reaprovechable.
