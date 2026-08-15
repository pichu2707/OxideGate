"""Coste en USD de una peticion, segun la tarifa del proveedor.

Los proveedores publican sus precios por MILLON de tokens.

Los tokens servidos desde cache se facturan mas baratos que los de entrada
normal: la tarifa de lectura de cache es el 10% del precio de entrada.
"""

# Fraccion del precio de entrada que se cobra por un token leido de cache.
TARIFA_CACHE = 0.10


def coste_usd(tokens_entrada, tokens_salida, tokens_cache, precio_entrada, precio_salida):
    """Coste en USD de una peticion.

    tokens_entrada  tokens de entrada facturados a precio completo
    tokens_salida   tokens generados por el modelo
    tokens_cache    tokens servidos desde cache (mas baratos, ver TARIFA_CACHE)
    precio_entrada  USD por MILLON de tokens de entrada
    precio_salida   USD por MILLON de tokens de salida
    """
    return (tokens_entrada * precio_entrada + tokens_salida * precio_salida) / 1000
