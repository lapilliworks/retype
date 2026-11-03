/* Guards for docs/ that no site generator provides, because they are about the
 * source rather than about a rendering of it: every relative link resolves inside
 * the docs root, and no link is labelled with a file name. */

import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs'
import { dirname, join, relative, resolve, sep } from 'node:path'

/* Repository evidence rather than pages: the safety review archive cites logs and
 * source files outside the docs root, which the site cannot serve. */
const archive = ['dev', 'reviews'].join(sep)

const LINK = /\[([^\]]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g
function markdownFiles(dir, base = dir) {
	const files = []
	for (const name of readdirSync(dir).sort()) {
		const path = join(dir, name)
		if (statSync(path).isDirectory()) files.push(...markdownFiles(path, base))
		else if (name.endsWith('.md')) files.push(relative(base, path))
	}
	return files
}

function isInternal(target) {
	return !/^[a-z][a-z0-9+.-]*:/i.test(target) && !target.startsWith('#')
}

function linkTargets(text) {
	return [...text.matchAll(LINK)].map((match) => match[2])
}

function checkTarget(docsDir, fromFile, target) {
	const withoutAnchor = target.replace(/[?#].*$/, '')
	const absolute = resolve(docsDir, dirname(fromFile), withoutAnchor)
	if (relative(docsDir, absolute).startsWith('..')) return { ok: false, reason: 'escapes the docs root' }
	if (withoutAnchor.endsWith('.md')) {
		return existsSync(absolute) ? { ok: true } : { ok: false, reason: 'no such file' }
	}
	if (existsSync(absolute) && statSync(absolute).isDirectory()) {
		const index = ['README.md', 'index.md'].find((name) => existsSync(join(absolute, name)))
		return index ? { ok: true } : { ok: false, reason: 'directory has no README.md or index.md' }
	}
	return { ok: false, reason: 'no such file or directory' }
}

/** Every relative link in the tree resolves inside the tree. */
export function checkLinks(docsDir) {
	const failures = []
	let checked = 0

	for (const file of markdownFiles(docsDir)) {
		if (file.startsWith(`${archive}${sep}`)) continue
		const text = readFileSync(join(docsDir, file), 'utf8')
		for (const match of text.matchAll(LINK)) {
			const [label, target] = [match[1], match[2]]
			/* A link is named by its subject; a file name says nothing to a reader. */
			if (label.replace(/[`*_]/g, '').trim().endsWith('.md')) {
				const line = text.slice(0, match.index).split('\n').length
				failures.push({ file, line, target, reason: 'link text is a file name' })
				continue
			}
			if (!isInternal(target)) continue
			checked += 1
			const result = checkTarget(docsDir, file, target)
			if (result.ok) continue
			const line = text.slice(0, match.index).split('\n').length
			failures.push({ file, line, target, reason: result.reason })
		}
	}

	return { checked, failures }
}

function main() {
	const docsDir = process.argv[2] ?? new URL('../../docs', import.meta.url).pathname
	const { checked, failures } = checkLinks(docsDir)

	for (const failure of failures) {
		console.error(`${failure.file}:${failure.line}: ${failure.target}: ${failure.reason}`)
	}
	console.log(`${checked} relative links checked, ${failures.length} failed`)

	process.exit(failures.length === 0 ? 0 : 1)
}

if (process.argv[1]?.endsWith('checks.mjs')) main()
