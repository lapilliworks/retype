# Product and documentation sites

Two apps, one deployable tree. `product/` is the static page at the root; `manual/` builds the manual from `docs/` with VitePress and sits under `/docs/` in the bundle.

```
sites/
	README.md
	bin/site.sh               Entry point, runs one app in a container
	bin/dev.mjs               Runs the product and the manual together
	bin/entrypoint.sh         Runs inside the container: install, then generate
	bin/bundle.sh             Builds both apps into sites/dist
	lib/nav.mjs               Navigation and titles, read from the filesystem
	lib/checks.mjs            Guards for docs/, generator-independent
	product/                  Product page: static HTML, CSS and vanilla JS
	                          plus install.sh and install.ps1, copied from packaging/ at build time
	manual/                   VitePress configuration for the manual
```

`docs/` contains plain CommonMark: no front matter, no components, no build artifacts. Page titles are the first `h1`, navigation is the directory tree, and `README.md` is the index of its directory. A page can be read in an editor, on the forge, and on the site without any of them being the source of truth.

The product page has no generator and no dependencies. It is a static page, a stylesheet and a small script, so it needs nothing installed beyond Node and it makes no external requests.

## Running it

```
sites/bin/site.sh dev                   # product at /, manual at /docs/
sites/bin/site.sh dev product           # product only
sites/bin/site.sh --host 0.0.0.0 dev    # the same, also reachable from the network
sites/bin/site.sh build product         # writes sites/product/dist
SITE_BASE=/docs/ sites/bin/site.sh build manual # writes sites/manual/dist
sites/bin/bundle.sh                     # writes sites/dist
sites/bin/site.sh shell                 # shell inside the container
sites/bin/site.sh clean manual          # drop volumes and dist
```

The dev command serves the deployed shape on one origin and one port: the product page at `/` and the manual at `/docs/`. The manual runs on an internal port and the product server proxies `/docs/` to it, including the live reload socket. Only the product port is published, and no second command or public port is involved. The manual runs with `SITE_BASE=/docs/` and `SITE_CLEAN_URLS=false`, so its links match the bundle.

`SITE_DOCS_URL` is the internal manual upstream the product server proxies to. It defaults to `http://127.0.0.1:5174/`; `SITE_MANUAL_PORT` moves that internal port without publishing another one.

`SITE_BASE` is the manual's base path. Leave it unset for a manual served from the root on its own; set it to `/docs/` when it sits under the product site. `SITE_CLEAN_URLS=false` emits `.html` links, which a plain static host can serve; the bundle sets it. `SITE_PRODUCT_URL` is the link the manual's navigation uses for the product page; a build defaults it to `https://justretype.com/`, and the combined dev command defaults it to the local product origin.

`bundle.sh` builds the product page at the root and the manual under `/docs/`, then writes one tree to `sites/dist`. The manual is built with `SITE_CLEAN_URLS=false`, so the tree works on a static host without rewrite rules.

The generator never runs on the host. `site.sh` picks `podman` or `docker`, or `SITE_RUNTIME` if both are installed, and runs the stock `node:22-bookworm-slim` image. There is no image of our own and no `Dockerfile`: the container gets its toolchain from the stock image and everything else from the mounted working tree, where `sites/bin/entrypoint.sh` is the entrypoint. Dependencies install into a named volume on first run; the dev cache and the build output live in volumes and in `/tmp`, so the only things the build exports into the working tree are the app `dist` directories. `bundle.sh` then assembles them into `sites/dist` and hands the result back owned by the calling user.

To add another app, copy the `manual/` layout into `sites/<name>/` with a `package.json` exposing `dev` and `build`; `site.sh dev <name>` works unchanged. An app's `dev` script owns its listening ports, and `SITE_PORT` sets the host port it is published on.

## Reaching the dev server

The port is published on the host's loopback, so `http://127.0.0.1:5173/` works and nothing else does. That is deliberate for a workstation. `--host <address>` (or `SITE_HOST` in the environment, which the flag overrides) moves the publish elsewhere; `--host 0.0.0.0` publishes on every interface. Three consequences worth knowing:

