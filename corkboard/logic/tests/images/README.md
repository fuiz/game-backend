# Test images

`animated.webp` is taken from the [image](https://github.com/image-rs/image) test suite
(`tests/images/webp/extended_images/anim.webp`), which is licensed MIT OR Apache-2.0. An animated
WebP cannot be produced with the `image` encoders, so it is checked in rather than generated.

`metadata.avif`, `animated.avif` and `still.heic` are 32x24 images made with `avifenc` and
`heif-enc`. `metadata.avif` is rotated with an `irot` property and carries EXIF with a GPS pointer
and XMP with a GPS latitude, so that removing them can be tested.
