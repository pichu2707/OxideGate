# Limpieza de ramas locales

Después de cada cadena de PR quedan ramas locales muertas. Borrarlas es
higiene, pero **el recuento de commits no basta para decidir cuáles**, y este
documento existe porque en la limpieza del 2026-08-16 esa suposición estuvo a
punto de perder una medición.

## El criterio, en tres comprobaciones

Una rama solo es borrable si pasa las **tres**. Cada una detecta algo que la
anterior no ve.

### 1. ¿Tiene commits fuera de `main`?

```sh
git rev-list --count main..<rama>
```

`0` es buena señal, y **no es suficiente**. Es lo único que se miraba antes.

### 2. ¿Están sus PARCHES en `main`?

```sh
git cherry main <rama>     # '-' = el parche ya está | '+' = no está
```

Compara por *patch-id*, no por SHA, así que reconoce un commit rebasado. Pero
un rebase con resolución de conflictos **cambia el parche**, y entonces marca
`+` un commit cuyo trabajo sí llegó. Un `+` no condena: obliga a mirar el
contenido.

### 3. ¿Aporta alguna línea que `main` no tenga?

```sh
git diff main <rama> | grep '^+[^+]'
```

Esta es la que decide, y la que faltaba. Si sale algo, hay que leerlo: casi
siempre son redacciones viejas de líneas que `main` ya actualizó, pero puede
haber trabajo único.

Para separar una cosa de otra, buscar los datos concretos —cifras, fechas,
nombres de versión— en las dos puntas:

```sh
git grep -n "92%" main -- .
git grep -n "92%" <rama> -- .
```

Un número que solo aparece en la rama es una medición que se pierde al borrar.

## Por qué `git branch -d` no sustituye a esto

`-d` se niega a borrar lo no fusionado, y por eso **siempre se usa `-d` primero**:
es el propio git quien certifica cada borrado, no quien escribe el comando.

Pero su criterio no es el que uno espera. Si la rama tiene upstream, `-d` exige
que esté fusionada **también en ese upstream**; una rama enteramente contenida
en `main` pero por delante de un remoto viejo sale rechazada. En la limpieza del
2026-08-16 le pasó a `fix/monitor-visibilidad-y-scroll`: `rev-list` daba 0 y
`cherry` daba 0, y aun así `-d` la rechazó por ir 11 por delante de su remoto.

Un rechazo de `-d` es una **pregunta**, no un veredicto. Se contesta con la
comprobación 3, y solo entonces `-D`.

## Lo hecho el 2026-08-16

`main` en `84cea7b`. Quince ramas locales además de `main`.

**Catorce borradas.** Trece con `-d` —git certificó que estaban fusionadas— y
`fix/monitor-visibilidad-y-scroll` con `-D` (SHA `64ebd08`) tras comprobar que
sus 79 líneas propias eran redacciones anteriores de ficheros que `main` ya
tiene actualizados. Sus hallazgos publicados (`86%`, `65% en Codex`, `202 B`,
la validación del 2026-08-09) están en `main`, y en más ficheros que en la rama.

**Una retenida: `backup/monitor-antes-del-rebase`** (SHA `0ec8392`).

## La rama que NO se borra, y qué hay que hacer con ella

`backup/monitor-antes-del-rebase` guarda los seis commits anteriores al rebase
de la cadena #109–#113. Cinco de sus parches están en `main`. El sexto
—`9280235 refactor(monitor): pintar sobre un lienzo`— sale `+` en `git cherry`,
y ahí la comprobación 3 encontró **12 líneas que `main` no tiene en ninguna
parte**: doc-comments en `src/bin/monitor.rs` con una medición dentro.

> En el camino compatible con OpenAI —el que proxea OxideGate— ollama **no
> expone** `load_duration`, así que `total_ms` y `tok/s` de una petición fría
> incluyen cargar el modelo sin que nada lo diga. Medido: **el 92%** del tiempo
> de una petición fría fue carga, no inferencia.

`git grep "92%" main` no devuelve nada: ni en código, ni en `docs/`, ni en el
README. **La medición se perdió en el rebase** y solo sobrevive en el backup.

Es justo la clase de pérdida silenciosa que este proyecto persigue en todas
partes: un número medido que desaparece sin que falle nada y sin que nadie se
entere.

**Tarea pendiente**: [#125](https://github.com/pichu2707/OxideGate/issues/125).
Rescatar esos doc-comments a `main` y solo entonces borrar el backup. Hasta que
eso pase, la rama se queda: es la única copia.

## Recuperar una rama borrada

El reflog las conserva unos 90 días:

```sh
git reflog                       # buscar el SHA
git branch <nombre> <sha>        # resucitarla
```

Por eso conviene **imprimir el SHA de cada rama antes de borrarla**: sin el
registro previo, encontrarla en el reflog es rebuscar a ciegas.
