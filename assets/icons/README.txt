CAROUSEL — ICON EXPORT
=======================

carousel-icon-master.svg   Full-colour vector master (full-bleed square,
                            no pre-rounded corners — iOS/Android apply
                            their own corner mask, don't round it yourself).

carousel-icon-mono.svg     White-on-transparent vector, for notification
                            trays, status bars, and watch faces.

/ios/                      PNGs at every size Xcode's asset catalog asks
                            for: 1024 (App Store), 180/120/87/80/60/58/40/29/20
                            (iPhone @1x–3x), 167/152/76 (iPad).

/android/                  PNGs for res/mipmap buckets: 512 (Play Store
                            listing), 192 (xxxhdpi), 144 (xxhdpi), 96 (xhdpi),
                            72 (hdpi), 48 (mdpi).

/notification/             Monochrome PNGs at 1024/180/120/60 — Android
                            notification icons must be pure white silhouettes
                            on transparent, which is exactly what these are.

Both SVGs are edited by hand if you ever need a colour tweak — open in
Figma/Illustrator/Inkscape, they're plain vector shapes with two gradients.
