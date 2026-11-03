import { createReadStream, existsSync, statSync } from 'node:fs'
import { readFile } from 'node:fs/promises'
import { createServer } from 'node:http'
import { extname, join, normalize, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const port = Number(process.env.SITE_PORT || 4173)
const host = process.env.SITE_HOST || '127.0.0.1'

const types = {
	'.css': 'text/css; charset=utf-8',
	'.html': 'text/html; charset=utf-8',
	'.js': 'text/javascript; charset=utf-8',
	'.json': 'application/json; charset=utf-8',
	'.svg': 'image/svg+xml',
	'.txt': 'text/plain; charset=utf-8'
}

const resolvePath = (pathname) => {
	const clean = normalize(decodeURIComponent(pathname)).replace(/^(\.\.[/\\])+/, '')
	const candidate = join(root, clean)
	if (!candidate.startsWith(root)) return null
	if (!existsSync(candidate)) return null
	const stats = statSync(candidate)
	return stats.isDirectory() ? join(candidate, 'index.html') : candidate
}

const server = createServer(async (request, response) => {
	const url = new URL(request.url || '/', `http://${request.headers.host || 'localhost'}`)
	let file = resolvePath(url.pathname)

	if (!file) {
		response.writeHead(404, { 'content-type': types['.html'] })
		response.end(await readFile(join(root, '404.html')))
		return
	}

	if (!existsSync(file)) {
		response.writeHead(404, { 'content-type': types['.html'] })
		response.end(await readFile(join(root, '404.html')))
		return
	}

	response.writeHead(200, {
		'content-type': types[extname(file)] || 'application/octet-stream',
		'cache-control': 'no-cache'
	})
	createReadStream(file).pipe(response)
})

server.listen(port, host, () => {
	console.log(`product site: http://${host}:${port}/`)
})
