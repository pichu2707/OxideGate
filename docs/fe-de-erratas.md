# Fe de erratas

Este proyecto publica números. Un proyecto que publica números tiene que
publicar sus retractaciones **con la misma visibilidad**, o la corrección se
queda enterrada en un hilo y el dato falso sigue circulando.

Este fichero es el registro de las afirmaciones que se publicaron y luego
resultaron falsas: qué se dijo, dónde, qué es cierto, cómo se midió, y qué
cambió al saberlo.

## Las reglas

**1. El texto equivocado no se borra.** Se anota y se enlaza a la corrección.
Borrarlo escondería que se cometió el error, y el camino que llevó a él suele
ser más instructivo que la conclusión. En GitHub eso significa un aviso al
principio del comentario original y la corrección como comentario aparte.

**2. La corrección lleva la medición, no la opinión.** Una entrada sin números
ni forma de reproducirlos no es una corrección: es un cambio de parecer.

**3. Se dice también qué SIGUE en pie.** Casi ninguna afirmación falsa lo es
entera. Separar la parte buena de la mala evita tirar trabajo válido.

**4. Antes de cerrar una entrada, se busca la afirmación por todo el repo.**
`grep` en código, `docs/` y README. De poco sirve corregir un issue si el
doc-comment de un `example` sigue imprimiendo la cifra retractada — ocurrió con
la E-005.

## Qué entra aquí

Afirmaciones **publicadas** —en issues, `docs/`, README o doc-comments— que
resultaron falsas. Los bugs de código van a sus PR; los fallos de instrumento
entran **solo si llegaron a producir un número que alguien pudiera citar**.

---

## E-001 · «`qwen2.5:7b` no emite llamadas a herramientas»

