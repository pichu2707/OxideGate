# Palanca B — forzar `effort` desde el proxy

> **Estado: implementada y APAGADA por defecto.** Es la segunda palanca de
> Nivel 2, y la primera que ahorra tokens en vez de reorganizarlos. Se enciende
> con `OXIDEGATE_FORCE_EFFORT=<nivel>`.

---

## 1. Por qué esta palanca y no otra

El catálogo publica cinco palancas medidas. **Cuatro no las puede aplicar el
proxy**, y no por falta de ganas: por lo que significaría aplicarlas.

| Palanca | ¿Está en el body? | Aplicarla desde el proxy sería |
|---|---|---|
| `mcp-lean.json` | Sí, los esquemas en `tools[]` | **Borrar herramientas** que el agente cree tener |
| `--tools <lista>` | Sí, ídem | Ídem |
| `CLAUDE.md` lean | Sí, en `messages[0]` | **Reescribir las instrucciones** del usuario |
| `disable-model-invocation` | Sí, el listado de skills | **Borrar skills** |
| **`--effort low`** | **Sí, `output_config.effort`** | **Cambiar un parámetro** |

Las cuatro primeras son decisiones de configuración **del cliente**. Recortarlas
en el cable no es optimizar: es mentirle al agente sobre lo que tiene. Pediría
una herramienta que ya no viaja y se comería un error que no entiende.

`effort` es distinto **en naturaleza, no en grado**: es un parámetro de la
petición, como `max_tokens`. Cambiarlo no le quita al agente ninguna capacidad;
le pide que piense menos antes de responder.

Esa tabla no es una preferencia. Es lo que hay en el cable.

---

## 2. Qué ahorra, medido

De `docs/findings.md` y `docs/speed.md` §3, medido antes de esta palanca y sin
cambiar por ella —el proxy no mejora el efecto, solo lo aplica:

| | Efecto |
|---|---|
| Tokens de salida | **−20,0%** (n=3 pares, rangos sin solape) |
| Reloj de pared | **−22,0%** |
| Exactitud | **45/45 = 100%** en `high` y en `low`, sobre 25 problemas de respuesta cerrada (90 peticiones) |

**No acelera: recorta.** El `tok/s` no se mueve. Se tarda menos porque se genera
menos, no porque se genere más rápido.

---

## 3. Qué cuesta, y lo que NO se sabe

Esta sección es la mitad del documento a propósito.

**Lo medido:** cero coste de exactitud sobre razonamiento de **respuesta
cerrada** — problemas con una solución verificable.

**Lo NO medido, y es lo que probablemente estés haciendo:**

- **Tareas abiertas** — código, diseño, redacción. Ahí «exactitud» no es una
  métrica que exista, y el A/B que midió los 45/45 no puede pronunciarse.
- **Problemas por encima del techo probado.** Los 25 problemas tenían una
  dificultad acotada; nada dice qué pasa cuando el problema es más duro que el
  presupuesto de razonamiento recortado.
- **En tareas que no razonan, la palanca no hace nada.** No es que sea barata:
  es que no aplica.

Por eso arranca apagada y por eso el arranque la anuncia en voz alta cuando la
enciendes. Un ahorro del 20% en tokens de salida es real; que no te cueste nada
**solo está demostrado para una clase de tarea que quizá no sea la tuya**.

---

## 4. Cómo se enciende

```sh
OXIDEGATE_FORCE_EFFORT=low oxidegate
```

Niveles válidos: `low`, `medium`, `high`, `xhigh`, `max`. Se normaliza
mayúsculas y espacios.

> **Sube tanto como baja, y es deliberado.** El nombre de la palanca es
> *forzar* un nivel, no *bajarlo*. Configurar uno más alto del que pide tu
> cliente **te costará más**, y el proxy no va a impedírtelo: la fila lo
> declara igual y la decisión es tuya.
>
> No es descuido. El A/B que produjo el −20,0% de este documento necesitó
> forzar `high` y forzar `low` sobre la misma tarea para poder compararlas.
> Una palanca que solo bajara dejaría de servir como instrumento, que es la
> mitad de su valor.

