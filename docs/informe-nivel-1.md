# El informe del nivel 1 — la misma tarea, cuánto cuesta en cada harness

> Medido el **2026-08-30** contra `ollama` 0.30.10 y `qwen3:14b-nothink`,
> `n=30` por harness. Coste: **cero**. Es la tercera rodaja de
> [#29](https://github.com/pichu2707/OxideGate/issues/29) y cierra
> [#122](https://github.com/pichu2707/OxideGate/issues/122).

Se genera con `cargo run --example informe-nivel-1`, que lee lo que el corredor
dejó anotado. **Este documento no contiene ninguna cifra escrita a mano.**

---

## 1. La tabla

Todas las repeticiones, resuelvan o no:

| harness | resueltas | bytes/rep | turnos | peaje | **trabajo real** |
|---|---:|---:|---:|---:|---:|
| `opencode` 1.18.25 | 28/30 | 259.103 (32.013-337.270) | 9 (2-11) | 31.975 | **227.128** (38-305.295) |
| `pi` 0.80.10 | 29/30 | 105.188 (22.731-156.382) | 10 (3-13) | 5.932 | **99.256** (16.799-150.450) |

Mediana (mín-máx). **Nunca solo la media**: [#29](https://github.com/pichu2707/OxideGate/issues/29)
lo exige, y con razón — dos harnesses con la misma media y rangos distintos no
cuestan lo mismo.

### Y las mismas cifras solo sobre lo que resolvió

| harness | resueltas | bytes/rep | peaje | **trabajo real** |
|---|---:|---:|---:|---:|
| `opencode` 1.18.25 | 28/30 | 259.103 (190.641-337.270) | 31.975 | **227.128** (158.666-305.295) |
| `pi` 0.80.10 | 29/30 | 105.188 (42.361-156.382) | 5.932 | **99.256** (36.429-150.450) |

**Las dos tablas hacen falta, y mezclarlas engaña.** Fíjate en el mínimo del
trabajo real de `opencode`: **38 B** arriba, **158.666 B** abajo. Ese 38 sale de
una repetición que murió en dos turnos sin tocar la tarea. Leído en la primera
tabla parece «lo barato que sale trabajar»; es «lo barato que sale rendirse».

---

## 2. Lo único que se puede afirmar, y por qué

**El trabajo real más caro de `pi` (150.450 B) es menor que el más barato de
`opencode` (158.666 B).** Los rangos **no se solapan**.

Eso es mucho más fuerte que comparar medianas, y es la única forma honesta de
decir «X cuesta menos que Y» con `n` finito: dos medianas distintas con rangos
que se pisan no distinguen nada, y esa es exactamente la cifra que #29 prohíbe
publicar. El informe lo comprueba solo y dice «LOS RANGOS SE SOLAPAN» cuando
toca.

### Lo que NO se puede afirmar

**Que un harness resuelva más que otro.** 29/30 contra 28/30 es ruido. Y la
propia tasa se mueve entre corridas idénticas: una corrida anterior del mismo
día dio **30/30 y 30/30**. Publicar «`pi` resuelve el 100%» habría quedado
desmentido por la siguiente ejecución.

Por eso `resueltas/n` va **primero** en la tabla y no como nota: sin eso el
coste no se puede interpretar. Una herramienta que resuelve 18 de 20 gastando el
doble no es peor que una que resuelve 9 de 20 gastando la mitad.

---

## 3. El peaje: por qué NO es el de `floor-across-tools.md`

**`trabajo real` = bytes totales − peaje fijo.** Separar «lo que cuesta
arrancar» de «lo que cuesta trabajar» es la aportación de este proyecto a la
pregunta de #29, y nadie que mire solo el total está en posición de hacerla.

Pero el peaje **no se puede tomar de [`floor-across-tools.md`](floor-across-tools.md)**:

| | peaje publicado (§1) | peaje bajo el aislamiento del corredor |
|---|---:|---:|
| `opencode` | 117.125 B (v1.18.5) | **31.975 B** (v1.18.25) |
| `pi` | **no está en la tabla** | **5.932 B** |

Aquella tabla mide «tal como está instalado aquí» — con las skills, el MCP y la
configuración del usuario. **El corredor corre con el `HOME` aislado**, y ahí no
existe nada de eso. Son instalaciones distintas, y el propio §4.2 ya avisaba:
*«los totales no son comparables entre instalaciones»*.

Restar el publicado habría dado `305.131 − 117.125 = 188.006`: un número que **no
parece absurdo** y está inventado. Es el tipo de cifra que suena medida y no lo
está — justo contra lo que existe este informe.

Así que el peaje se mide con `CORREDOR_MODO=peaje`: **mismo aislamiento, misma
config, mismo prompt trivial que §1, sin la tarea**. La resta es válida **por
construcción**. Y sale determinista: rango cero en las 5 repeticiones de los dos
harnesses.

> El `AGENTS.md` normalizado **cae del lado del peaje**, no del trabajo. Es
> ceremonia: el harness lo manda antes de hacer nada.

---

## 4. La contaminación, que va aquí y no en un apéndice

[`optimizer-tool-search.md`](optimizer-tool-search.md) §3 lo tiene medido con
grupo de control: enrutar Claude Code por OxideGate le hace **dejar de diferir
sus esquemas MCP**, porque `ANTHROPIC_BASE_URL` no es first-party. **El
instrumento produce el fenómeno.**

La consecuencia es que **Claude Code medido a través del proxy no es Claude
Code**. No está en esta tabla; si algún día entra, entra con este aviso pegado a
la cifra.

---

## 5. Quién falta, y por qué

| harness | por qué no está |
|---|---|
| **Codex** 0.142.5 | Su router rechaza las llamadas del modelo contra el `/v1/responses` de ollama (`unsupported call:`). **No es de OxideGate**: pasa igual apuntando directo. Ver [`corredor-nivel-1.md`](corredor-nivel-1.md) §2 |
| **Qwen Code** 0.21.7 | 102 KB de declaraciones de herramientas: ~33.000 tokens contra un techo de 32.768. **No cabe** |
| **Claude Code** | El instrumento lo contamina (§4) |

Dos de cuatro. La comparación existe y es **más estrecha de lo que #29 quería**.

---

## 6. Lo que este informe NO hace

- **No recomienda una herramienta.** Aquí se publica el dato; elegir es de quien
  paga.
- **No compara tokens.** Los tokenizadores difieren entre proveedores. Se compara
  por **bytes mandados**, que es la variable controlada.
- **No es el nivel 2.** Todo esto es con un modelo local y coste cero. Cuánto
  cuesta con el modelo de cada herramienta es
  [#123](https://github.com/pichu2707/OxideGate/issues/123).

---

## Ver también

- [`corredor-nivel-1.md`](corredor-nivel-1.md) — el corredor que produce estos
  datos, sus guardas y sus veredictos
- [`banco-de-tareas.md`](banco-de-tareas.md) — la tarea y su verificador
- [`floor-across-tools.md`](floor-across-tools.md) — el peaje con la
  configuración instalada, que es **otra** medición
- [`modelo-del-nivel-1.md`](modelo-del-nivel-1.md) — el modelo constante
