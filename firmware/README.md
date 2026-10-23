# Firmware this project does not distribute

The Keysight 82357B boots blank and takes a firmware image over USB every time it is plugged
in. The image belongs to Keysight, and this project has no right to pass it on, so it is not in
this repository (see `.gitignore`). To use an 82357B, provide the file yourself.

| File | For | Where from |
|---|---|---|
| `measat_releaseX1.8.hex` | 82357B | linux-gpib's firmware, https://github.com/fmhess/linux_gpib_firmware, `agilent_82357a/` |

The daemon looks for it, in order: `$SYMBIOTIC_KS_FIRMWARE`, `firmware/` in the configuration
directory (`%LOCALAPPDATA%\Symbiotic`, `C:\ProgramData\Symbiotic` after an all-users install,
`~/.config/symbiotic`), `firmware/measat_releaseX1.8.hex` beside `symbioticd`, the same file beside
`symbioticd`, then, for a daemon built in a source tree (`target/.../release` or `debug`), this
`firmware/`. The daemon runs in its own directory, so the caller's working directory is not searched.
If it is missing the adapter is left out of `viFindRsrc` and the reason is written to
`symbioticd.log`.

The first upload to an 82357B always fails: the adapter disconnects, returns still blank
(product id `0x0518`), and takes the same image on the second try, coming back as `0x0718`.
The loader (`ks.rs`) uploads twice.
