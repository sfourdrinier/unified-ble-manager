# Unified BLE Manager mark

`ubm-mark.svg` is the canonical, resolution-independent mark. The Web example
builds it into its favicon and the project README references it directly.
`ubm-mark-512.png` is the raster app icon used by
the repository's Tauri proof app and the packed external Tauri consumer;
`ubm-mark-32.png` is the small icon. They are derivatives, not separate designs.

The mark is distinct from the inherited React Native BLE PLX artwork retained
in `docs/logo.png`. Do not reuse that historical logo for new 5.x apps.

On macOS, regenerate both PNGs from the SVG with:

```sh
sips -s format png -z 512 512 assets/brand/ubm-mark.svg --out assets/brand/ubm-mark-512.png
sips -s format png -z 32 32 assets/brand/ubm-mark.svg --out assets/brand/ubm-mark-32.png
```

The package gate checks dimensions and consumers of these assets. The icon
does not imply platform certification or any support qualification.
