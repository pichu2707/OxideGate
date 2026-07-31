# Agregación por sesión — `GET /sessions`

> `GET /stats` responde *"¿cuánto me costó este modelo?"*. Para quien corre
> varios agentes a la vez esa no es la pregunta: el gasto por modelo **no dice
> quién lo generó**. Este endpoint responde *"¿cuánto costó esta sesión?"*.

> **Desde la rehidratación del histórico, este agregado NO empieza vacío**: al
> arrancar se relee `telemetry.jsonl` dentro de una ventana (7 días por
> defecto). `GET /history` dice desde cuándo mide — ver
> [`history-rehydration.md`](history-rehydration.md).

---

## 1. Forma

```json
{
  "sessions": [
    { "source": "explicit", "key": "sesion-A", "is_session": true,
      "requests": 3, "input_tokens": 36, "output_tokens": 9,
      "cache_read_tokens": 0, "cost_usd": 0.001215 },
    { "source": "unattributed", "key": "curl/8", "is_session": false,
      "requests": 1, "input_tokens": 12, "output_tokens": 3,
      "cache_read_tokens": 0, "cost_usd": 0.000405 }
  ],
  "saturated": false
}
```

La clave de sesión se resuelve por precedencia de cabeceras; el contrato
completo está en [`telemetry-per-request.md`](telemetry-per-request.md) §4.6.

---

## 2. Se agrega por `(source, key)`, nunca por `key` sola

**Es la decisión que hace honesto el endpoint.** La misma clave bajo distinto
origen significa cosas distintas:

| `source` | Qué es la `key` |
|---|---|
| `explicit` | Etiqueta que puso quien invoca (`X-OxideGate-Session`) |
| `native` | Identificador de sesión real del harness |
| `unattributed` | El `User-Agent`, **no una identidad** |

Agrupar `claude-cli/1.0` como `native` con `claude-cli/1.0` como
`unattributed` fusionaría **una sesión concreta** con **todas las sesiones no
atribuidas de ese harness**. El total parecería una sesión y serían muchas.

### `is_session` dice cuál es cuál

Las filas con `source: "unattributed"` llevan **`is_session: false`**. No son
una sesión: son un **cubo de fallback**. Sin esa marca, un consumidor las
trata como una más y suma peras con cajas de peras.

> Si toda tu fila de mayor gasto es `is_session: false`, no has descubierto una
> sesión cara: has descubierto que **ese tráfico no está atribuido**. La
> solución no es leer el número, es estampar el header — ver
> `telemetry-per-request.md` §4.6 para cómo hacerlo en cada harness.

---

## 3. La cota, y por qué satura en vez de crecer

`X-OxideGate-Session` es una cabecera **controlada por quien llama**. Un mapa
sin cota keyeado por ella es un vector de crecimiento de memoria en el camino
crítico — el mismo riesgo que ya acotan `MAX_DISTINCT_PROMPTS_PER_MODEL` y el
tope de servidores MCP.

Al llegar a **10.000 sesiones distintas** se dejan de admitir claves NUEVAS;
**las ya conocidas siguen sumando**. Y el snapshot lo declara:

```json
"saturated": true
```

Con `saturated: true` las filas son una **cota inferior honesta**: falta
tráfico de claves que no se admitieron. No es un número inflado ni un OOM.

---

## 4. Por qué un endpoint aparte y no un campo en `/stats`

`GET /stats` devuelve un **array**, y el monitor lo deserializa como tal.
Convertirlo en un objeto `{by_model, by_session}` rompería a todo consumidor
existente por un cambio de forma.

Un endpoint hermano es **aditivo**: una build anterior devuelve 404, que es un
"no lo tengo" inequívoco, no un dato mal interpretado.

---

## 5. Lo que NO hace todavía

- **Panel de sesión en el monitor TUI.** Los datos están; la vista no.
- **No hay ventana temporal**: el agregado es desde que arrancó el proceso. No
  se puede pedir "las últimas 2 horas".
- **No persiste**: se pierde al reiniciar, igual que `/stats`. El histórico
  vive en `telemetry.jsonl`, que sí lleva `session` por fila.

---

## Ver también

- [`telemetry-per-request.md`](telemetry-per-request.md) §4.6 — cómo se
  resuelve la clave y cómo estamparla desde cada harness
- [`telemetry-by-model.md`](telemetry-by-model.md) — el otro eje de agregación
