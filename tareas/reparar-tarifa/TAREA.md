# Tarea sonda: reparar la tarifa

`test_tarifa.py` falla. Haz que pase.

No cambies el fichero de tests: el defecto esta en `tarifa.py`.

## Como se comprueba

```sh
python3 test_tarifa.py
```

Sale `0` si pasa, `1` si no. Sin dependencias.

---

## Para quien mantiene el banco, no para quien resuelve la tarea

Esta tarea es el estimulo de [#29](https://github.com/pichu2707/OxideGate/issues/29).
Se eligio con tres condiciones, y cada una tiene su motivo:

1. **Veredicto binario y objetivo.** El runner sale 0 o no sale 0. Nadie tiene
   que juzgar si la respuesta «esta bien». El camino no es determinista; el
   veredicto si.
2. **Dos defectos, no uno.** Uno de escala —la division es por mil y los
   precios vienen por millon— y otro de omision —`tokens_cache` se recibe y no
   se usa—. Con un solo defecto de un caracter, la tarea se resuelve de
   memoria sin leer los tests y deja de discriminar entre herramientas.
3. **ASCII puro.** Ni una tilde en el fixture. El contenido viaja a modelos y
   lo editan agentes distintos: si llevara acentos, la codificacion entraria
   como variable en un experimento que mide otra cosa.

El estado inicial **tiene que fallar**. Si algun dia pasa sin tocar nada, el
banco esta roto y cualquier medicion hecha con el no vale.
