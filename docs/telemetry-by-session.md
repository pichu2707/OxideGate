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
      "cache_read_tokens": 0, "cost_usd": 0.001215,
      "fixed_toll": {
        "instructions": { "bytes": 33716, "seen_in": 3 },
        "hooks":        { "bytes": 12097, "seen_in": 3 },
        "skills":       { "bytes": 14902, "seen_in": 3 }
      } },
    { "source": "unattributed", "key": "curl/8", "is_session": false,
      "requests": 1, "input_tokens": 12, "output_tokens": 3,
      "cache_read_tokens": 0, "cost_usd": 0.000405,
      "fixed_toll": { "instructions": null, "hooks": null, "skills": null } }
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

## 5. `fixed_toll`: lo que cuesta ARRANCAR con esta configuración

Los tres bloques del peaje fijo —`instructions` 48%, `hooks` 29%, `skills`
23%— viajan agregados por sesión. Es la cifra que decide si un plugin vale su
peaje, y no existía: `GET /requests` los publica por petición, pero su buffer
son 200 filas. Medir sin agregar es medir para el momento.

### `bytes` NO es una suma, y esa es la decisión importante

Los tres bloques son el MISMO texto repetido en cada petición de la sesión.
Sumarlos daría un número correcto y engañoso: multiplicaría por 500 un bloque
que se escribió una vez y se cacheó.

Se publica **el valor por petición** y **cuántas peticiones lo trajeron**, y
multiplica quien quiera con el criterio que quiera:

| Pregunta | Cuenta |
|---|---|
| ¿Cuánto pago por arrancar? | `bytes` |
| ¿Cuánto he pagado ya por este bloque? | `bytes × seen_in` |
| ¿Y si sigo a este ritmo? | `bytes × requests` |

`seen_in` puede ser menor que `requests`: el bloque no se reconoció en algunas
filas, o el dialecto no lo publica —`hooks` solo lo trae Anthropic—. Sin ese
número no se sabe si `bytes` se apoya en una muestra o en mil.

### `bytes` es PUNTUAL, no una media

Si cambias el `CLAUDE.md` a mitad de ventana, lo que sirve para decidir es lo
que cuesta AHORA arrancar, no el promedio de dos configuraciones que ya no
conviven. Gana el valor más reciente. Mismo criterio que `tools_declared` en
`GET /mcp`, y por la misma razón.

### `null` no es cero

Un bloque que no se pudo ver no cuesta cero bytes: cuesta un dato que no
tenemos. Tratar el hueco como un cero es el error que documenta
[`telemetry-level-1.md`](telemetry-level-1.md), y aquí daría el consejo
contrario al correcto — un bloque caro pareciendo gratis.

### Por qué aquí y no en `/stats`

Porque un valor representativo solo significa algo si todas las filas que lo
producen comparten configuración, y eso pasa dentro de una **sesión**, no
dentro de un modelo. En `/stats` se mezclarían sesiones con `CLAUDE.md`
distintos y el número no querría decir nada.

---

## 6. Lo que NO hace todavía

- **Panel de sesión en el monitor TUI.** Los datos están; la vista no.

---

## Ver también

- [`telemetry-per-request.md`](telemetry-per-request.md) §4.6 — cómo se
  resuelve la clave y cómo estamparla desde cada harness
- [`telemetry-by-model.md`](telemetry-by-model.md) — el otro eje de agregación
