/* VitePress config for the retype documentation site.
 *
 * docs/ is the only source: this directory holds configuration, never content.
 * Titles come from each page's first h1, navigation comes from the filesystem,
 * and README.md is treated as the index of its directory. No front matter. */

import { fileURLToPath } from 'node:url'
import { join } from 'node:path'
import { defineConfig } from 'vitepress'
import { navEntries, readmeRewrites } from '../../lib/nav.mjs'

const docs = fileURLToPath(new URL('../../../docs', import.meta.url))

/* Served at the root on its own, or under /docs/ beside the product site. */
const configuredBase = process.env.SITE_BASE ?? '/'
const base = configuredBase.endsWith('/') ? configuredBase : `${configuredBase}/`
const productUrl = process.env.SITE_PRODUCT_URL ?? 'https://retype.name/'
const cleanUrls = process.env.SITE_CLEAN_URLS !== 'false'

/* The dev server is reached through a domain, and Vite refuses Host headers it does
 * not know, which is what stops DNS rebinding. A leading dot matches every subdomain. */
const allowedHosts = (process.env.SITE_ALLOWED_HOSTS ?? '.retype.test,.retype.name')
	.split(',')
	.map((host) => host.trim())
	.filter((host) => host !== '')

/* A README.md is a directory index everywhere except in the site's router. */
function indexLinks(markdown) {
	markdown.core.ruler.push('readme-index-links', (state) => {
		for (const block of state.tokens) {
			if (block.type !== 'inline' || !block.children) continue
			for (const token of block.children) {
				if (token.type !== 'link_open') continue
				const href = token.attrGet('href')
				if (!href || !/(^|\/)README\.md(#.*)?$/.test(href)) continue
				const target = href.replace(/(^|\/)README\.md/, '$1')
				token.attrSet('href', target === '' ? './' : target)
			}
		}
	})
}

/* Vite resolves a page's bare imports from the page upward, and docs/ is not under
 * this app directory, so the two Vue entry points a page module imports are pinned
 * here. The app directory is npm's working directory. */
const vue = join(process.cwd(), 'node_modules/vue/dist/vue.runtime.esm-bundler.js')
const vueServerRenderer = join(process.cwd(), 'node_modules/vue/server-renderer/index.mjs')

export default defineConfig({
	title: 'Retype',
	description: 'Convert a file by renaming it',
	lang: 'en-GB',
	base,
	srcDir: '../../docs',
	/* Repository evidence, not pages: the safety review archive cites logs and source
	 * outside the docs root, so the site does not build it. */
	srcExclude: ['dev/reviews/**'],
	cleanUrls,
	rewrites: readmeRewrites(docs),
	/* Keeps the working tree clean: the dev server writes its cache outside the repository. */
	cacheDir: process.env.SITE_CACHE_DIR ?? '/tmp/vitepress-cache',
	outDir: process.env.SITE_OUT_DIR ?? '/tmp/vitepress-dist',
	markdown: { config: indexLinks },
	vite: {
		resolve: {
			alias: [
				{ find: /^vue$/, replacement: vue },
				{ find: /^vue\/server-renderer$/, replacement: vueServerRenderer },
			],
		},
		server: { allowedHosts },
	},
	themeConfig: {
		nav: [{ text: 'Product', link: productUrl }],
		sidebar: navEntries(docs),
		outline: { level: [2, 3] },
		search: { provider: 'local' },
	},
})
