/* Development server for the deployed tree. It serves the product page at the
 * root and proxies /docs/ to the manual's dev server, so both apps share one
 * origin and one port. The production bundle serves the manual from sites/dist/docs/. */

import { createReadStream, existsSync } from 'node:fs'
import { createServer, request as httpRequest } from 'node:http'
import { request as httpsRequest } from 'node:https'
import { connect as netConnect } from 'node:net'
import { connect as tlsConnect } from 'node:tls'
import { dirname, extname, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

const app = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const port = Number(process.env.PORT ?? process.env.SITE_APP_PORT ?? process.env.SITE_PORT ?? 5173)
const host = process.env.HOST ?? '0.0.0.0'
const docs = new URL(process.env.SITE_DOCS_URL ?? 'http://127.0.0.1:5174/')
const docsPort = Number(docs.port || (docs.protocol === 'https:' ? 443 : 80))
const docsRequest = docs.protocol === 'https:' ? httpsRequest : httpRequest

const types = {
	'.css': 'text/css; charset=utf-8',
	'.html': 'text/html; charset=utf-8',
	'.js': 'text/javascript; charset=utf-8',
	'.json': 'application/json; charset=utf-8',
	'.svg': 'image/svg+xml',
	'.sh': 'text/plain; charset=utf-8',
	'.ps1': 'text/plain; charset=utf-8',
	'.txt': 'text/plain; charset=utf-8',
	'.webp': 'image/webp',
	'.png': 'image/png',
	'.jpg': 'image/jpeg',
	'.woff2': 'font/woff2',
}

function fileFor(pathname) {
	const relative = pathname === '/' ? '/index.html' : pathname.endsWith('/') ? `${pathname}index.html` : pathname
	const file = resolve(app, `.${relative}`)
	if (file !== app && !file.startsWith(`${app}${sep}`)) return null
	return existsSync(file) ? file : null
}

function send(response, status, type, body) {
	response.writeHead(status, {
		'Content-Type': type,
		'Cache-Control': 'no-store',
		'X-Content-Type-Options': 'nosniff',
		'Referrer-Policy': 'strict-origin-when-cross-origin',
	})
	if (typeof body === 'string' || Buffer.isBuffer(body)) {
		response.end(body)
		return
	}
	body.pipe(response)
}

function isDocsPath(pathname) {
	return pathname === '/docs' || pathname.startsWith('/docs/')
}

/* Keep the public path intact: the manual runs with SITE_BASE=/docs/, so it
 * expects the prefix the bundle serves it under. */
function proxyToDocs(request, response) {
	const upstream = docsRequest(
		{
			hostname: docs.hostname,
			port: docsPort,
			method: request.method,
			path: request.url,
			headers: request.headers,
		},
		(upstreamResponse) => {
			response.writeHead(upstreamResponse.statusCode ?? 502, upstreamResponse.headers)
			upstreamResponse.pipe(response)
		},
	)
	upstream.on('error', (error) => {
		if (response.headersSent) response.destroy(error)
		else send(response, 502, types['.txt'], `manual unavailable: ${error.message}\n`)
	})
	request.pipe(upstream)
}

/* The live reload socket takes the same path as the page it belongs to. */
function proxyUpgrade(request, socket, head) {
	if (!isDocsPath(request.url ?? '/')) {
		socket.destroy()
		return
	}
	const connect = docs.protocol === 'https:' ? tlsConnect : netConnect
	const upstream = connect({ host: docs.hostname, port: docsPort, servername: docs.hostname }, () => {
		const lines = [`${request.method} ${request.url} HTTP/1.1`]
		for (let index = 0; index < request.rawHeaders.length; index += 2) {
			lines.push(`${request.rawHeaders[index]}: ${request.rawHeaders[index + 1]}`)
		}
		upstream.write(`${lines.join('\r\n')}\r\n\r\n`)
		if (head.length > 0) upstream.write(head)
		upstream.pipe(socket)
		socket.pipe(upstream)
	})
	upstream.on('error', () => socket.destroy())
	socket.on('error', () => upstream.destroy())
}

const server = createServer((request, response) => {
	const url = new URL(request.url ?? '/', `http://${request.headers.host ?? 'localhost'}`)
	let pathname
	try {
		pathname = decodeURIComponent(url.pathname)
	} catch {
		send(response, 400, types['.txt'], 'bad request\n')
		return
	}

	/* The manual runs as a second process. The bundle serves it as static files. */
	if (pathname === '/docs') {
		response.writeHead(308, { Location: `/docs/${url.search}` })
		response.end()
		return
	}
	if (isDocsPath(pathname)) {
		proxyToDocs(request, response)
		return
	}

	const file = fileFor(pathname)
	if (file) {
		send(response, 200, types[extname(file)] ?? 'application/octet-stream', createReadStream(file))
		return
	}

	const notFound = resolve(app, '404.html')
	if (existsSync(notFound)) {
		send(response, 404, types['.html'], createReadStream(notFound))
		return
	}

	send(response, 404, types['.txt'], 'not found\n')
})

server.listen(port, host, () => {
	console.log(`product site on http://${host === '0.0.0.0' ? '127.0.0.1' : host}:${port}/`)
	console.log(`manual proxied from ${docs.href}`)
})
server.on('upgrade', proxyUpgrade)
