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

> [!WARNING]
> **Esta entrada es ella misma una errata.** Su conclusión —que el backup
> guardaba 12 líneas que `main` no tenía— es falsa: `main` las tiene, y
> mejoradas. Retractada en la [E-009](#e-009--12-líneas-que-main-no-tiene-en-ninguna-parte).
> Se conserva entera por la regla 1.

- **Publicado**: en conversación, durante la limpieza de ramas del 2026-08-16
- **Corregido**: el mismo día, antes de borrar nada
- **Retractada**: 2026-08-24, [E-009](#e-009--12-líneas-que-main-no-tiene-en-ninguna-parte)

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

## E-009 · «12 líneas que `main` no tiene en ninguna parte»

- **Publicado**: [#125](https://github.com/pichu2707/OxideGate/issues/125),
  [`limpieza-de-ramas.md`](limpieza-de-ramas.md) y la E-006 de este mismo fichero
- **Corregido**: comprobado el 2026-08-23, publicado el 2026-08-24

La E-006 acertó en lo importante —el recuento de commits no basta para borrar una
rama— y erró en su conclusión. Dijo que `backup/monitor-antes-del-rebase`
guardaba una medición que `main` había perdido en el rebase de #109–#113. **No se
perdió: fue retractada**, siete días antes de que nadie mirase esa rama.

Quien la retracta es `64063bf docs: poner la documentacion al dia con ollama y
con los vatios` (2026-08-09). Está en `main` y **no** está en el backup:

```sh
$ git merge-base --is-ancestor 64063bf main                              # cierto
$ git merge-base --is-ancestor 64063bf backup/monitor-antes-del-rebase   # falso
```

Sustituye el 92% por un rango en los tres sitios exactos donde vivía:

```diff
-/// cargar el modelo sin que nada lo diga. Medido: **el 92%** del tiempo de una
-/// petición fría fue carga, no inferencia.
+/// Cuánto pesa esa carga depende por completo de cuánto se genere: medido
+/// entre el **54%** del tiempo (200 tokens) y el **98%** (un token). Por eso no
+/// hay un número que valga como constante — hay un aviso.
```

Vivos hoy en `main`: `src/bin/monitor.rs:208`, `:2819` y `:6036`.

**`main` es estrictamente mejor.** El 92% es un punto suelto presentado como
constante. El 54%–98% es un rango CON la razón de que no exista constante
—depende de cuánto se genere— más el dato de que la ruta **nativa**
(`/api/chat`) sí separa la carga con `load_us`. La misma corrección está en
`README.md`, `docs/findings.md`, `docs/monitor-tui.md`,
`docs/telemetry-per-request.md` y `src/provider/ollama.rs`.

**Las otras líneas tampoco eran únicas.** De las 12, una no era del 92%
(`generation_throughput`, «Estas filas se EXCLUYEN»): `main` la tiene
**ampliada** en `src/bin/monitor.rs:1216-1229`, partida en lista numerada y con
la medición de #102 dentro. El inventario completo del diff: **12 líneas solo en
el backup, 129 solo en `main`**.

**El instrumento, otra vez.** `git grep "92%" main` no devuelve nada, y de ahí
salió «se perdió». Pero un `grep` por una cifra literal **no distingue perder de
retractar**: da exactamente cero en los dos casos. Lo que sí los separa es mirar
el historial en las dos puntas:

```sh
$ git log --oneline -S "92%" main -- src/bin/monitor.rs                     # 4 commits
$ git log --oneline -S "92%" backup/monitor-antes-del-rebase -- src/bin/monitor.rs   # 3 commits
```

Los tres que la introducen están en ambas. El cuarto —el que la quita— solo está
en `main`. **Ese commit de más ES la retractación.**

La pista estaba en el propio #125: decía que la rama «arrastraba bloques de doc
de `docs/al-dia-con-ollama-y-vatios`». Ese es el nombre de la rama de `64063bf`.
La causalidad estaba al revés — el rebase no se comió los docs, el backup es una
foto ANTERIOR a la corrección.

**Qué cambió**: `limpieza-de-ramas.md` pasa de tres comprobaciones a **cuatro**;
la nueva pregunta si el dato ausente fue SUPERADO antes de declararlo perdido.
`backup/monitor-antes-del-rebase` se borra (SHA `0ec8392`) y #125 se cierra sin
rescatar nada: rescatar el 92% habría sido una **regresión documental**,
reintroducir una cifra retractada en un área que ya se retractó una vez.

**Sigue en pie de la E-006**: el recuento de commits no basta, `git cherry` marca
`+` un commit rebasado con conflictos, y la comprobación de contenido es
obligatoria. Todo eso es correcto y sigue en el criterio. Lo único que falla es
la lectura de lo que esa comprobación encontró.

---

## E-010 · «`-d` exige que esté fusionada también en ese upstream»

- **Publicado**: [`limpieza-de-ramas.md`](limpieza-de-ramas.md), sección «Por qué
  `git branch -d` no sustituye a esto» — escrita en `e1f5e0d` y en `main` desde el
  2026-08-16 (PR #127)
- **Corregido**: 2026-08-24, ocho días después, al borrar la rama de la que hablaba

La frase describía el criterio de `git branch -d` como una **suma**: fusionada en
`main` **y además** en el upstream. Es una **sustitución**. Del manual:

> The branch must be fully merged in its upstream branch, **or** in HEAD if no
> upstream was set with `--track` or `--set-upstream-to`.

`or`, no `and`. Si la rama tiene upstream, git comprueba **solo** el upstream y
deja de mirar `main` por completo.

**Cómo se cazó**: borrando `backup/monitor-antes-del-rebase`, la rama de la
[E-009](#e-009--12-líneas-que-main-no-tiene-en-ninguna-parte). Se usó `-d`
primero, como manda el documento, **esperando un rechazo**. Git la aceptó —con
seis commits fuera de `main`— y lo dijo por escrito:

```
advertencia: deleting branch 'backup/monitor-antes-del-rebase' that has been
merged to 'refs/remotes/origin/backup/monitor-antes-del-rebase',
but not yet merged to HEAD
```

**Por qué importa más que un matiz de manual.** El documento vendía `-d` como red
de seguridad: «es el propio git quien certifica cada borrado, no quien escribe el
comando». Con la regla real, una rama de respaldo con remoto propio **siempre está
fusionada consigo misma**, así que `-d` la aprueba siempre, tenga dentro lo que
tenga. La red falla exactamente en la clase de rama para la que se quiere una red.

**El origen del error**: el documento conocía el fallo en UNA dirección —el falso
negativo de `fix/monitor-visibilidad-y-scroll`, rechazada por ir 11 por delante de
su remoto— y **generalizó desde ese único caso** a una regla que lo explicaba. La
regla encajaba con el dato que había y era falsa. Faltaba la dirección contraria,
que es la peligrosa: un falso negativo molesta, un falso positivo borra trabajo.

**Qué cambió**: la sección se reescribe con las dos direcciones, el aviso literal
de git y la guarda `git branch --no-merged main`. Y la conclusión se invierte:
además de «un rechazo de `-d` es una pregunta, no un veredicto», ahora dice que
**una aceptación de `-d` tampoco es un veredicto**.

**Sigue en pie**: usar `-d` antes que `-D` sigue siendo lo correcto — lo que no se
sostiene es tratar su silencio como certificación. Y el criterio de cuatro
comprobaciones queda **reforzado**: este borrado salió bien precisamente porque las
comprobaciones 3 y 4 dictaron sentencia antes de tocar `-d`.

---

## E-011 · «`qwen3:14b` hace esto 5/5», y el `content` vacío como propiedad del modelo

- **Publicado**: doc-comment de `Encadenado::PensoSinContestar` en
  [`examples/sonda-herramientas.rs`](../examples/sonda-herramientas.rs), en `main`
  desde el 2026-08-16 (PR #126). La cifra viene de la
  [E-004](#e-004--qwen314b-ignora-el-resultado-de-la-herramienta-55), que es una
  entrada de esta misma lista
- **Corregido**: 2026-08-25, buscando candidato para el corredor de #121

Fallan dos cosas: la cifra, y lo que se concluyó de ella.

### La cifra era `n=5`

Medido a `n=30` el 2026-08-25, ollama 0.30.10, mismo techo y mismo centinela:

| ruta | mensaje de herramienta | `content` vacío |
|---|---|---|
| `/api/chat` | pelado | 18/30 |
| `/api/chat` | con `tool_name` | 20/30 |
| `/v1/chat/completions` | con `tool_call_id` | 13/30 |

Entre el 43% y el 67%. No el 100%. **«5/5» se lee como «siempre»**, y no lo era.

Es el mismo `n=5` que la E-007 y la E-008 ya habían dejado por incapaz de
sostener una cifra en este banco — y aquí estaba sosteniendo una **desde antes
que ellas**, sin que nadie volviera a mirarla.

De paso, el `n=30` **confirma** lo que la E-004 ya había descartado a mano: el
formato del mensaje de herramienta no mueve nada (18/30 contra 20/30). Esa parte
del trabajo original era correcta.

### Lo que se concluyó, que es lo grave

La E-004 cerró con «eso sigue siendo un dato real sobre su viabilidad». Ese dato
**mató la opción A de #121**: sin candidato local, no había corredor que
escribir, y el issue se quedó ocho días eligiendo entre pagar nube o declarar
que el nivel 1 no existe.

No era una propiedad del modelo. Era una propiedad del modelo **con el
razonamiento encendido**, y se quita. Por `/v1`, sin tocar la petición:

| modelo | emite | entrega `content` | tiempo |
|---|---|---|---|
| `qwen3:14b` | 30/30 | **17/30** | 107 s |
| el mismo, con el razonamiento apagado | 30/30 | **30/30** | 36 s |

**Sigue en pie**, y es casi todo: el fallo del clasificador que cazó la E-004 era
real, `PensoSinContestar` merece existir, y su criterio —no contarlo como `Uso`
porque un harness consume `content`— es el correcto. Lo único que cae es tratar
el síntoma como incurable sin haber probado a apagar la única variable que lo
producía.

**Qué cambió**: el doc-comment publica el `n=30` en vez del `5/5`, la
justificación de por qué la sonda no toca `think` se reescribe —el dato la
invertía—, y **el nivel 1 vuelve a tener candidato**: el mismo modelo con el
razonamiento apagado dentro.

**Cómo se cazó**: subir el `n`. Otra vez. Y el disparador fue desconfiar de un
resultado redondo — el mismo reflejo que cazó la E-004, aplicado esta vez a la
propia E-004.

---

## E-012 · El ancla del suelo suspendía a un modelo con el suelo LIMPIO

- **Publicado**: guarda `suelo_discrimina` de
  [`examples/sonda-herramientas.rs`](../examples/sonda-herramientas.rs), en `main`
  desde el 2026-08-16 (PR #126). Es la regla que la
  [E-007](#e-007--el-ancla-del-suelo-hacía-que-medir-mejor-empeorase-el-veredicto)
  puso para arreglar la anterior
- **Corregido**: 2026-08-25, la primera vez que se midió el candidato del nivel 1

La sonda **descartó** a `qwen3:14b` con el razonamiento apagado —el candidato que
la [E-011](#e-011--qwen314b-hace-esto-55-y-el-content-vacío-como-propiedad-del-modelo)
acababa de rescatar— con este veredicto:

```
DESCARTADO — el SUELO emitió 0/30 llamadas ante un saludo, sin tarea ni
fichero, y el encargo más flojo dio 0/30. (…) este modelo llama demasiado a
ciegas para que la batería discrimine.
```

**El modelo emitió 0/30 ante el saludo.** Acusarlo de «llamar demasiado a ciegas»
es decir exactamente lo contrario de lo que el propio mensaje acababa de imprimir.

### Lo que había medido, que es lo que se perdía

| redacción | emite |
|---|---|
| techo (se le nombra) | 30/30 |
| `averigua` | 30/30 |
| `arréglalo`, `arregla` (seco), constatación | 0/30 |
| **suelo (saludo)** | **0/30** |

30/30 contra 0/30 con el suelo impecable es **la discriminación máxima que esta
batería puede dar**. La guarda la suspendió.

### La causa: `min` sobre los encargos

La regla era `suelo * 2 < min(encargos)`. Con un encargo a 0, eso es `0 * 2 < 0`
—falso—, y el modelo cae aunque otro encargo esté al máximo. La regla trataba «el
encargo más flojo dio 0» como «este modelo no llama nunca», y no era el caso.

**Qué cambió**: el suelo se compara contra **el encargo más flojo DE LOS QUE LO
SUPERAN**. Un encargo que no supera al suelo no está contaminado por él —es
indistinguible de él—, así que no tiene voto. El margen del doble sigue
aplicándose donde hace falta y deja de aplicarse contra tasas que no dicen nada.
El caso que sigue suspendiendo es que **ningún** encargo supere al suelo, ahora
con un mensaje que dice eso y no lo contrario. Tres tests nuevos fijan las
piezas.

### El primer arreglo estaba mal, y lo cazó la revisión

El primer intento trataba **aparte el suelo limpio**: si el suelo era 0, aprobar
mientras algo discriminase. Pasaba todos los tests y **dejaba el fallo a una
llamada de distancia**: con `suelo = 1` y encargos `[30, 0, 0, 0]` la regla volvía
a comparar contra el 0 y volvía a suspender.

Y no es hipotético. La [E-011](#e-011--qwen314b-hace-esto-55-y-el-content-vacío-como-propiedad-del-modelo)
—en este mismo lote— es justo la entrada que establece que estas tasas se mueven
entre ejecuciones idénticas. Ese `1` llega solo.

**Arreglar el borde en vez de la comparación es aplazar la errata, no
corregirla**, y es la tercera forma que toma el mismo defecto en esta ancla.

**Sigue en pie**: todo el razonamiento de la E-007. No exigir `suelo == 0` era
correcto y lo sigue siendo; lo que estaba mal era contra QUÉ se comparaba cuando
el suelo ya era limpio.

### Es la segunda vez que esta ancla falla hacia el mismo lado

La E-007 la cambió porque **medir mejor empeoraba el veredicto** —`qwen2.5:7b`
aprobaba a `n=5` y suspendía a `n=30`—. La E-012 es el mismo defecto con otra
forma: **el modelo con el suelo más limpio de todos los medidos es el único al
que la guarda suspendió**. Una guarda que aborta es lo correcto; una guarda cuyo
criterio no se vuelve a mirar después de arreglarlo, no.

---

## El patrón, que es lo más útil de esta lista

De las doce entradas, **ocho fallan hacia el mismo lado**: E-001, E-003, E-004,
E-005, E-007, E-009, E-011 y E-012 son casos en los que el instrumento le cargó al material que
medía un defecto propio, publicó azar como si fuera señal, o lo suspendió por una
regla suya mal puesta.

Un banco de medida no se equivoca al azar. Se equivoca **hacia donde le resulta
cómodo**, y aquí lo cómodo siempre fue creer que el examinado era malo antes que
revisar el examen. Es más fácil escribir «este modelo no sabe» que «mi
clasificador solo miraba un campo», y más fácil escribir «el rebase se comió la
medición» que «mi `grep` no distingue perder de retractar».

Las otras tres comparten causas distintas y también repetidas: **dar por buena
una cifra o un criterio sin comprobarlo** porque parecía evidente (E-002, E-006,
E-010), y **salvar de más al retractar** — quedarse con la parte de una afirmación
falsa que parecía sólida, sin volver a comprobarla (E-008).

La E-010 afina esa primera causa: no es solo no comprobar, es **generalizar desde
un único caso**. Una regla inventada para explicar la observación que tienes
delante encaja con ella por construcción, y eso no la hace cierta.

**Ninguna de las doce la encontró un test en rojo.** La suite estuvo en verde
todo el tiempo, porque ninguna era un fallo de código. Las cazaron cosas
distintas, y ninguna es automática:

| cómo se cazó | entradas |
|---|---|
| una **guarda** que se negó a publicar | E-001, E-003 |
| **medir** en vez de fiarse del dato heredado | E-002 |
| **desconfiar de un resultado demasiado limpio** (`5/5 ignoró`) | E-004 |
| **repetir** la misma medición | E-005 |
| **comprobar el contenido**, no el recuento | E-006 |
| **mirar el historial del dato** (`log -S`), no solo el dato (`grep`) | E-009 |
| **usar la herramienta y leer lo que responde**, en vez de fiarse de la regla escrita | E-010 |
| **subir el `n`** y ver cambiar un veredicto que no debía cambiar | E-007, E-008, E-011 |
| **leer el mensaje de la propia guarda** y ver que se contradecía | E-012 |

De ahí que las guardas de este proyecto **aborten en vez de avisar**: un aviso se
ignora, y aquí hicieron falta dos abortos para descubrir que el instrumento
estaba mal planteado.

Y de ahí también el reverso, que trae la E-012: **una guarda que aborta también
puede equivocarse**, y cuando se equivoca lo hace con toda la autoridad de un
veredicto. La misma ancla del suelo ha fallado dos veces hacia el mismo lado
—suspender al modelo que se estaba midiendo mejor— con dos reglas distintas. Un
aborto obliga a leer el motivo; nadie obliga a comprobarlo.

Y una novedad incómoda que trae la E-009, y que la E-011 repite: **una
corrección también se retracta.** La E-006 y la E-004 eran entradas de esta misma
lista, escritas con la guardia alta y con la comprobación hecha, y las dos
publicaron algo falso — la primera entera, la segunda en su cifra y en lo que
dedujo de ella. Corregir no vacuna. Lo
único que separa una fe de erratas de un segundo montón de afirmaciones sin
comprobar es aplicarle sus propias reglas —sobre todo la 4— también a lo que ella
misma publica.

Y de ahí también la regla que no se puede automatizar: **un resultado redondo
merece más desconfianza que uno feo.** `5/5 ignoró` era un fallo del clasificador
disfrazado de propiedad del modelo, y `5/5 emitida` era una medida correcta que
demostraba que la sonda medía lo que no era. Ninguna de las dos significaba lo
que aparentaba. Y la E-011 añade el caso peor: un `5/5` que sobrevivió nueve días
**dentro de la propia corrección que lo había cazado**, porque una vez escrita una
entrada de erratas nadie vuelve a mirarla.
