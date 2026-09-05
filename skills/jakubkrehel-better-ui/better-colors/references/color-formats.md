# Color formats

Which notation to write colors in, and how to convert between them. Every other rule in this skill is stated perceptually and holds whatever notation you write. Gamut clipping and Display P3 have moved to [gamut.md](gamut.md).

## Choosing a notation

| Notation | Good for | Weakness |
| --- | --- | --- |
| Hex | Universal support, compact, what design tools hand you | Opaque. No channel is readable or editable by hand |
| `rgb()` | Same reach as hex, readable alpha | Channels do not correspond to anything a designer thinks about |
| `hsl()` | Channels look like design controls | Its lightness is not perceptual and its hue drifts; a ramp built by varying lightness bunches at one end and shifts hue |
| `oklch()` | Perceptually uniform lightness, stable hue, predictable ramps | Baseline 2023, so very old browser matrices need a fallback |

**Match whatever the project already uses.** Notation is not a defect: a project on hex is not doing it wrong.

**For a genuinely new color system, `oklch()` is the best default.** Equal L steps read as equal brightness, and hue holds constant across the whole lightness range — the reason its numbers behave the way [palette-generation.md](palette-generation.md) describes. See [palette-generation.md](palette-generation.md).

```css
oklch(L C H)          /* lightness 0–1, chroma 0–~0.4, hue 0–360 */
oklch(L C H / alpha)  /* alpha uses a slash, never a comma */
```

L and C use 3 decimal places, H uses up to 3. Drop trailing zeros. Format `-0` as `0`.

## Supported input formats

| Format | Examples |
| --- | --- |
| Hex (3/6/8-digit) | `#f00`, `#ff0000`, `#ff000080` |
| `rgb()` / `rgba()` | `rgb(255, 0, 0)`, `rgba(255, 0, 0, 0.5)` |
| `hsl()` / `hsla()` | `hsl(0, 100%, 50%)`, `hsla(0, 100%, 50%, 0.5)` |

## When to convert

Convert when the user asks, when an agreed migration is in scope, or when the project is standardizing on a notation and this value is the straggler. Never convert an isolated value in a project that deliberately uses something else, and never just because this skill happened to load.

Bulk conversion is a migration, not cleanup. It shifts every rendered color by a rounding margin and touches files nobody asked about, so it has to be the task rather than a side effect. When it is in scope:

1. Replace all hex colors with their oklch equivalents.
2. Replace all `rgb()`, `rgba()`, `hsl()`, `hsla()` function calls.
3. Leave gradient functions unchanged — only convert the color stops inside them, not the interpolation method.
4. Leave CSS keywords alone: `currentColor`, `inherit`, `transparent`, `initial`, `unset`.
5. Leave colors in third-party configs that expect a specific format.
6. Preserve comments and formatting.

```css
/* Before */
color: #3b82f6;
background: #1e293b;
border-color: #e2e8f0;
border: 1px solid rgba(0, 0, 0, 0.1);

/* After */
color: oklch(0.623 0.188 259.815);
background: oklch(0.279 0.037 260.031);
border-color: oklch(0.929 0.013 255.508);
border: 1px solid oklch(0 0 0 / 0.1);
```

Alpha uses the forward-slash syntax. Omit alpha when it's 1.

## Modern CSS worth knowing

- **`color-mix()`** derives one color from another, as in `color-mix(in oklab, var(--color-accent-solid) 15%, white)` for a tinted background. Useful for states, but keep generated values out of the token layer, since a mixed color cannot be inspected in a design tool.
- **Relative color syntax** adjusts one channel of an existing color. `oklch(from var(--color-accent-solid) calc(l - 0.1) c h)` darkens the accent by hand. Powerful and easy to overuse, since a token defined by three chained derivations is unreadable.
- **`light-dark()`** puts both appearances in one declaration. See [palette-generation.md](palette-generation.md).

All three compute at render time, so their output cannot be contrast-checked statically. Measure the rendered result ([contrast.md](contrast.md)).
