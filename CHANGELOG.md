# Changelog

## 0.1.0 - 2026-10-19

First release.

- `sparkle-surge` animation: a CPU port of Android's wired-charging ripple, with a glow, sparkle ring and wobbling edge.
- `--origin` to start the animation from any corner, edge or the center, for example where the charging port is.
- `--primary-color` tint, `--duration-ms` length (default 1750ms) and `--all-monitors [true|false]`.
- Renders on the CPU using lookup tables, block skipping, dirty-region damage and multiple threads; about 3ms per frame at 2560x1600.
