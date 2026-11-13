# Medición reproducible del índice

Medición del 13 de noviembre de 2026, Linux x86-64, compilación release, Rust 1.100.0. El proceso informó 9 CPU lógicas disponibles. Los resultados completos están en [`benchmark-linux-1m.json`](benchmark-linux-1m.json); se conserva la [medición anterior](benchmark-linux-1m-baseline.json).

```bash
cargo build --locked --release --no-default-features --bin es
./target/release/es benchmark --records 1000000
```

El comando crea una base temporal con un millón de registros de metadatos artificiales y la elimina al terminar. No toca el índice del usuario. Cada consulta se repite cinco veces; la medición incluye el conteo exacto y la primera página de 500 filas, con la base ya cargada por la inserción. No es una prueba de caché fría.

## Resultados

| Medida de construcción | Resultado |
|---|---:|
| Registros | 1.000.000 |
| Inserción del índice | 61,44 s |
| Ritmo de inserción | 16.277 registros/s |
| Base de datos | 636.125.184 bytes (606,66 MiB) |

| Consulta | Antes, mediana | Actual, mediana |
|---|---:|---:|
| Primera página sin filtro | 0,30 ms | 0,34 ms |
| Nombre específico, 1 coincidencia | 226,54 ms | 7,52 ms |
| Ruta específica, 1.000 coincidencias | 647,63 ms | 80,39 ms |
| `ext:pdf`, 250.000 coincidencias | 268,13 ms | 267,46 ms |
| Tamaño y extensión, 198.800 coincidencias | 281,09 ms | 268,84 ms |
| `document !ext:txt`, 700.000 coincidencias | 641,86 ms | 859,17 ms |

La versión inicial elegía solamente el primer trigrama del término. Prefijos comunes como `inv` y `pro` admitían demasiados candidatos. La versión actual combina hasta ocho trigramas distintos repartidos por el término, incluyendo su sufijo; mantiene el predicado exacto para eliminar falsos positivos. En esta prueba, el nombre específico pasó de 226,54 a 7,52 ms y la ruta de 647,63 a 80,39 ms.

La mejora tiene un coste cuando el término aparece en casi todos los registros: `document !ext:txt` pasó de 641,86 a 859,17 ms. Los filtros por metadatos también deben contar y ordenar sus coincidencias. No se promete que todas las consultas sean instantáneas.

## Límites

Los tiempos no se extrapolan a Windows ni al índice de 8,5 millones de objetos de la captura. La lectura inicial del sistema de archivos, antivirus, discos, permisos, nombres reales, páginas profundas y caché del sistema pueden cambiar sustancialmente el comportamiento. El tiempo de inserción indicado mide metadatos sintéticos, no el recorrido de un disco real.

La base persistente, las colas acotadas y la paginación evitan cargar el catálogo completo de rutas y resultados en la GUI. El tiempo de consulta permanece fuera del hilo de renderizado; una consulta amplia puede tardar aunque la ventana siga respondiendo.
