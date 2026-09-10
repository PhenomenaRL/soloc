# Local texture overrides

Planet textures load from the jsDelivr CDN by default
(`jeromeetienne/threex.planets`, 1k). Drop a file in this folder to override
one — local files always win, and nothing else needs changing.

## The resolution ladder

Every body looks for these names in order and takes the first that is present,
falling back to the CDN baseline:

```
<stem>8k.jpg  →  <stem>4k.jpg  →  <stem>2k.jpg  →  <CDN baseline>
```

| Body    | stem          | CDN baseline      | bump stem     | bump baseline      |
|---------|---------------|-------------------|---------------|--------------------|
| Sun     | `sunmap`      | `sunmap.jpg`      | —             | —                  |
| Mercury | `mercurymap`  | `mercurymap.jpg`  | `mercurybump` | `mercurybump.jpg`  |
| Venus   | `venusmap`    | `venusmap.jpg`    | `venusbump`   | `venusbump.jpg`    |
| Earth   | `earthmap`    | `earthmap1k.jpg`  | `earthbump`   | `earthbump1k.jpg`  |
| Moon    | `moonmap`     | `moonmap1k.jpg`   | `moonbump`    | `moonbump1k.jpg`   |
| Mars    | `marsmap`     | `marsmap1k.jpg`   | `marsbump`    | `marsbump1k.jpg`   |
| Jupiter | `jupitermap`  | `jupitermap.jpg`  | —             | —                  |
| Saturn  | `saturnmap`   | `saturnmap.jpg`   | —             | —                  |
| Uranus  | `uranusmap`   | `uranusmap.jpg`   | —             | —                  |
| Neptune | `neptunemap`  | `neptunemap.jpg`  | —             | —                  |

**Name by actual pixel size, not by the vendor's download tier.** Solar System
Scope labels several 4096×2048 maps "8k"; calling one `…8k.jpg` would make the
ladder prefer it over a genuinely larger file.

Any equirectangular map works, provided it follows the usual convention: north
pole at the top, prime meridian down the middle, longitude increasing eastward
to the right. `src/scene/textures.test.ts` pins that convention.

## Saturn's rings

Two layouts are accepted:

- `saturnring8k.png` — one RGBA image, colour and transparency together. This is
  how Solar System Scope ships them, and it wins if present.
- `saturnringcolor.jpg` + `saturnringpattern.gif` — the CDN baseline, which
  supplies the alpha as a separate map.

## Where to get maps

Neither of these has been verified from this repo — fetch them in a browser or
shell with normal internet access.

- **NASA CGI Moon Kit** — LRO/LOLA derived, public domain, the best free lunar
  map there is, with a matching elevation map: <https://svs.gsfc.nasa.gov/4720>
- **Solar System Scope** — 2k/8k for every planet, CC BY 4.0 (attribution
  required): <https://www.solarsystemscope.com/textures/>

Take the **elevation map** alongside the colour map where one exists. An 8k
colour map over a 1k bump map is a mismatch you can see: fine surface detail
with coarse blobby relief stamped across it. A partly-downloaded file is
handled — the loader tries each candidate and falls through on a decode failure
— but a mismatched pair still looks wrong.

An 8k JPEG runs 5–15 MB. Decide whether you want that in git — add
`visualizer/public/textures/*.jpg` to `.gitignore` if not.

## Not currently read

- `venussurface8k.jpg` — the Magellan radar surface. Venus is drawn as it
  *looks*, which is the cloud tops, so this sits unused. Rename it to
  `venusmap8k.jpg` if you would rather see through the atmosphere.

## Everything else (1k, CDN baseline)

`earthcloudmap.jpg` · `earthcloudmaptrans.jpg` · `galaxy_starfield.png`

Overriding one of these works too — use the exact filename.

Default texture credit: James Hastings-Trew — planetpixelemporium.com.
Free to use in projects; not to be redistributed as standalone textures.
Solar System Scope textures are CC BY 4.0 and require attribution if you ship
them.