- **Publicado**: [#121](https://github.com/pichu2707/OxideGate/issues/121), comentario del 2026-08-15
- **Corregido**: 2026-08-16 — [corrección](https://github.com/pichu2707/OxideGate/issues/121#issuecomment-5307039329)

Se afirmó, con tres niveles de comprobación y en negrita, que el modelo *«no
emite llamadas a herramientas válidas»* y que *«no es `pi`, no es OxideGate, es
el modelo»*.

**Es falso.** Reproducido en dos ejecuciones independientes: emite `tool_calls`
bien formada **5/5** con el prompt imperativo, y usa el resultado devuelto
**5/5**. También por `curl` directo, y por los dos endpoints —`/api/chat` y
`/v1/chat/completions`—, así que el endpoint tampoco era la variable.

**Qué pasó**: la prueba aislada de aquel día usó **un encargo, no una orden**, y
con `n=1`. Aquel `tool_calls: null` era una muestra de la redacción que menos
uso de herramientas provoca, leída como incapacidad categórica.

**Sigue en pie**: el cableado por `OPENAI_API_BASE` funciona y el proxy mide; el
modelo no condujo el harness de `pi` en seis turnos; resuelve la tarea 4/10 en
el suelo de un turno (#120).

**Qué cambió**: la explicación. No es que no sepa emitir llamadas — es que no
elige usar herramientas de forma fiable cuando nadie se las nombra. Eso cambia
el criterio para elegir un modelo candidato, y por tanto lo que hay que medir.

---

## E-002 · «~10 GB libres de VRAM»

- **Publicado**: [#121](https://github.com/pichu2707/OxideGate/issues/121), mismo comentario
- **Corregido**: 2026-08-16

Se describió el hardware como *«RTX 4080 SUPER, 16 GB, ~10 GB libres»*, y sobre
esa cifra se construyó la opción A del desenlace: *«en 10 GB cabe un 14B
cuantizado»*.

`nvidia-smi` reporta **15.433 MiB libres de 16.376**. Son ~15 GB, no 10.

**Qué cambió**: el espacio de candidatos era bastante más ancho de lo que decía
el issue. Menor en apariencia, pero era un dato de partida para una decisión.

---

## E-003 · La sonda medía obediencia y la llamaba iniciativa

- **Publicado**: `examples/sonda-herramientas.rs`, primera versión (nunca commiteada)
- **Corregido**: 2026-08-16, el mismo día

La sonda nació con un nivel llamado «iniciativa» que aprobaba o suspendía a un
modelo según si usaba la herramienta ante un encargo. Su prompt **nombraba la
herramienta**, así que medía si el modelo obedecía una orden — no si tomaba una
decisión.

Lo cazó el control negativo: `qwen2.5:7b`, el modelo que #121 había descartado,
aprobó **5/5**. La guarda abortó sin publicar nada.

**Qué cambió**: el nivel de aprobado/suspenso desapareció. En su lugar hay una
batería de redacciones del mismo encargo, ninguna de las cuales nombra la
herramienta, y se publica el **rango**. Un test
(`solo_el_techo_nombra_la_herramienta`) impide que la orden vuelva a colarse.

**La lección**: un umbral construido sobre un prompt escrito a mano mide a quien
lo escribió.

---

## E-004 · «`qwen3:14b` ignora el resultado de la herramienta, 5/5»

- **Publicado**: salida de `examples/sonda-herramientas.rs`, 2026-08-16
- **Corregido**: 2026-08-16, antes de citarlo en ningún sitio

La tabla dio `usó 0/5, ignoró 5/5` y se estuvo a punto de anotar como propiedad
del modelo.

**Era del instrumento.** El modelo había leído el resultado perfectamente y
dejaba el valor en el campo `thinking`; el clasificador solo miraba `content`.
Se descartó además la hipótesis del formato del mensaje de herramienta: con
`tool_name`, pelado y con `tool_call_id` el resultado es idéntico.

**Qué cambió**: existe un veredicto propio, `PensoSinContestar`, que no lo
mezcla con `Uso` ni con `Ignoro`. No cuenta como `Uso` **porque un harness
consume `content`**: si viene vacío, al agente no le llega nada aunque el modelo
lo supiera. Eso sigue siendo un dato real sobre su viabilidad.

---

## E-005 · «60 puntos por una palabra»

- **Publicado**: doc-comment y salida por pantalla de `examples/sonda-herramientas.rs`
- **Corregido**: 2026-08-16, el mismo día

Se publicó que la redacción del encargo movía el uso de herramientas *«60 puntos
por una palabra»*, comparando 2/5 contra 5/5 con `n=5`.

La misma batería corrida **dos veces sin cambiar nada** dio:

| redacción | 1.ª | 2.ª |
|---|---|---|
| averigua | 5/5 | 5/5 |
| arréglalo | 4/5 | 3/5 |
| **arregla (seco)** | **1/5** | **3/5** |
| **rango publicado** | **1-5/5** | **3-5/5** |

Con tasas cerca del 50%, el error de muestreo de cinco tiradas es del mismo
tamaño que el efecto. El rango, que ES el resultado que publica la sonda, se
movía solo.

**Sigue en pie**: que la redacción mueve el uso de herramientas. Pero ver la
E-008: la forma concreta en que se salvó esta parte también estaba mal.

**Qué cambió**: existe `N_MINIMO_PUBLICABLE = 30`, la sonda **avisa por pantalla**
cuando se corre por debajo, y un test impide bajar el umbral a la zona que
resultó inestable.

**La cifra buena**, ya con `n=30` sobre los dos modelos:

| redacción | `qwen2.5:7b` | `qwen3:14b` |
|---|---|---|
| averigua | **30/30** | **30/30** |
| arréglalo (con contexto) | 24/30 | 26/30 |
| arregla (seco) | 25/30 | 25/30 |
| constatación (sin petición) | 24/30 | 20/30 |
| **rango** | **24-30/30** (20 pts) | **20-30/30** (33 pts) |

De 60 puntos a 20-33 según el modelo. El efecto existe y es grande; el número
publicado era casi el doble del real.

**El agravante**: el aviso ya estaba escrito en este repo. `calibrar.rs` usa
`n=10` por defecto y documenta por qué el `n>1` no es decorativo.

---

## E-006 · «Los 6 commits del backup son gemelos del rebase»

- **Publicado**: en conversación, durante la limpieza de ramas del 2026-08-16
- **Corregido**: el mismo día, antes de borrar nada

Al limpiar ramas locales se afirmó que `backup/monitor-antes-del-rebase` era
redundante: sus seis commits tenían los mismos asuntos que seis de `main`, luego
eran las versiones pre-rebase y la rama se podía borrar.

**Cinco lo eran. El sexto no.** `git cherry` lo marcó, y la comprobación de
contenido encontró **12 líneas que `main` no tiene en ninguna parte**: los
doc-comments con la medición de que el 92% del tiempo de una petición fría es
carga y no inferencia.

**Qué cambió**: la rama no se borra ([#125](https://github.com/pichu2707/OxideGate/issues/125)),
y el criterio de borrado pasó de una comprobación a tres, documentado en
[`limpieza-de-ramas.md`](limpieza-de-ramas.md).

---

## E-007 · El ancla del suelo hacía que medir mejor empeorase el veredicto

- **Publicado**: `examples/sonda-herramientas.rs`, guarda de anclas
- **Corregido**: 2026-08-16, el mismo día

El ancla inferior exigía `suelo == 0`: si el modelo llamaba a la herramienta
ante un saludo aunque fuera una vez, se descartaba.

`qwen2.5:7b` da **0/5** ante el saludo y **1/30** —con la ruta inventada
`/path/to/your/file.txt`—. Con la regla binaria **aprobaba a `n=5` y suspendía a
`n=30`, siendo el mismo modelo**. El veredicto dependía de cuánto midieras, que
es la definición de un ancla que no ancla nada.

Peor: empujaba a medir poco. Cuanto más riguroso el `n`, más modelos caían por
una llamada suelta.

**Qué cambió**: el ancla ya no exige pureza sino **discriminación**. El suelo
tiene que quedar por debajo de la mitad del encargo más flojo. Un 3% de llamadas
a ciegas contra encargos al 67-100% discrimina de sobra; un modelo que llame
igual al saludo que al encargo, no. Cuatro tests nuevos lo fijan, incluido el
caso de la mitad exacta, que no pasa.

---

## E-008 · «`arregla (seco)` sale siempre por debajo»

- **Publicado**: doc-comment de `examples/sonda-herramientas.rs`
- **Corregido**: 2026-08-16, horas después

Al retractar la E-005 se conservó lo que parecía la parte sólida: que la
dirección del efecto estaba establecida, con `averigua` arriba y `arregla
(seco)` abajo.

La mitad de la cola era falsa. Con `qwen3:14b` a `n=5`, `arregla (seco)` sacó
**5/5, empatado en cabeza**; a `n=30` el último fue `constatación` (20/30), no
`arregla (seco)` (25/30).

Con `n=30` sobre los dos modelos, `arregla (seco)` da **25/30 en ambos** y no es
el último en ninguno.

**Sigue en pie**: la cabeza y, ahora sí medido, la cola. `averigua` sale **30/30
en los dos modelos** —al máximo—, y `constatación` queda abajo o empatada abajo
en los dos. Lo que no se puede ordenar es lo de en medio.

**La lección**: al retractar una afirmación, la parte que uno decide salvar
merece la misma comprobación que la que tira. Aquí se salvó de más, y encima el
`println!` del programa siguió imprimiendo la versión falsa hasta que la regla 4
lo cazó — el mismo fallo que la E-005, dos veces en el mismo fichero.

---

## El patrón, que es lo más útil de esta lista

De las ocho entradas, **cinco fallan hacia el mismo lado**: E-001, E-003, E-004,
E-005 y E-007 son casos en los que el instrumento le cargó al modelo un defecto
propio, publicó azar como si fuera señal, o lo suspendió por una regla suya mal
puesta.

Un banco de medida no se equivoca al azar. Se equivoca **hacia donde le resulta
cómodo**, y aquí lo cómodo siempre fue creer que el examinado era malo antes que
revisar el examen. Es más fácil escribir «este modelo no sabe» que «mi
clasificador solo miraba un campo».

Las otras tres comparten causas distintas y también repetidas: **dar por buena
una cifra o un criterio sin comprobarlo** porque parecía evidente (E-002, E-006),
y **salvar de más al retractar** — quedarse con la parte de una afirmación falsa
que parecía sólida, sin volver a comprobarla (E-008).

**Ninguna de las ocho la encontró un test en rojo.** La suite estuvo en verde
todo el tiempo, porque ninguna era un fallo de código. Las cazaron cosas
distintas, y ninguna es automática:

| cómo se cazó | entradas |
|---|---|
| una **guarda** que se negó a publicar | E-001, E-003 |
| **medir** en vez de fiarse del dato heredado | E-002 |
| **desconfiar de un resultado demasiado limpio** (`5/5 ignoró`) | E-004 |
| **repetir** la misma medición | E-005 |
| **comprobar el contenido**, no el recuento | E-006 |
| **subir el `n`** y ver cambiar un veredicto que no debía cambiar | E-007, E-008 |

De ahí que las guardas de este proyecto **aborten en vez de avisar**: un aviso se
ignora, y aquí hicieron falta dos abortos para descubrir que el instrumento
estaba mal planteado.

Y de ahí también la regla que no se puede automatizar: **un resultado redondo
merece más desconfianza que uno feo.** `5/5 ignoró` era un fallo del clasificador
disfrazado de propiedad del modelo, y `5/5 emitida` era una medida correcta que
demostraba que la sonda medía lo que no era. Ninguna de las dos significaba lo
que aparentaba.
