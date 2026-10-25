# iOS app icon

`Assets.xcassets/AppIcon.appiconset` uses the existing 1024px headphones master
from `../macos/trash-amp.png`. Its transparent outer margin is composited over
charcoal for an opaque iOS icon; iOS supplies the final corner mask.

To regenerate the platform asset with ImageMagick:

```sh
magick assets/macos/trash-amp.png -background '#181817' -alpha remove -alpha off \
  assets/ios/Assets.xcassets/AppIcon.appiconset/AppIcon.png
```

Run this from the repository root. Xcode compiles the asset catalog through the
project's Resources phase. `scripts/bundle-ios.sh` uses `actool` and merges its
generated icon declarations into the standalone bundle's Info.plist. Both debug
and release builds use the icon.
