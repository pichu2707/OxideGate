"""Comprobaciones de tarifa.coste_usd.

Sin dependencias: se ejecuta con `python3 test_tarifa.py`.
Sale 0 si todo pasa, 1 si algo falla.
"""

import sys

from tarifa import coste_usd


def casi_igual(a, b):
    return abs(a - b) < 1e-9


CASOS = [
    # (nombre, argumentos, esperado)
    (
        "un millon de tokens de entrada a 3 USD/millon cuesta 3 USD",
        (1_000_000, 0, 0, 3.0, 15.0),
        3.0,
    ),
    (
        "un millon de tokens de salida a 15 USD/millon cuesta 15 USD",
        (0, 1_000_000, 0, 3.0, 15.0),
        15.0,
    ),
    (
        "una peticion vacia no cuesta nada",
        (0, 0, 0, 3.0, 15.0),
        0.0,
    ),
    (
        "un millon de tokens de cache cuesta el 10% de la entrada",
        (0, 0, 1_000_000, 3.0, 15.0),
        0.3,
    ),
    (
        "entrada, salida y cache se suman",
        (1_000_000, 1_000_000, 1_000_000, 3.0, 15.0),
        18.3,
    ),
]


def main():
    fallos = 0
    for nombre, args, esperado in CASOS:
        obtenido = coste_usd(*args)
        if casi_igual(obtenido, esperado):
            print("ok   " + nombre)
        else:
            fallos += 1
            print("FALLA " + nombre)
            print("      esperado " + repr(esperado) + ", obtenido " + repr(obtenido))

    if fallos:
        print("")
        print(str(fallos) + " de " + str(len(CASOS)) + " comprobaciones fallan")
        return 1

    print("")
    print("las " + str(len(CASOS)) + " comprobaciones pasan")
    return 0


if __name__ == "__main__":
    sys.exit(main())
