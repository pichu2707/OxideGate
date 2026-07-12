# Velocidad — la otra moneda, y por qué no es la misma palanca que tokens

> Todo lo demás que este proyecto documentó hasta ahora optimiza **tokens**
> (coste, ventana de contexto, rate limits). Esta página es sobre **TIEMPO**.
> Son dos ejes distintos, no dos nombres para lo mismo, y optimizar uno **no**
> optimiza el otro — a veces se oponen: reducir el prefijo ahorra tokens y no
> acelera nada medible (ver §2). Quien busque "cómo hago esto más rápido"
> tiene que leer esta página, no `docs/context-tax.md`.

---

## 1. Dónde está el tiempo

De `docs/context-tax.md` §3, sobre la sesión medida el 2026-07-09:

- Generación (streaming): **82%** del tiempo ocupado.
- TTFT (time-to-first-token): **18%**.

Medición nueva de esta página, sobre las peticiones reales a `claude-opus-4-8`
en `~/.config/oxidegate/telemetry.jsonl` (metodología y población exactas en
§2):

| métrica | mínimo | mediana | máximo |
|---|---|---|---|
| TTFT | 1.237 ms | 1.884 ms | 9.846 ms |
| Throughput de generación (tok/s) | 3,9 | 69,3 | 330,3 |

---

## 2. El TTFT no es una palanca — MEDIDO

Población: las peticiones a `claude-opus-4-8` con `status = 200` en
`telemetry.jsonl`, **excluyendo** los dos sondeos de comparación de
`--effort` de §3 (que se miden aparte, con n=2, y no dicen nada como
muestra) y la única petición rechazada por rate limit (`status = 429`, ver
`docs/context-tax.md` §3) — esa fila no generó ni un token y su "TTFT" de
347 ms mide cuánto tardó el proveedor en devolver el rechazo, no una
respuesta real. Quedan **87 peticiones**.

Correlación de Pearson del TTFT contra cada variable capturada, sobre esas
87 peticiones:

| variable | r |
|---|---|
| prefijo total (`input + cache_read + cache_write`) | +0,10 |
| `cache_write` (escritura en frío) | −0,01 |
| `cache_read` (relectura) | +0,08 |
| `input` fresco | −0,06 |
| `output_tokens` (control: no debería influir) | +0,08 |

Ninguna explica nada — todas quedan por debajo de |r| = 0,11, el mismo orden
que el `output_tokens` de control, que por construcción no debería
correlacionar con nada. El TTFT lo determinan la cola y la carga del
proveedor, que el proxy no ve y no puede tocar.

**Consecuencia que corrige una intuición razonable:** reducir el prefijo
ahorra tokens, ventana de contexto y rate limits, pero NO acelera de forma
medible. El prefill de tokens ya cacheados es barato. Las dos monedas no se
mueven juntas.

> **Corrección.** El mensaje del commit `13023dd` cita un primer cálculo de
> estas correlaciones —prefijo +0,190, `cache_write` +0,010— hecho sobre una
> población que incluía la petición rechazada con `429`. Sobre la población
> limpia de arriba, el prefijo baja a +0,113 y `cache_write` cambia de signo a
> −0,037. La conclusión no se mueve: ninguna variable explica el TTFT. Las
> cifras válidas son las de la tabla, no las del commit.

---

## 3. Las dos palancas de velocidad, y dónde se accionan

**`--effort <nivel>`** (`low`, `medium`, `high`, `xhigh`, `max`). Los tokens
de pensamiento son tokens de salida: se generan, se pagan, y sobre todo se
esperan. Menos `effort` ⇒ menos pensamiento ⇒ menos tiempo de generación.

- Flag del CLI: `claude --effort low -p "..."`, o `--effort` para la sesión.
- MEDIDO: Claude Code envía `output_config: {"effort": "high"}` por defecto.
  Lo confirman dos métodos independientes: el body capturado con un sumidero
  HTTP local lleva esa clave, y la sonda sin ningún flag registra
  `requested_effort = "high"` en `GET /requests`. Con `--effort low`, la misma
  sonda registra `"low"`.

  > **Trampa de lectura del JSONL.** De las 90 filas de `claude-opus-4-8` en
  > `telemetry.jsonl`, solo **2** llevan la clave `requested_effort`: las 88
  > restantes son de builds anteriores al commit `13023dd`, donde el campo
  > todavía no existía. La **ausencia de la clave** significa "el proxy no lo
  > capturaba", no "el cliente no lo envió". Confundir las dos cosas lleva a
  > concluir, falsamente, que Claude Code no manda `effort`. Al filtrar el
  > JSONL, conviene comprobar `'requested_effort' in row` antes de leer su
  > valor.
