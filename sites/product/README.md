# Retype product site

The page at the root of `sites/dist`, and the reason the manual lives under `/docs/`. Static HTML, CSS and vanilla JS: no generator, no dependencies, and no external requests.

```
sites/product/
	index.html
	404.html
	robots.txt
	assets/
		styles.css
		app.js
	public/
		favicon.svg
		mark.svg
		mark-reversed.svg
		lapilli-wordmark.svg
		lapilli-wordmark-reversed.svg
		og.svg
	bin/serve.mjs       Dev server, serves / and proxies /docs/ to the manual
	bin/build.mjs       Copies the static files to SITE_OUT_DIR
```

## Running it

```
sites/bin/site.sh dev          # product at /, manual at /docs/
sites/bin/site.sh dev product  # product only
```

The dev server proxies `/docs/` to the manual's internal dev server, including the live reload socket, so both sites share one origin and one public port.

## Building it

```
sites/bin/site.sh build product
SITE_BASE=/docs/ sites/bin/site.sh build manual
sites/bin/bundle.sh
```

`bundle.sh` runs both builds and assembles `sites/dist`: the product page at the root, the manual at `/docs/`. The page's manual links are absolute `/docs/...html` paths, so the bundle is served from the host root.

The build copies `packaging/install.sh` and `packaging/install.ps1` to `/install.sh` and `/install.ps1`, so the page commands and the release assets are the same files.

## Notes

- The social card is `public/og.svg`. Some social platforms want a raster image; a PNG is a deployment follow-up, not a build dependency
- The page states the release status, the installers and the paid desktop app. Keep those claims aligned with `README.md` and `docs/`
- The installers are copied from `packaging/` at build time; edit the sources there, not the output
