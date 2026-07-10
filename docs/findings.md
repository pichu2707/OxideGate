# Hallazgos — qué se probó, qué se descartó, qué se retractó

> Esta página organiza por CONCLUSIÓN lo que una jornada de medición dejó en
> `docs/context-tax.md`, `docs/optimizer-dedup.md` y `docs/monitor-tui.md`. No
> repite la evidencia — la referencia. Quien clone este repo debería poder
> leer esta página en cinco minutos y salir sabiendo qué no volver a intentar.

---

## A. La conclusión central

El código de este repositorio no optimiza nada. Construye el instrumento que
permite ver dónde está el desperdicio. Las optimizaciones reales encontradas
en una jornada completa fueron un archivo de configuración de siete líneas y
dos flags del CLI. Ninguna se habría encontrado sin medir.

Y una advertencia que vale para las tres: **ninguna es gratis.** `--tools`
recorta lo que el agente puede hacer, `CLAUDE.md` lean recorta las reglas que
obedece, y `--effort low` recorta cuánto razona. La optimización aquí no
consiste en encontrar desperdicio puro, sino en saber exactamente qué se está
cambiando por qué.

---

## B. Dónde está el coste

Medido sobre tráfico real de agente (`docs/context-tax.md` §1-§4):

| Hallazgo | Cifra |
|---|---|
| Proporción del coste que es maquinaria de contexto (releer + escribir el prefijo) | 78,2% |
| Proporción que es input genuinamente nuevo | 3,0% |
| Ahorro del prompt caching frente a no cachear nada | de ~$45 nocionales a ~$8,33 por sesión — el caching ya trabaja al máximo; la palanca que queda no es cachear más, es reducir el prefijo |
| Crecimiento del coste de una conversación | N², no N — cada turno relee el prefijo entero, y el prefijo crece con cada turno |

Composición del body de una petición típica (225.798 B), de mayor a menor:

| Bloque | Bytes | % del body |
|---|---|---|
| `tools` (esquemas de herramientas) | 159.874 | 70,8% |
| `CLAUDE.md` global, inyectado como `<system-reminder>` en `messages[0]` | 35.140 | 15,6% |
| Volcado del hook `SessionStart` de Engram, en `messages[1]` | 19.668 | 8,7% |
| `system` del harness | 8.928 | 4,0% |
| El mensaje del usuario | 75 | 0,03% |

Detalle completo: `docs/context-tax.md` §2 y §4.1.

---

## C. Un concepto que corrige la intuición

La caché cambia el PRECIO, no la CANTIDAD. Un token cacheado se sube igual
por el cable, ocupa la misma ventana de contexto, pasa por prefill igual y
cuenta igual para los rate limits. Cuesta el 10% de la tarifa de input, en
cada turno, para siempre. No existe "cachear al abrir el proyecto": la API es
sin estado y una conversación es su lista de mensajes completa, repetida en
cada request.

---

## D. Las palancas que funcionan (medidas)

| Palanca | Efecto medido | Advertencia |
|---|---|---|
| Configuración de MCP: `.claude/mcp-lean.json` + `--strict-mcp-config` | Elimina 55.098 B/petición de tres conectores de Google que no se usan en este repo | El archivo por sí solo no hace nada: hace falta el flag, porque una config de proyecto SUMA servidores, no los quita |
| `--tools <lista>` | Recorta 94,9% del array de esquemas | `--disallowedTools` NO sirve para esto — solo ahorra 0,5%, porque es una puerta de permiso, no de payload |
| Techo apilado sobre la misma sonda | 224.653 B (sin cambios) → 149.221 B (+ `--strict-mcp-config`) → 51.540 B (+ `--tools Read,Bash`), −77,1% total | El trade es real: un agente así no edita, no busca por patrón ni delega a subagentes |
| `CLAUDE.md` lean | Medido: ahorra 29.867 B por petición (−13,3%). El 85,1% de ese archivo describe flujos que se invocan, no reglas que se obedecen | **No es una palanca lista.** Se midió el byte, no el comportamiento. Adoptarlo sin crear antes las skills a las que apunta perdería las reglas en silencio: el agente dejaría de delegar y de guardar en memoria sin avisar |
| `--effort low` | −20,0% tokens de salida y −22,0% de reloj de pared en una tarea de razonamiento (n=3 pares, rangos sin solape) | **No acelera: recorta.** El `tok/s` no se mueve (rangos solapados). Y lo que recorta es pensamiento, así que el tiempo se paga en profundidad de razonamiento. En tareas que no razonan, no hace nada |