### 3.1. `--effort low`: no genera más rápido, genera menos — MEDIDO

Tres pares de sondas, misma tarea de razonamiento, mismo modelo, mismo
prompt. La única variable es el `effort`:

| métrica | `high` (media) | `low` (media) | delta | rangos |
|---|---|---|---|---|
| `output_tokens` | 1.279,3 | 1.023,0 | **−20,0%** | sin solape (1.114–1.392 frente a 1.012–1.037) |
| `total_ms` | 21.389,2 | 16.685,7 | **−22,0%** | sin solape (19.916–23.019 frente a 16.406–17.034) |
| `tok/s` | 65,2 | 66,8 | +2,4% | **se solapan** (64–68 frente a 66–68) |

Las dos últimas columnas se leen juntas. El `tok/s` se solapa: **la velocidad
de generación no cambia**. Lo que cae es la cantidad de tokens a generar, y
por eso el turno completo tarda un 22% menos.

El mecanismo es "menos tokens que esperar", no "tokens más rápidos". La
distinción no es académica: quien espere una aceleración de la generación no
verá nada, y quien mida sobre una tarea que no requiere razonar tampoco.

> **Un experimento fallido, conservado porque enseña más que el bueno.** El
> primer intento comparó `high` y `low` sobre "escriba 500 palabras de prosa,
> sin usar herramientas". Resultado: 1.374 frente a 1.376 tokens de salida, una
> diferencia del 0,1%. El `effort` gobierna cuánto piensa el modelo; en una
> tarea que no pide pensamiento, el pensamiento adaptativo no se activa y la
> palanca no tiene sobre qué actuar. Se le puso una palanca a una puerta que ya
> estaba abierta. Para medir `effort` hace falta una tarea que obligue a
> razonar.

El precio no es cero: menos pensamiento es menos razonamiento. La pregunta —si
ese 22% de tiempo se paga en calidad de la respuesta— se dejó explícitamente
sin medir en la primera versión de esta página. Ya está medida para
razonamiento de respuesta cerrada, y el resultado está en §3.2. El resumen: en
ese tipo de tarea, no se detectó coste de exactitud; en tareas abiertas, sigue
sin medir.

### 3.2. ¿Cuesta calidad el `--effort low`? — MEDIDO para respuesta cerrada, y no

§3.1 probó que `--effort low` recorta la generación. Lo que dejó abierto es si
esa poda de pensamiento **empeora la respuesta**. Medirlo exige un criterio de
calidad que no sea una impresión, así que la tarea de prueba se restringió a
problemas de razonamiento con **una respuesta objetiva verificable**: la
calidad se vuelve **exactitud** (acierto/fallo contra un ground-truth), no un
juicio de gusto. Cada ground-truth se calculó por fuerza bruta antes de correr
las sondas — si la respuesta "correcta" está mal, el experimento entero se
corrompe.

Dos baterías, cada problema corrido en `high` y en `low` (misma sonda,
`--strict-mcp-config`, intercaladas para que cualquier deriva temporal afecte a
ambas por igual), corregidas automáticamente contra el ground-truth:

| batería | qué mide | HIGH | LOW |
|---|---|---|---|
| 1 — rutina (15 problemas, incluye trampas system-1: bat-and-ball, ovejas, máquinas) | ¿el modelo cae en atajos si piensa menos? | 15/15 | 15/15 |
| 2 — duros (10 problemas × 3 reps: `3^27 mod 100`, `2^2024 mod 1000`, SEND+MORE=MONEY, conteos por inclusión-exclusión…) | ¿el cómputo sostenido se resiente sin pensamiento? | 30/30 | 30/30 |
| **total (25 problemas, 90 peticiones)** | | **45/45 (100%)** | **45/45 (100%)** |

**Hallazgo 1 — cero coste de exactitud, y la palanca SÍ actuó.** No es el falso
null de §3.1. Allí la palanca no tenía sobre qué actuar (prosa: 0,1% de
diferencia de tokens). Aquí actuó de sobra —agregando las 90 peticiones, `low`
generó un **−26,6%** de `output_tokens` y tardó un **−17,6%** de `total_ms`,
con el `tok/s` solapado (87,4 frente a 84,0), consistente con §3.1—. Pensó
demostrablemente menos y **no falló ni uno** de los 25 problemas. Cero
divergencia entre condiciones.

