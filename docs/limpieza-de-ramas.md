# Limpieza de ramas locales

Después de cada cadena de PR quedan ramas locales muertas. Borrarlas es
higiene, pero **el recuento de commits no basta para decidir cuáles**, y este
documento existe porque en la limpieza del 2026-08-16 esa suposición se dio de
bruces con una rama que parecía redundante y no lo era del todo.

La respuesta que se dio entonces —mirar el contenido— era necesaria pero
**incompleta**: leyó mal lo que encontró, dio por perdida una medición que en
realidad había sido retractada, y retuvo la rama ocho días de más. De ahí la
cuarta comprobación, y de ahí la
[E-009](fe-de-erratas.md) de la fe de erratas.

## El criterio, en cuatro comprobaciones

Una rama solo es borrable si pasa las **cuatro**. Cada una detecta algo que la
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

Un número que solo aparece en la rama **parece** una medición que se pierde al
borrar. No basta con eso: pasa a la 4.

### 4. ¿El dato que falta se perdió, o fue SUPERADO?

```sh
git log --oneline -S "92%" main -- <fichero>
git log --oneline -S "92%" <rama> -- <fichero>
```

`git grep` mira una foto; `git log -S` mira la película. Un dato **borrado a
propósito** y un dato **perdido en un rebase** dan exactamente el mismo cero en
`grep`, y solo se separan aquí: si `main` tiene un commit **de más** tocando esa
cifra, ese commit es la retractación, y no hay nada que rescatar.

Fue el caso del 92% ([#125](https://github.com/pichu2707/OxideGate/issues/125)):
tres commits lo introducen y están en las dos puntas; el cuarto, `64063bf`, lo
sustituye por el rango 54%–98% y solo está en `main`. La rama no guardaba una
medición perdida sino **una foto anterior a su corrección**, y rescatarla habría
reintroducido una cifra retractada.

Un segundo indicio, más barato, para el mismo caso: **contar el diff en ambos
sentidos**. Doce líneas solo en la rama contra 129 solo en `main` no es el perfil
de una rama que guarda trabajo único; es el perfil de una rama vieja.

```sh
git diff main <rama> -- <fichero> | grep -c '^+[^+]'   # solo en la rama
git diff main <rama> -- <fichero> | grep -c '^-[^-]'   # solo en main
```

## Por qué `git branch -d` no sustituye a esto

`-d` se niega a borrar lo no fusionado, y por eso **siempre se usa `-d` primero**:
es el propio git quien certifica cada borrado, no quien escribe el comando.

Pero su criterio no es el que uno espera. Si la rama tiene upstream, `-d` exige
que esté fusionada **también en ese upstream**; una rama enteramente contenida
en `main` pero por delante de un remoto viejo sale rechazada. En la limpieza del
2026-08-16 le pasó a `fix/monitor-visibilidad-y-scroll`: `rev-list` daba 0 y
`cherry` daba 0, y aun así `-d` la rechazó por ir 11 por delante de su remoto.

Un rechazo de `-d` es una **pregunta**, no un veredicto. Se contesta con las
comprobaciones 3 y 4, y solo entonces `-D`.

## Lo hecho el 2026-08-16

`main` en `84cea7b`. Quince ramas locales además de `main`.

**Catorce borradas.** Trece con `-d` —git certificó que estaban fusionadas— y
`fix/monitor-visibilidad-y-scroll` con `-D` (SHA `64ebd08`) tras comprobar que
sus 79 líneas propias eran redacciones anteriores de ficheros que `main` ya
tiene actualizados. Sus hallazgos publicados (`86%`, `65% en Codex`, `202 B`,
la validación del 2026-08-09) están en `main`, y en más ficheros que en la rama.

**Una retenida: `backup/monitor-antes-del-rebase`** (SHA `0ec8392`) — retenida
por error, como se cuenta abajo.

## La rama que se retuvo por error

`backup/monitor-antes-del-rebase` (SHA `0ec8392`) guardaba los seis commits
anteriores al rebase de la cadena #109–#113. Cinco de sus parches están en
`main`. El sexto —`9280235 refactor(monitor): pintar sobre un lienzo`— sale `+`
en `git cherry`, y ahí la comprobación 3 encontró 12 líneas que `main` no tenía:
doc-comments en `src/bin/monitor.rs` con una medición dentro.

> En el camino compatible con OpenAI —el que proxea OxideGate— ollama **no
> expone** `load_duration`, así que `total_ms` y `tok/s` de una petición fría
> incluyen cargar el modelo sin que nada lo diga. Medido: **el 92%** del tiempo
> de una petición fría fue carga, no inferencia.

`git grep "92%" main` no devuelve nada, y de ahí se concluyó que el rebase se
había comido la medición. La rama se retuvo como única copia.

**La conclusión era falsa.** El 92% no se perdió: lo retractó `64063bf` (del
2026-08-09, siete días antes de esta limpieza) y lo sustituyó por el rango
**54%–98%**, que sigue vivo en `main` en los tres mismos sitios
(`src/bin/monitor.rs:208`, `:2819`, `:6036`) junto con la razón de que no exista
constante y el dato de que la ruta nativa `/api/chat` sí separa la carga con
`load_us`. El backup es una foto ANTERIOR a esa corrección. El inventario del
diff lo dice sin ambigüedad: **12 líneas solo en el backup, 129 solo en `main`**.

Es justo la clase de error que este documento existía para evitar, cometido por
el propio documento: no una pérdida silenciosa, sino **una pérdida imaginada**.
Un `grep` a cero se leyó como «desapareció» cuando significaba «se corrigió».

**Borrada el 2026-08-24**, en local y en el remoto, con
[#125](https://github.com/pichu2707/OxideGate/issues/125) cerrado sin rescatar
nada. El SHA queda escrito arriba por si alguna vez hiciera falta resucitarla.
El relato completo, en la [E-009](fe-de-erratas.md).

## Recuperar una rama borrada

El reflog las conserva unos 90 días:

```sh
git reflog                       # buscar el SHA
git branch <nombre> <sha>        # resucitarla
```

Por eso conviene **imprimir el SHA de cada rama antes de borrarla**: sin el
registro previo, encontrarla en el reflog es rebuscar a ciegas.