Detalle completo y las cuatro sondas: `docs/context-tax.md` §5. La única
palanca de velocidad medida, con su experimento y su experimento fallido:
`docs/speed.md` §3.1.

---

## E. Los callejones sin salida

Descartados con evidencia, para que nadie los repita.

| Descartado | Por qué | Dónde está la evidencia |
|---|---|---|
| Palanca B — dedup de respuestas por `prompt_hash` | Muerta para tráfico conversacional: `redundancy_rate` es 0,0 por construcción (el hash se calcula sobre el body completo y `messages` crece cada turno), Claude Code siempre streamea, y el techo teórico de ahorro es solo el 3,0% del costo | `docs/optimizer-dedup.md` §0 |
| Optimizar el transporte MCP | Un salto por stdio ronda el orden de un milisegundo, frente al orden de segundos que tarda un turno completo del agente — la brecha es de varios órdenes de magnitud; llevarlo a cero no movería el número que importa | reportado, sin fila de `docs/context-tax.md` que lo respalde línea a línea — ver nota de verificación abajo |
| Gateway MCP que activa/desactiva servidores a mitad de sesión | Cambiar el array `tools` invalida el prefijo cacheado entero; el punto de equilibrio reportado ronda el centenar de turnos por toggle. Sirve como selector al arrancar, no como interruptor en vivo | reportado — mismo caso que la fila anterior, ver nota de verificación abajo |
| Hilos paralelos como ahorro | Compran reloj de pared, no lo ahorran gratis: cada hilo paga su propio piso de prefijo de herramientas (del orden de las decenas de miles de tokens, ver §B). Además, la mayor parte del reloj de pared de una sesión con humano en el loop es el humano pensando, no la máquina: el 77% del reloj de pared de la sesión medida | `docs/context-tax.md` §3 |
| Subida de bytes y overhead del propio proxy | ~280 KB por request son ~7 ms de transferencia en fibra. `prepare_us` (el tiempo que OxideGate pasa dentro de `prepare`) va de 43 µs con un body minúsculo a 15.135 µs con uno de 450 kB: entre 0,04 y 15 milisegundos, frente a un TTFT medio de 2.097 ms. Es el 0,67% de una petición típica. Ninguno de los dos explica la dispersión del TTFT | `docs/context-tax.md` §3 |
| El tiempo de generación del modelo | 82% del tiempo "ocupado" de una petición es el modelo generando tokens. Un proxy se sienta en el medio del wire; no puede acelerar eso | `docs/context-tax.md` §3 |
| Fast mode (`speed: "fast"`) como palanca disponible | Existe, está documentada y ataca precisamente ese 82%. Pero **requiere créditos de API**: con suscripción plana (Max), `/fast` pide créditos y no se activa. No es una palanca que se pueda accionar sin cambiar de modelo de facturación | `docs/speed.md` §3 |
| `--exclude-dynamic-system-prompt-sections` como palanca de tamaño | Es una **reubicación, no una reducción**: mueve ~3,5 kB de secciones per-máquina (cwd, env, git status, rutas de memoria) de `system` a `messages`; el neto del body baja solo 330 B (0,22%). Su beneficio real —caché compartida entre usuarios— es inerte para un único usuario, mismo patrón que la Palanca A | `docs/context-tax.md` §5.1 |

> **Dónde vive la evidencia.** Estas cifras están medidas, pero su evidencia
> cruda no está versionada en el repositorio, así que conviene decir dónde
> buscarla antes de darlas por buenas.
>
> - La invalidación de caché al cambiar el array `tools` es reproducible desde
>   `~/.config/oxidegate/telemetry.jsonl`: tres peticiones consecutivas con
>   `cache_read` de 54.247, luego 0 con `cache_write` de 76.356, y de vuelta
>   54.247 al restaurar la configuración anterior.
> - La latencia del salto MCP (0,68 ms de mediana) se midió con un cliente
>   JSON-RPC directo por stdio contra el servidor, fuera de OxideGate: el proxy
>   no ve el tráfico MCP y no puede medirlo. Ese número no está en el JSONL.
>
> Ninguna de las dos es una hipótesis. Ambas son mediciones cuya evidencia vive
> fuera del control de versiones, que no es lo mismo.