**Falla cerrado.** Un valor que no reconoce —un typo, un `true`, un `1`— deja la
palanca **apagada** y lo dice por `stderr`. El error opuesto —mutar cada
petición por un typo, en un proxy cuya promesa es no tocar nada— no se nota
hasta que ya has medido mal un día entero. Mismo criterio que `OXIDEGATE_HOST`.

Y cuando SÍ se enciende, se anuncia:

```
🔧 Palanca B ACTIVA: se fuerza output_config.effort=low en las peticiones a Anthropic.
   Las filas afectadas llevan effort_forced=low; requested_effort sigue diciendo qué pidió el cliente.
```

---

## 5. Sobrescribe lo que pidió el cliente, y por qué

**Medido:** Claude Code manda `output_config: {"effort": "high"}` **explícito en
cada petición**. Una palanca que solo actuara ante la ausencia del campo no
haría nada nunca contra el cliente principal.

Así que sobrescribe. Lo que la hace honesta no es abstenerse — es que **la fila
publica las dos cosas**:

```json
"requested_effort": "high",
"effort_forced": "low"
```

`requested_effort` se lee **antes** de mutar, así que sigue diciendo lo que
pidió el cliente. `effort_forced` dice lo que subió de verdad. Una fila con los
dos campos avisa de que sus `output_tokens` son del segundo.

**Sin ese par, un ahorro provocado por el propio medidor sería indistinguible de
uno del cliente** — que es el peor fallo que este proyecto puede cometer, y el
único que haría inservible toda la telemetría anterior.

Dos casos en los que no toca nada, y los dos se distinguen de una intervención:

- **El cliente ya pedía ese nivel.** No hay nada que forzar; `effort_forced`
  queda `null` y el body ni se reserializa.
- **`output_config` existe pero no es un objeto.** Entonces no es el dialecto
  que creemos, y meter una clave dentro rompería el request. Se reenvía intacto.

---

## 6. Dónde se ve el ahorro

En la vista **ANTES/DESPUÉS** del monitor, que ya publica **Δoutput_tokens**
(`docs/monitor-tui.md` §3). El flujo es el que la herramienta ya tenía:

1. Toma un baseline con la palanca apagada.
2. Enciéndela y sigue trabajando.
3. La Δ de tokens de salida es el ahorro, **en tu tráfico real**, no el −20,0%
   de este documento.

Que la cifra la ponga tu tráfico y no un README es la diferencia entre medir y
citar.

> **Lo que el TUI todavía no enseña.** `effort_forced` viaja en `GET /requests`
> y en `telemetry.jsonl`, pero el monitor aún no lo pinta: mientras la palanca
> está encendida, la única confirmación visual en vivo es el aviso del arranque.
> El ahorro sí se ve (Δoutput_tokens); la *intervención*, no. Va con el resto
> del hueco de visibilidad en
> [#67](https://github.com/pichu2707/OxideGate/issues/67).

---

## 7. Solo Anthropic

`output_config.effort` es dialecto de Anthropic. En OpenAI y Gemini,
`effort_forced` es siempre `null` — no porque la palanca falle, sino porque el
campo no existe en su dialecto. Ver `docs/telemetry-per-request.md` §4.14.

---

## Ver también

- [`telemetry-per-request.md`](telemetry-per-request.md) §4.14 — el contrato del campo `effort_forced`
- [`speed.md`](speed.md) §3 — la medición original de `--effort`, con su n y sus rangos
- [`findings.md`](findings.md) — la fila resumida, con la renuncia declarada
- [`optimizer-prompt-cache.md`](optimizer-prompt-cache.md) — la palanca A, que reorganiza en vez de recortar
