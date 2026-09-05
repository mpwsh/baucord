# Gamut and Display P3

Every sRGB color exists in Display P3, but not the reverse. P3 covers roughly 50% more colors, which matters only for the most saturated values. A color at 60% of maximum vividness looks the same on both.

## Max chroma varies by lightness and hue

The gamut boundary is irregular; a ramp that clips does not clip evenly across steps. At L=0.5 in sRGB:

- Highest chroma: purple (H ≈ 285) at C ≈ 0.29
- Red-orange (H ≈ 0–30): C ≈ 0.20
- Lowest chroma: cyan (H ≈ 195) at C ≈ 0.09

The peak hue shifts with lightness — at L=0.7 magenta peaks, at L=0.9 green peaks — and cyan stays the lowest at nearly every lightness. That's why a ramp built by copying one chroma value across hues clips at some steps and not others: cyans and yellows top out far lower than reds and purples.

## Clamping

A color more vivid than its display can render gets clipped, and clipping is not graceful. It flattens neighbouring steps into one rendered color, so the top of a ramp can lose its distinctions on an sRGB screen.

The fix is to reduce chroma while holding lightness and hue constant:

```css
/* Out of sRGB gamut */
oklch(0.7 0.35 150)

/* Clamped to max chroma for this L/H */
oklch(0.7 0.22 150)
```

Generate ramps against sRGB unless the product is display-restricted, and add P3 as an enhancement.

## CSS fallback patterns

```css
.accent {
  background: #3b82f6;
}

@media (color-gamut: p3) {
  .accent {
    background: oklch(0.62 0.24 259);
  }
}
```

Order matters. The sRGB value comes first so every display gets something, and the P3 rule overrides only where it will render. A P3 color with no fallback is a `HIGH` finding; it does not degrade, it fails.

For browser matrices predating `oklch()` support, layer `@supports` underneath the same structure:

```css
.accent {
  background: #3b82f6;
}

@supports (color: oklch(0 0 0)) {
  .accent {
    background: oklch(0.62 0.19 259);
  }

  @media (color-gamut: p3) {
    .accent {
      background: oklch(0.62 0.24 259);
    }
  }
}
```

Check the project's actual browser matrix before adding this. On a modern baseline it is dead weight.

## Tailwind v4

Tailwind v4 defines its default palette in oklch. Custom themes should follow the same convention, declared inside `@theme`:

```css
@theme {
  --color-brand-50: oklch(0.971 0.012 250);
  --color-brand-100: oklch(0.932 0.028 250);
  --color-brand-200: oklch(0.882 0.048 250);
  --color-brand-300: oklch(0.812 0.078 250);
  --color-brand-400: oklch(0.722 0.148 250);
  --color-brand-500: oklch(0.623 0.188 250);
  --color-brand-600: oklch(0.535 0.168 250);
  --color-brand-700: oklch(0.445 0.138 250);
  --color-brand-800: oklch(0.362 0.108 250);
  --color-brand-900: oklch(0.289 0.078 250);
  --color-brand-950: oklch(0.215 0.048 250);
}
```

This yields `bg-brand-500`, `text-brand-200` and so on automatically. Opacity modifiers work the same way as elsewhere — `bg-brand-500/50` compiles to `oklch(0.623 0.188 250 / 0.5)` — but see [contrast.md](contrast.md) on why an alpha color can't be statically contrast-checked.

**Migrating an existing theme:**

1. Convert all hex values in `@theme` to oklch.
2. Replace any `theme()` references that used hex.
3. Test dark mode — oklch values may look slightly different due to perceptual accuracy.
4. Check for hardcoded hex in component code and convert those too.
