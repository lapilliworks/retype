# Native watch fixture

`photo.heic` is a synthetic 40 by 32 orange image, generated with ImageMagick 7.1.2-34 using `magick -size 40x32 xc:orange photo.heic`.

`oriented-8bit.heic` is a synthetic 96 by 64 HEVC Main Still Picture image with four colored quadrants and an HEIF rotation property, not an EXIF orientation tag. Its displayed image is 64 by 96, with green, red, yellow and blue quadrants in reading order. It was generated with ImageMagick 7.1.2-34:

```sh
magick -size 96x64 xc:white \
	-fill red -draw 'rectangle 0,0 47,31' \
	-fill blue -draw 'rectangle 48,0 95,31' \
	-fill green -draw 'rectangle 0,32 47,63' \
	-fill yellow -draw 'rectangle 48,32 95,63' \
	-depth 8 -orient RightTop -quality 95 oriented-8bit.heic
```

Both fixtures are checked in so HEIC-to-JPEG tests need an HEIC decoder, not an HEIC encoder.