---

### Compresión: dos cosas distintas, una sola importa

**Compresión de bytes (gzip, brotli) — otro callejón sin salida.** El
modelo tokeniza el texto DESCOMPRIMIDO. Un body de 280 KB comprimido a 40 KB
sigue siendo la misma cantidad de tokens: mismo costo, misma ocupación de
ventana de contexto, mismo prefill, mismo consumo de rate limit. El único
ahorro posible es el tiempo de subida, y ese ya se midió y se descartó como
cuello de botella: ~7 ms en fibra para un body de 280.764 bytes (~225 ms a
10 Mbps), frente a un TTFT medio de 2.097 ms.

Con un matiz honesto: hoy el proxy descarta a propósito `Accept-Encoding`
(`src/middleware/proxy.rs`, línea 107) para que el proveedor devuelva la
respuesta sin comprimir y el escáner SSE pueda leer `usage` en texto plano.
Habilitar el feature `gzip` de `reqwest` dejaría que el transporte
descomprima de forma transparente y restauraría la compresión de cable sin
cegar el medidor. La ganancia es chica (la salida de esta sesión fue de
20.943 tokens) y la interacción con el streaming está SIN VERIFICAR. Queda
anotado como deuda, no como hallazgo.

**Compresión de tokens — esta sí es real, y está medida como sub-usada.**
Anthropic expone dos mecanismos, ambos visibles en la clave raíz
`context_management` del body de la request: `clear_tool_uses_20250919`
(limpia resultados viejos de tools del historial, opcionalmente también sus
inputs) y `compact_20260112` (resume contexto anterior del lado servidor).

MEDIDO sobre un body real capturado: Claude Code manda
`context_management: {"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]}`.
Es decir, declara context editing pero no limpia nada (`keep: "all"`), y no
usa ni `clear_tool_uses_20250919` ni `compact_20260112`. En la sesión
interactiva medida, el historial llegó a 265.704 bytes — los resultados
viejos de tools viven ahí y se releen en cada turno.

También medido en el mismo body: `cache_control` aparece 3 veces (del máximo
documentado de 4 breakpoints), o sea que el cliente sí gestiona su propio
prompt caching. Es exactamente por eso que la Palanca A es un no-op contra
Claude Code: `has_cache_control` encuentra una ocurrencia y declina inyectar
(detalle en `docs/optimizer-prompt-cache.md` §5 y §8).

OxideGate puede VER todo esto — `context_management` es una clave raíz del
body que ya parsea. Detalle de la palanca de caché y del fallo silencioso
relacionado: `docs/optimizer-prompt-cache.md` §8-§10.

---

## F. Hallazgos inesperados

- **Truncamiento silencioso.** Un agente contra un modelo local devolvió
  `200 OK` con una respuesta sin sentido: el proveedor había descartado buena
  parte del prompt al llegar al tope de su ventana de contexto, sin error ni
  aviso. El detector `TRUNC` del monitor lo caza sin depender de una
  constante mágica de bytes-por-token — ver `docs/monitor-tui.md` §7.4.
- **La transparencia del proxy depende de que el cliente coopere.** Vale para
  Claude Code, `curl` y Ollama, que respetan la variable de entorno de base
  URL del proveedor. No vale para clientes que traen su propio gateway
  interno y no exponen ese punto de redirección de la misma forma.
- **La Palanca A funciona, y es inerte donde se usa.** El optimizador de
  prompt cache (`force_cache_control`) se ejecutó por primera vez fuera de sus
  tests unitarios, contra un sumidero HTTP local: inyecta el breakpoint cuando
  el cliente no trae el suyo, y respeta el del cliente cuando lo trae, incluso
  anidado. Pero contra Claude Code —el único cliente que atraviesa este
  proxy— **nunca dispara**, porque Claude Code cachea por su cuenta. Una
  palanca correcta apuntando a un cliente que no existe. Detalle y sondas:
  `docs/optimizer-prompt-cache.md` §6.1.
