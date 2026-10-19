# AOSP wired charging ripple (reference only)

Upstream source of the ripple Android SystemUI plays when a charger is connected.
Kept here as a reference for glint's `sparkle-surge` animation. It is not compiled.

- Source: https://android.googlesource.com/platform/frameworks/base/+/1cdfff555f4a21f71ccc978290e2e212e2f8b168/packages/SystemUI
- Fetched: 2026-10-19 from `refs/heads/main`
- License: Apache 2.0 (see `LICENSE` and the header in each file)

## Files

| Path | Role |
|---|---|
| `src/.../charging/WiredChargingRippleController.kt` | Hooks the battery "plugged in" event, sizes the ripple and centres it on the charging port |
| `animation/.../ripple/RippleShader.kt` | The AGSL shader: base ring, sparkle ring, center fill, distortion, fade curves |
| `animation/.../ripple/RippleView.kt` | Drives the shader over time (default duration 1750 ms) |
| `animation/.../ripple/RippleAnimation*.kt`, `MultiRipple*.kt` | Newer config/multi-ripple wrappers around the same shader |
| `animation/.../shaderutil/SdfShaderLibrary.kt` | Signed-distance functions for circle / ellipse / rounded box |
| `animation/.../shaderutil/ShaderUtilLibrary.kt` | Noise and sparkle helpers used by the shader |
