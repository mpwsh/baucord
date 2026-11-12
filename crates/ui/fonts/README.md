# Inter

The app's typeface: [Inter](https://github.com/rsms/inter) 4.1 by The
Inter Project Authors, SIL Open Font License 1.1
(`licenses/native/inter-OFL.txt`; listed in Settings → Licenses).

Only the weights the UI uses (Regular, Medium, SemiBold, Bold, Italic,
SemiBold Italic), each cut from the release's `extras/ttf` to the
scripts the app shows and without hinting, about 180 KB a file:

    pyftsubset Inter-Regular.ttf --no-hinting --name-IDs='*' \
      --layout-features='kern,liga,calt,ccmp,locl,mark,mkmk,tnum,case' \
      --unicodes='U+0000-024F,U+02B0-02FF,U+0300-036F,U+0370-03FF,U+0400-04FF,U+1E00-1EFF,U+2000-206F,U+2070-209F,U+20A0-20CF,U+2100-214F,U+2150-218F,U+2190-21FF,U+2200-22FF,U+2300-23FF,U+2460-24FF,U+2500-25FF,U+2600-26FF,U+2700-27BF,U+FB00-FB06,U+FEFF,U+FFFD' \
      --output-file=Inter-Regular.ttf

A character outside those ranges (CJK, Arabic, emoji…) is drawn from
the system's fonts. A weight the UI starts to use needs its file here
and an `import` in `slint/main.slint`.