- To open the site from another machine, `sites/bin/site.sh --host 0.0.0.0 dev`, then browse to this machine's address
- `localhost` is not the same address as `127.0.0.1`. If the browser resolves `localhost` to `::1` first and nothing answers, the connection stalls; use the literal `127.0.0.1`
- Through a proxy or a hosts entry, the request carries a domain as its `Host` header, and Vite refuses any host it does not know: `Blocked request. This host ("...") is not allowed`. The allowlist is `.retype.test` and `.justretype.com`, where a leading dot matches every subdomain, and `SITE_ALLOWED_HOSTS` replaces it with a comma separated list

A reverse proxy on a port other than the one Vite listens on also needs `vite.server.hmr.clientPort`, or the live reload socket connects to the wrong port.

Docker and podman report a failed publish immediately (`port is already allocated`, `address already in use`), so a silent absence of the site is a container that exited. `site.sh dev` runs in the foreground: read its output.

## Rules

`docs/` is written for readers, not for the generator. Four rules hold it that way, and the third is checked:

- The docs home names every section and says what is in it, in prose rather than as a second list, because the sidebar already lists every page
- A page is titled for what it contains, since the title is what the navigation shows
- A link points inside the docs tree and is named by subject, never by file name
- No front matter, no components, no build artifacts in `docs/`
- The safety review archive under `docs/dev/reviews` is repository evidence rather than a page: it cites logs and source files outside the docs root, so navigation, the build and the checks all skip it

Page titles, navigation and ordering are all derived from the markdown as it stands, which is what lets the same files be read in an editor, on the forge and on the site.

## Checks

```
node sites/lib/checks.mjs
```

Two rules, both about the source rather than about a rendering of it.

Every relative link resolves inside `docs/`. A generator cannot serve, rewrite, or even see anything above its root, so a link that escapes the root is broken for every reader that is not the site.

No link is labelled with a file name. A reader who is offered `grammar.md` learns nothing; the same link offering "Retype intent grammar" says what is on the other side.

## Deploying

`.github/workflows/site.yml` builds the bundle on `master` and uploads `sites/dist` to Cloudflare Pages as a direct upload: Cloudflare receives the finished files and never runs the build. The Pages project is `retype`, and `master` is its production branch. The workflow reads `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` from repository secrets.

The bundle is served from the root of the domain, so the product app keeps the default `SITE_BASE` and the manual is built with `/docs/`. A deployment under a path prefix would need `SITE_BASE` set for the product app as well.

`ci.yml` builds the same bundle on every push, so a docs build that breaks fails before it reaches the default branch.

## Why not Starlight

Checked against `@astrojs/starlight` 0.42.8, not from memory:

- `docsLoader` has no `srcDir` option; it reads `src/content/docs` and nothing else, so `docs/` needs either a symlink into the project or a hand-written glob loader
- `title` is a required field of the front matter schema (`title: z.ZodString`), so all twenty pages would need front matter, and the generated `docs/dev/measurement/events.md` would have to emit it
- Relative `.md` links need a remark plugin

Each of those is one decision away from "the site reads docs/" and lands on "the site reads a copy of docs/ with metadata added". That is the trade the plain-markdown premise forbids.

VitePress needs three pieces of glue for the same source, all inside `sites/lib/nav.mjs` and the config: `srcDir`, a `rewrites` map because its router only treats `index.md` as a directory index (`indexRE` matches `index.md`, never `README.md`), and a markdown rule that turns `README.md` links into directory links. Its `.md` link rewriting, h1 titles and local search are built in, and it has no front matter requirement.

## Open

- `docs/dev/measurement/probe/` holds transcripts, so it is not a page and not a link target
- The safety review archive under `docs/dev/reviews/` is repository evidence, not a page, and is excluded from the site
- The product page's social card is SVG. Some social platforms want a raster image; a PNG is a deployment follow-up
- `SITE_BASE` is `/` by default and `/docs/` in the bundle. A deployment under another path needs it set
- The Pages project and its secrets do not exist in the repository; [Deploying](#deploying) lists what has to be created
