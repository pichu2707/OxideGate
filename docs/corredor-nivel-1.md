# El corredor del nivel 1 — la misma tarea, harnesses distintos, cuánto cuesta

> Medido el **2026-08-30** contra `ollama` 0.30.10 y `qwen3:14b-nothink`,
> `n=30` por harness. Coste: **cero**. Es la segunda rodaja de
> [#29](https://github.com/pichu2707/OxideGate/issues/29) y cierra
> [#121](https://github.com/pichu2707/OxideGate/issues/121).

`examples/corredor-nivel-1.rs` pone un harness **de verdad** —aislado, sin
credenciales— a resolver la tarea del banco contra el modelo local, y lee la
telemetría que escribió el propio proxy. El modelo es constante; el harness es
la única variable.

---

## 1. El resultado

`pi 0.80.10` y `opencode 1.18.25`, `n=30` cada uno, mismo modelo, misma tarea,
mismo `AGENTS.md`:

| | `pi` | `opencode` | oc/pi |
|---|---:|---:|---:|
| **resuelto** | **30/30** | **30/30** | — |
| peticiones por repetición | 8,0 | 10,2 | 1,27x |
| **`input_tokens` por repetición** | **19.704** | **66.741** | **3,39x** |
| `input_tokens` mediana | 2.485 | 7.074 | 2,85x |
| `input_tokens` pico | 4.480 | 9.901 | 2,21x |
| **`output_tokens` por repetición** | **408** | **473** | **1,16x** |
| `total_ms` por repetición | 8.356 | 12.295 | 1,47x |
| `context_tools_bytes` | 2.900 | 19.518 | **6,73x** |
| `context_system_bytes` | 2.909 | 9.952 | 3,42x |

**La fila que dice más es la de `output_tokens`: 1,16x.** Los dos harnesses
hacen que el modelo genere prácticamente lo mismo — el trabajo es el mismo y se
completó igual. Toda la diferencia de coste está en **lo que el harness manda**,
no en lo que el modelo produce. Eso es exactamente el peaje fijo de
[`floor-across-tools.md`](floor-across-tools.md), ahora medido sobre trabajo real
en vez de sobre una tarea trivial.

---

## 2. Qué harnesses entran, y por qué faltan dos

De los cuatro que #29 quería comparar, **entran dos**:

| harness | estado | `tools_B` | pico tokens |
|---|---|---:|---:|
| `pi` 0.80.10 | **mide** | 2.900 | 4.480 |
| `opencode` 1.18.25 | **mide** | 19.518 | 9.901 |
| Codex 0.142.5 | **bloqueado río arriba** | 20.483 | 6.686 |
| Qwen Code 0.21.7 | **no cabe** | 102.509 | ~33.000 |

### Codex: el modelo emite, el harness no sabe enrutar

Codex **no puede operar herramientas contra el `/v1/responses` de ollama**. El
modelo emite la llamada; el router de Codex la rechaza, 3/3:

```
ERROR codex_core::tools::router: error=unsupported call:
```

con el nombre de la función **vacío**. Contesta entonces «no puedo ejecutar
comandos» y no toca la tarea.

**No es de OxideGate**: comprobado apuntando Codex directamente a ollama, con el
proxy fuera, pasa lo mismo. Y no hay salida por el dialecto: `wire_api = "chat"`
está **eliminado** en 0.142.5 (*«no longer supported»*). Su receta se conserva en
el corredor para poder **reproducir el bloqueo**, no para medir con ella.

> **«Existe» no es «interopera».** Los tres eslabones estaban comprobados por
> separado —Codex habla `responses`, OxideGate lo expone, ollama lo expone— y la
> cadena no funciona. Probar los eslabones no prueba la cadena.

### Qwen Code: no cabe por unos cientos de tokens

Qwen Code inyecta **102.509 bytes** de declaraciones de herramientas, y su
primer turno pide ~33.000 tokens. El techo son 32.768 (§4). Se queda fuera.

**Eso es un resultado de #29, no un fallo del banco**: una herramienta que quema
100 KB en declararse tiene menos sitio para trabajar. Lo que habría que evitar
es que el conjunto **sature en cero**; con dos harnesses resolviendo, no satura.

---

## 3. El 30/30 no es mérito del harness, y hay que decirlo

`calibrar.rs` sobre el **mismo modelo**, `n=30`, un turno y sin herramientas:

| modelo | resuelto | fallado | sin código | mezclados |
|---|---:|---:|---:|---:|
| `qwen3:14b-nothink` | **30/30** | 0 | 0 | 0 |

El modelo resuelve la tarea **sin harness**. Así que el 30/30 del corredor no
mide que el harness conduzca bien: mide que **no estorba**. La tasa de éxito
**no puede discriminar** entre harnesses con este par (modelo, tarea).

**No es un fallo del diseño: es la precondición de la pregunta.** #29 define el
nivel 1 como *«¿cuánto cuesta esta herramienta por hacer el **mismo trabajo**?»*,
y «el mismo trabajo» exige que todos lo completen. El riesgo que #29 declaró era
el contrario —*«mide cuánto gasta cada harness dando vueltas antes de rendirse»*—,
o sea saturar en **0**. Saturar en **1** es la condición experimental limpia:
mismo trabajo terminado, se compara lo que costó.

Lo que **no** se puede hacer es leer el 30/30 como mérito. Corregido también en
[`banco-de-tareas.md`](banco-de-tareas.md) §4, que afirmaba que la tasa del
corredor sería «igual o menor» que el suelo — falso: un harness además **itera**,
y un turno único no puede.

### Que el instrumento discrimina, comprobado y no supuesto

Un corredor que da 30/30 y nunca ha dicho «no» no vale nada. Mismo corredor,
misma tarea, mismo harness, con un modelo que se sabe que no puede:

| modelo | resuelto |
|---|---:|
| `qwen3:14b-nothink` | 30/30 |
| `llama3.2:3b-ctx` | **0/5** — las cinco «no tocó el fichero» |

---

## 4. Las tres guardas, y lo que cazó cada una

Ninguna se salta, y todas **abortan sin publicar nada**.

1. **Estado inicial.** Aborta si la tarea pasa de salida. Un banco que pasa
   invalida todo lo medido con él.
2. **Proxy.** Manda una petición real y aborta si no enruta al modelo local. Una
   instancia apuntando a la nube contesta igual de bien, mediría otro modelo y
   **gastaría cuota**.
3. **Contexto.** Manda un prompt deliberadamente enorme: lo que el proveedor dice
   haber leído **es** el techo efectivo, medido en vez de supuesto.

La tercera es la cara: `ollama` aplica su `num_ctx` por defecto —4096— aunque el
modelo declare 40960, y **corta el prompt en silencio**. La primera corrida dio
0/3 con el prompt cortado a 4095. Ver [`fe-de-erratas.md`](fe-de-erratas.md),
E-013, y [`modelo-del-nivel-1.md`](modelo-del-nivel-1.md) §3.

Y por repetición se marca `PromptTruncado` si alguna petición llega pegada al
techo: es la **única** firma del truncamiento, porque ollama no avisa, no cambia
el código de estado y no toca el cuerpo.

### El techo es de hardware

Medido sobre una RTX 4080 Super de 16 GB:

| `num_ctx` | tamaño | dónde corre |
|---|---:|---|
| 32.768 | 14 GB | **100% GPU** |
| 40.960 | 16 GB | 10% CPU / 90% GPU |

32.768 es el máximo con el que un 14B entra entero en GPU. Subir al máximo que
declara el modelo desborda y multiplica los tiempos.

---

## 5. Los veredictos no se colapsan

Seis, y **tres no son del modelo**:

| veredicto | de quién es |
|---|---|
| `resuelto` | — |
| `no resuelto` | del modelo: editó y siguen fallando |
| `no toco el fichero` | del modelo: llegó y no tocó la tarea |
| `timeout` | ni una cosa ni la otra |
| **`sin peticiones`** | **del banco**: cableado o aislamiento |
| **`tests alterados`** | **del banco**: reescribió el verificador |
| **`prompt truncado`** | **del banco**: el estímulo llegó cortado |

Colapsarlos en «no resuelto» es el error que ya hizo publicar un 2/10 donde
había un 4/10 ([`banco-de-tareas.md`](banco-de-tareas.md) §5).

---

## 6. Cómo se corre

OxideGate tiene que estar arriba **y enrutando al modelo local**. No se deduce:
se comprueba y se aborta.

```sh
# 1. El proxy, apuntando a ollama
OXIDEGATE_PORT=8901 OPENAI_API_BASE=http://127.0.0.1:11434/v1 cargo run --release

# 2. El corredor
CORREDOR_PUERTO=8901 CORREDOR_N=30 cargo run --example corredor-nivel-1
CORREDOR_HARNESS=opencode CORREDOR_N=30 cargo run --example corredor-nivel-1
```

Variables: `CORREDOR_HARNESS` (`pi`), `CORREDOR_N` (3), `CORREDOR_MODELO`
(`qwen3:14b-nothink`), `CORREDOR_PUERTO` (8899), `CORREDOR_TIMEOUT` (300),
`CORREDOR_ENCARGO`, `CORREDOR_RASTROS` (`./rastros-corredor`).

### Dos banderas que NO son opcionales

Las dos hacen que el harness **no haga nada sin dar ningún error**, y la
repetición se contaría como «no tocó el fichero» culpando al modelo de una
config del banco:

- **`pi`: `--approve`.** Sin él puede no confiar en los ficheros project-local y
  no inyectar el `AGENTS.md`.
- **`opencode`: `permission: {edit: allow, bash: allow}`.** Sin eso pide
  confirmación, y en modo no interactivo se queda parado.

La receta de `opencode` **no estaba** en [`banco-de-captura.md`](banco-de-captura.md)
§4 — solo se mencionaba su `HOME` aislado en §2. Va en
`$HOME/.config/opencode/opencode.json`, con `webfetch: deny` porque el banco es
de coste cero, y se lanza con `--pure`: se mide opencode, no lo que alguien le
haya instalado encima.

---

## 7. Lo que esto NO demuestra

- **No es el nivel 2.** Todo esto es con un modelo local y coste cero. Cuánto
  cuesta de verdad con el modelo de cada herramienta es
  [#123](https://github.com/pichu2707/OxideGate/issues/123).
- **No es el informe.** Las medianas, los rangos y la resta del peaje fijo son
  [#122](https://github.com/pichu2707/OxideGate/issues/122).
- **`tool_calls` no se mide en esta ruta.** Los dos dialectos de OpenAI declaran
  `captura_invocaciones() -> false`, así que el campo sale `n/d`. **No significa
  que el modelo no llamara a nada**: significa que este banco no lo mide.
- **Dos harnesses no son cuatro.** La comparación existe, y es más estrecha de lo
  que #29 quería.

---

## Ver también

- [`banco-de-tareas.md`](banco-de-tareas.md) — la tarea, su verificador y las
  reglas que este corredor hereda
- [`modelo-del-nivel-1.md`](modelo-del-nivel-1.md) — el modelo, su razonamiento
  apagado y su ventana de contexto
- [`banco-de-captura.md`](banco-de-captura.md) — las recetas de aislamiento
- [`floor-across-tools.md`](floor-across-tools.md) — el peaje fijo sobre una
  tarea trivial, que es lo que este documento mide ya sobre trabajo real
- [`fe-de-erratas.md`](fe-de-erratas.md) — E-013