- **Un flag apagado explica una ausencia mejor que cualquier teoría.** Durante
  133 peticiones, `cache_control_forced` fue `false` siempre. La causa no era
  ninguna hipótesis interesante sobre el cliente: `OXIDEGATE_FORCE_CACHE`
  nunca se puso, y `unwrap_or(false)` hizo el resto. Antes de explicar por qué
  un mecanismo se comporta raro, conviene comprobar que llegó a ejecutarse.
- **OxideGate mide modelos locales sin código nuevo.** Ollama habla el
  dialecto de la API de OpenAI, así que el adaptador existente lo mide sin
  cambios (`docs/telemetry-level-1.md` §5, `docs/monitor-tui.md` §7.3.1).
  `cost_estimate_usd` queda en `null` para tráfico local sin tabla de
  precios — es la respuesta correcta, no un dato faltante: en local no hay
  dólares. La moneda real es la ventana de contexto, y los esquemas de
  herramientas de un agente pueden ser varias veces esa ventana.

---

## G. Retractaciones

Con la misma prominencia que los hallazgos.

Se documentó que estar dentro del proyecto costaba 22.153 tokens extra de
prefijo y se atribuyó a la memoria persistente y al registro de skills.
**Falso.** Era un experimento de n=1 que no controlaba cuántos servidores MCP
se cargaban en cada corrida. Verificado con captura directa de ambos bodies y
con cuatro repeticiones: la diferencia real de estar dentro del proyecto son
**589 bytes**, no 22.153 tokens. Detalle completo en `docs/context-tax.md`
§4.

Se afirmó que la Palanca A nunca disparaba **porque** Claude Code manda su
propio `cache_control`. La conclusión resultó cierta, pero se enunció sin
comprobarla, y la causa inmediata era otra: el flag `OXIDEGATE_FORCE_CACHE`
estaba apagado por defecto, así que la comprobación sobre el cliente ni
siquiera llegaba a ejecutarse. Había dos puertas y se señaló la segunda sin
mirar la primera. Acertar sin verificar no es acertar; es tener suerte, y la
suerte no es reproducible.

---

## H. Lecciones de método

La parte que sobrevive al proyecto, aunque el proyecto cambie.

| Lección | Por qué importa |
|---|---|
| Un experimento de n=1 no distingue una causa de una coincidencia | Repetir y controlar las variables que no se están mirando — es exactamente lo que produjo la retractación de §G |
| Comparar solo lo comparable | Mezclar una sonda de 2 mensajes con un turno de 130 produce números sin significado |
| Triangular con métodos independientes | El desglose por servidor MCP se validó con resta de sondas, captura de red y proxy en vivo; los tres coincidieron dentro del 0,5% |
| Un test que no falla cuando se introduce el bug no vale nada | Exigir la prueba de mordida: romper a propósito, ver el test caer, restaurar |
| Desconfiar de un número que se repite | Dos muestras de tamaños distintos con idéntico `prompt_tokens` no son casualidad: son un tope |
| El instrumento también miente | Un barrido de texto que no distingue mayúsculas puede dar un falso limpio |
| Ante "no se ve algo", sospechar primero de la vista, no del dato | La primera hipótesis debe ser "el dato existe y falta cómo mostrarlo", no "hay que medirlo de nuevo" |

---

## Pendiente

Ideas registradas, no implementadas:

- **Detector `NOCACHE`** en el monitor TUI: marca el fallo silencioso de la
  Palanca A cuando `cache_control_forced == true` y `cache_write_tokens` es
  `null` o `0` — misma forma que el detector `TRUNC` ya existente. Detalle:
  `docs/optimizer-prompt-cache.md` §10.
- **Reporte de `context_management` por petición**: exponer si el cliente
  usa compresión de tokens (`clear_tool_uses_20250919`, `compact_20260112`)
  y con qué configuración, ya que OxideGate parsea esa clave del body pero
  hoy no la reporta. Detalle: sección "Compresión: dos cosas distintas, una
  sola importa" más arriba en este mismo archivo.

---

## Ver también

- `docs/context-tax.md` — la descomposición medida de costo y latencia de una sesión real
- `docs/optimizer-dedup.md` — por qué se descartó la Palanca B, en detalle
- `docs/telemetry-per-request.md` — el endpoint que expone el desglose de contexto por petición
- `docs/monitor-tui.md` — el detector `TRUNC` y el resto de marcadores de outlier
