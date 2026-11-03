# Retype product site

A standalone static product page for Retype, made by Lapilli Works. No generator, no dependencies, and no external requests.

```
	sites/product/
		index.html
		404.html
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
			robots.txt
		bin/serve.mjs
```

## Running it

```
	bun run dev                 # http://127.0.0.1:4173/
	SITE_PORT=8080 bun run dev
```

The server is dependency-free and serves the directory as the site root. The manual is expected at `/docs/` when the bundle is deployed; a standalone preview has no `/docs/` tree, so those links lead to a 404 there.

## Notes

- All content is sourced from the manual in `docs/`; update the page when the product's documented status changes
- The page is pre-alpha, and says so. Do not promote declared-but-unbuilt tokens as working
- Social cards are SVG. Some platforms want a raster image; a PNG is a deployment follow-up
