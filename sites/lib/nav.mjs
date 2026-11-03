/* Filesystem navigation for the docs tree. Generator-agnostic: it reads the
 * markdown files, takes each page title from its first h1, and maps README.md to
 * its directory. Nothing here knows about a site generator. */

import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs'
import { join } from 'node:path'

/** First h1 of a page, falling back to the file name. */
export function titleOf(file) {
	const text = readFileSync(file, 'utf8')
	const heading = /^#\s+(.+?)\s*$/m.exec(text)
	return heading ? heading[1] : file.slice(file.lastIndexOf('/') + 1).replace(/\.md$/, '')
}

/** Site route for a markdown path relative to the docs root. */
export function routeOf(relativePath) {
	const withoutReadme = relativePath.replace(/(^|\/)README\.md$/, '$1').replace(/\.md$/, '')
	return withoutReadme === '' ? '/' : `/${withoutReadme}`
}

/** Sections read in this order; anything else follows alphabetically. */
const sectionOrder = ['usage', 'advanced', 'reference', 'dev']

/* Repository evidence rather than pages: these directories cite logs and source
 * files outside the docs root, which the site cannot serve. */
const archiveDirs = new Set(['dev/reviews'])

function bySection(a, b) {
	const rank = (name) => {
		const index = sectionOrder.indexOf(name)
		return index === -1 ? sectionOrder.length : index
	}
	const difference = rank(a) - rank(b)
	return difference !== 0 ? difference : a.localeCompare(b)
}

/** Nested VitePress sidebar items for a docs directory. */
export function navEntries(srcDir, base = '') {
	const dir = base === '' ? srcDir : join(srcDir, base)
	const entries = readdirSync(dir)
	const pages = entries.filter((name) => name.endsWith('.md') && name !== 'README.md').sort()
	const dirs = entries.filter((name) => statSync(join(dir, name)).isDirectory()).sort(bySection)

	const items = []
	/* The docs root is the page to start at, so it opens the navigation. */
	if (base === '') {
		const readme = join(srcDir, 'README.md')
		if (existsSync(readme)) items.push({ text: titleOf(readme), link: '/' })
	}
	items.push(
		...pages.map((name) => {
			const relativePath = base === '' ? name : `${base}/${name}`
			return { text: titleOf(join(srcDir, relativePath)), link: routeOf(relativePath) }
		}),
	)

	for (const name of dirs) {
		const relativePath = base === '' ? name : `${base}/${name}`
		if (archiveDirs.has(relativePath)) continue
		const readme = join(srcDir, relativePath, 'README.md')
		const children = navEntries(srcDir, relativePath)
		/* Directories that hold no pages, such as a folder of probe transcripts, are not navigation. */
		if (children.length === 0 && !existsSync(readme)) continue
		items.push({
			text: existsSync(readme) ? titleOf(readme) : name.charAt(0).toUpperCase() + name.slice(1),
			collapsed: false,
			items: children,
		})
	}

	return items
}

/** Every README.md under the docs root, mapped to the index page the site serves. */
export function readmeRewrites(srcDir, base = '') {
	const dir = base === '' ? srcDir : join(srcDir, base)
	const rewrites = {}

	for (const name of readdirSync(dir).sort()) {
		const relativePath = base === '' ? name : `${base}/${name}`
		if (name.endsWith('.md')) {
			if (name === 'README.md') rewrites[relativePath] = `${base === '' ? '' : `${base}/`}index.md`
			continue
		}
		if (statSync(join(dir, name)).isDirectory()) Object.assign(rewrites, readmeRewrites(srcDir, relativePath))
	}

	return rewrites
}