**Hallazgo 2 — dos límites que impiden generalizar a "la calidad no baja".**

1. **Exactitud no es toda la calidad.** Esto mide problemas con UNA respuesta
   correcta. No mide profundidad en tareas abiertas —código sobre specs
   ambiguas, diseño, cobertura de edge cases—, que es donde "calidad" no es un
   único valor verificable. Esa dimensión, la que más importa para trabajo de
   agente, este método no la toca.
2. **Efecto techo.** Ni los problemas duros rompieron a Opus 4.8 a `low`. No se
   encontró un problema *limpiamente corregible* lo bastante difícil como para
   separar las dos condiciones. La banda donde `low` empezaría a fallar más que
   `high` —si existe— está por encima de lo probado aquí, o en las dimensiones
   subjetivas del límite anterior.

**Consecuencia práctica, sin sobrevender.** Para razonamiento rutinario y de
respuesta cerrada, `--effort low` es ~27% menos generación por el mismo
acierto: gratis en esa banda. Reservar `high` tiene sentido para lo que este
experimento no pudo medir —tareas abiertas y problemas más allá del techo
probado—, no para el conteo o la deducción de cada día. Lo que NO se puede
seguir afirmando es que `low` "recorta profundidad" como coste general: sobre
lo medido, no lo hizo.

> **Salvedad de método.** Batería 1 con n=1 por problema, batería 2 con n=3. El
> 100%/100% con varianza cero significa que no apareció ni un caso borderline,
> no que el intervalo de confianza sea estrecho por potencia estadística. Un
> resultado de "no se detectó diferencia" no es "se probó que no existe": es que
> no la hubo en 25 problemas que abarcan de lo trivial a teoría de números de
> varios pasos.

**Fast mode (`speed: "fast"`)**. Documentado por Anthropic: hasta ~2,5×
más tokens por segundo de salida, a precio premium, sobre Opus 4.8 y 4.7.
Tiene su propio rate limit, separado del estándar.

- En Claude Code se activa con el comando interactivo `/fast`.
- Es la única palanca que atacaría directamente el 82% de §1: acelera los
  tokens en sí, no reduce su número.
- **REQUIERE CRÉDITOS DE API.** Comprobado en este proyecto: con una
  suscripción plana (Max), `/fast` responde que hacen falta créditos y no se
  activa. El precio premium no está cubierto por la cuota mensual. Para quien
  trabaja con suscripción y no con facturación por uso, **fast mode no es una
  palanca accionable**, por mucho que esté documentada.
- No está disponible en Amazon Bedrock, Vertex AI ni Microsoft Foundry
  (documentado por Anthropic; OxideGate no enruta tráfico hacia esos tres
  hoy, así que no hay forma de confirmarlo desde este repo).
- ESTADO EN ESTE PROYECTO: **no observado**, y no por falta de intentarlo.
  `requested_speed` (la clave `speed` de la raíz del body) nunca se envía.
  `served_speed` (`usage.speed` de la respuesta) tampoco llega: sigue siendo
  `-` incluso en las sondas a velocidad estándar. Esto **confirma la
  convención** del proyecto: `None` significa "el proveedor no lo reportó",
  nunca "es estándar". Anthropic solo devuelve `usage.speed` cuando se le
  pide fast mode.

### Cuál de las dos se puede accionar hoy

Solo `--effort`. Y no acelera nada: recorta. Es una palanca de **cantidad**,
no de **velocidad**, y por eso su efecto desaparece en cuanto la tarea no
requiere razonar. La palanca que sí atacaría la velocidad de generación —
fast mode — está detrás de un muro de facturación.

Conviene decirlo sin rodeos, porque es el resultado incómodo de este
documento: de las dos palancas que Anthropic ofrece, una está fuera de
alcance con suscripción plana y la otra funciona pagando en profundidad de
razonamiento lo que ahorra en segundos.

---

## 4. Qué captura OxideGate (implementado, commit `13023dd`)

Tres campos nuevos, expuestos en `GET /requests` y en la vista `Latency`
del monitor (columnas `effort`, `spd_req`, `spd_got`):

| campo | de dónde sale | significado |
|---|---|---|
| `requested_effort` | `output_config.effort` del body | El nivel de esfuerzo pedido |
| `requested_speed` | `speed` de la raíz del body | `"fast"` si el cliente pidió fast mode |
| `served_speed` | `usage.speed` de la respuesta | La velocidad con la que el proveedor sirvió de hecho |

