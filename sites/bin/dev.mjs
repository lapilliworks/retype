/* Development server for the deployed tree: the product page at the root and
 * the manual under /docs/, one origin and one public port. The manual runs on an
 * internal port and the product server proxies to it. */

import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const site = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const manual = join(site, 'manual')
const product = join(site, 'product')
const vitepress = join(manual, 'node_modules/vitepress/bin/vitepress.js')
const publicPort = Number(process.env.SITE_APP_PORT ?? 5173)
const manualPort = Number(process.env.SITE_MANUAL_PORT ?? 5174)

if (!existsSync(vitepress)) {
	console.error(`missing ${vitepress}: install the manual's dependencies first`)
	process.exit(1)
}

const children = []
let stopping = false

function stop(code) {
	if (stopping) return
	stopping = true
	for (const child of children) {
		if (child.exitCode === null) child.kill('SIGTERM')
	}
	process.exit(code)
}

function start(command, args, options) {
	const child = spawn(command, args, { stdio: 'inherit', ...options })
	children.push(child)
	child.on('exit', (code, signal) => stop(code ?? (signal ? 1 : 0)))
}

process.on('SIGINT', () => stop(0))
process.on('SIGTERM', () => stop(0))

/* Start the product only once the manual answers, so the first visit to /docs/
 * does not land on the proxy's error page. */
async function waitForManual() {
	for (let attempt = 0; attempt < 100; attempt += 1) {
		try {
			const response = await fetch(`http://127.0.0.1:${manualPort}/docs/`)
			if (response.ok) return
		} catch {}
		await new Promise((resolve) => setTimeout(resolve, 100))
	}
	throw new Error(`the manual did not answer on port ${manualPort}`)
}

/* The manual is served under the same paths the bundle uses, so its links work
 * unchanged in development and on the edge. */
start(
	process.execPath,
	[vitepress, 'dev', '--host', '0.0.0.0', '--port', String(manualPort), '--strictPort'],
	{
		cwd: manual,
		env: {
			...process.env,
			SITE_BASE: '/docs/',
			SITE_CLEAN_URLS: 'false',
			SITE_PRODUCT_URL: process.env.SITE_PRODUCT_URL ?? `http://127.0.0.1:${publicPort}/`,
		},
	},
)

try {
	await waitForManual()
} catch (error) {
	console.error(error.message)
	stop(1)
}

start(process.execPath, [join(product, 'bin/serve.mjs')], {
	cwd: product,
	env: {
		...process.env,
		PORT: String(publicPort),
		HOST: '0.0.0.0',
		SITE_DOCS_URL: `http://127.0.0.1:${manualPort}/`,
	},
})
