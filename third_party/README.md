# Build-only AMF headers

`amf-headers-1.5.3.tar.gz` contains unmodified AMD AMF headers and the upstream MIT license from official release `v1.5.3`, commit `8c648005e07d4309033282bfd9947df2c7e76104`. No SDK sample executables are included. It is used only when building the optional FFmpeg AMF backend.

SHA-256: `65e06bbbc515c3125cffd89fe0a3639a2fedc4d8c7423fc82a60218295a3cc31`.

Reproduce from the [official repository](https://github.com/GPUOpen-LibrariesAndSDKs/AMF/tree/8c648005e07d4309033282bfd9947df2c7e76104):

```sh
git archive --format=tar --prefix=amf-headers-1.5.3/ \
  8c648005e07d4309033282bfd9947df2c7e76104 amf/public/include LICENSE.txt \
  | gzip -n > amf-headers-1.5.3.tar.gz
```
