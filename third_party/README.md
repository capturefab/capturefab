# Build-only AMF headers

`amf-headers-1.5.3.tar.gz` contains unmodified AMD AMF headers and the upstream MIT license from official release `v1.5.3`, commit `8c648005e07d4309033282bfd9947df2c7e76104`. No SDK sample executables are included. It is used only when building the optional FFmpeg AMF backend.

SHA-256: `65e06bbbc515c3125cffd89fe0a3639a2fedc4d8c7423fc82a60218295a3cc31`.

Reproduce from the [official repository](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/tree/8c648005e07d4309033282bfd9947df2c7e76104):

```sh
git archive --format=tar --prefix=amf-headers-1.5.3/ \
  8c648005e07d4309033282bfd9947df2c7e76104 amf/public/include LICENSE.txt \
  | gzip -n > amf-headers-1.5.3.tar.gz
```

# Phosphor icon fonts

`assets/fonts/Phosphor.ttf` and `assets/fonts/Phosphor-Fill.ttf` are the unmodified regular and fill weights from [`@phosphor-icons/web`](https://www.npmjs.com/package/@phosphor-icons/web) 2.1.2 (`src/regular`, `src/fill`), MIT licensed; the license is `assets/fonts/Phosphor-LICENSE.txt`. The desktop GUI embeds them for its icons, and release packages append the license to `THIRD-PARTY-LICENSES.txt`. Codepoints in `src/gui/icon.rs` come from the package's `style.css`.

SHA-256: `06b91e022b7ee899a63efced879392a74f0bacbda54e4467e9f663220d173a10` (Phosphor.ttf), `a53f5d2630cab5e3b7536ecb9d69d71519a2190298c22b1f8d770dd37bc2940a` (Phosphor-Fill.ttf).