Por qué `requested_speed` y `served_speed` son campos SEPARADOS: el fast
mode tiene su propio rate limit, así que una petición puede pedir `fast` y
ser servida en `standard`. Un solo campo escondería exactamente el fallo
que este par existe para delatar.

`served_speed` está DOCUMENTADO por Anthropic pero NO OBSERVADO todavía en
el tráfico de este proyecto (§3). Un `None` significa "no reportado", nunca
"estándar" — mismo criterio de "ausente ≠ cero" que el resto de la
telemetría (ver `docs/monitor-tui.md` §7.2).

Son dialecto de Anthropic: OpenAI y Gemini devuelven `None` a propósito,
con la razón escrita en el código (`src/provider/openai.rs`,
`src/provider/gemini.rs`).

---

## 5. Cómo medir el antes/después

1. Levante el proxy: `OXIDEGATE_PORT=8899 cargo run --bin oxidegate`
2. Levante el monitor en otra terminal: `OXIDEGATE_PORT=8899 cargo run --bin oxidegate-monitor`
3. Apunte el cliente al proxy: `ANTHROPIC_BASE_URL=http://localhost:8899 claude`
4. Genere tráfico normal y pulse `b` en el monitor para marcar el baseline.
5. Active la palanca: reinicie con `--effort low` (o `/fast`, si dispone de
   créditos de API — ver §3).
6. Genere tráfico equivalente y observe el panel `Δ desde baseline`. `p` abre
   el panel por petición; las columnas `effort`, `spd_req` y `spd_got`
   confirman que la palanca llegó al cable.

**Qué columna mirar, según la palanca.** Confundirlas lleva a concluir que
`--effort` no hace nada:

- Con `--effort`, responden `output_tokens` y `total_ms`. **`tok/s` no se
  mueve** (§3.1): la generación va a la misma velocidad, simplemente hay
  menos que generar.
- Con fast mode, respondería `tok/s`, y `output_tokens` no.

Dos reglas metodológicas más, ambas aprendidas a base de medir mal:

- **Tareas equivalentes, y suficientemente largas.** El `tok/s` de un turno de
  cuatro tokens no dice nada; hace falta salida larga para que el throughput
  signifique algo.
- **La tarea debe ejercitar la palanca.** Medir `--effort` sobre prosa sin
  razonamiento da cero diferencia, y no porque la palanca no funcione (§3.1).

---

## 6. Lo que NO acelera (con puntero a la evidencia)

| descartado | cifra | evidencia |
|---|---|---|
| Reducir el prefijo (tokens) | ahorra coste, no tiempo — r=+0,10 (§2) | esta página, §2 |
| Compresión de bytes (gzip) | el modelo tokeniza el texto descomprimido; solo ahorra ~7 ms de subida en fibra sobre ~280 KB | `docs/findings.md` §E |
| Optimizar el transporte MCP | 0,68 ms de mediana (salto JSON-RPC por stdio) contra un turno real de 11.123 ms | `docs/findings.md` §E |
| El overhead del propio proxy | `prepare_us` va de 43 µs a 15.135 µs — el 0,67% de una petición típica | `docs/findings.md` §E |
| Hilos paralelos | compran reloj de pared, lo pagan en tokens de prefijo por hilo | `docs/context-tax.md` §3, `docs/findings.md` §E |
| `--effort low`, como acelerador | no acelera: el `tok/s` se solapa (64–68 frente a 66–68, n=3). Recorta un 20% de tokens generados, y de ahí sale el 22% de reloj | esta página, §3.1 |
| `--effort low` en tareas sin razonamiento | 1.374 frente a 1.376 tokens de salida: la palanca no tiene sobre qué actuar | esta página, §3.1 |
| `--effort low` como pérdida de exactitud (en respuesta cerrada) | 45/45 = 100% en ambas condiciones sobre 25 problemas, pese a un −26,6% de generación en `low`. No se detectó coste — con el techo y el límite de "exactitud ≠ calidad abierta" anotados | esta página, §3.2 |

---

## Ver también

- `docs/context-tax.md` — descomposición medida de coste y latencia de una sesión real (§3, la base de esta página)
- `docs/findings.md` — qué se probó, qué se descartó y qué se retractó, por conclusión
- `docs/monitor-tui.md` §7.2 — las columnas `effort`, `spd_req`, `spd_got` en el panel de requests recientes
