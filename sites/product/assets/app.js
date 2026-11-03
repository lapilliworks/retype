(() => {
	'use strict'

	const source = { stem: 'holiday', ext: 'heic' }

	const formatNames = {
		jpg: 'JPEG',
		png: 'PNG',
		webp: 'WebP',
		avif: 'AVIF',
		heic: 'HEIC',
		gif: 'GIF',
		tiff: 'TIFF',
		bmp: 'BMP',
		ico: 'ICO',
		mp4: 'MP4',
		mkv: 'Matroska',
		mov: 'QuickTime',
		webm: 'WebM',
		mp3: 'MP3',
		flac: 'FLAC',
		wav: 'WAV',
		opus: 'Opus',
		md: 'Markdown',
		html: 'HTML',
		docx: 'Word',
		odt: 'OpenDocument',
		epub: 'EPUB',
		rst: 'reStructuredText',
		tex: 'TeX',
		typst: 'Typst',
		pdf: 'PDF',
		zip: 'ZIP',
		tar: 'TAR',
		'7z': '7-Zip'
	}

	const aliases = {
		jpeg: 'jpg',
		tif: 'tiff',
		htm: 'html',
		markdown: 'md',
		'1080p': 'cap1080',
		'4k': 'cap2160',
		q: 'quality',
		w: 'width',
		h: 'height'
	}

	const losslessImages = new Set(['png', 'gif', 'tiff', 'bmp', 'ico'])
	const declaredNotBuilt = new Set([
		'lossless',
		'size',
		'effort',
		'compress',
		'colors',
		'crop',
		'gray',
		'nometa',
		'noprofile',
		'trim',
		'speed',
		'normalize',
		'mono',
		'bitrate',
		'codec',
		'page',
		'dpi'
	])

	const numberValue = (token) => {
		const match = token.match(/^([a-z]+)=?(\d+.*)$/)
		if (!match) return null
		return { name: match[1], value: match[2].replace('=', '') }
	}

	const readToken = (raw) => {
		const lower = raw.toLowerCase().replace('=', '')
		if (aliases[lower]) {
			return readToken(aliases[lower])
		}
		if (lower === 'ask' || lower === 'now') {
			return { name: lower, printed: lower, meta: true, say: lower === 'ask' ? 'asks before running' : 'runs without asking' }
		}
		if (lower === 'fliph' || lower === 'flipv') {
			return { name: lower, printed: lower, say: lower === 'fliph' ? 'mirrors left to right' : 'mirrors top to bottom' }
		}
		if (lower === 'mute') {
			return { name: lower, printed: lower, say: 'drops the audio' }
		}

		const baseName = (lower.match(/^[a-z]+/) || [lower])[0]
		if (declaredNotBuilt.has(baseName)) {
			return { error: `${baseName} is declared, but not built in this build` }
		}

		const parsed = numberValue(lower)
		if (!parsed) {
			if (['quality', 'cap', 'rotate', 'width', 'height', 'resize', 'at', 'fps'].includes(baseName)) {
				return { error: `${baseName} takes a value` }
			}
			return { error: `Unknown token: ${raw}` }
		}

		const { name, value } = parsed
		const canonical = aliases[name] || name
		if (declaredNotBuilt.has(canonical)) {
			return { error: `${canonical} is declared, but not built in this build` }
		}

		switch (canonical) {
			case 'quality':
				return { name: canonical, printed: `quality${value}`, say: `quality ${value}` }
			case 'cap':
				return { name: canonical, printed: `cap${value}`, say: `longest edge ${value}` }
			case 'rotate':
				return { name: canonical, printed: `rotate${value}`, say: `rotate ${value}°` }
			case 'width':
				return { name: canonical, printed: `width${value}`, say: `width ${value}` }
			case 'height':
				return { name: canonical, printed: `height${value}`, say: `height ${value}` }
			case 'resize':
				return { name: canonical, printed: `resize${value}`, say: `fit inside ${value}` }
			case 'at':
				return { name: canonical, printed: `at${value}`, say: `from ${value}` }
			case 'fps':
				return { name: canonical, printed: `fps${value}`, say: `${value} frames per second` }
			default:
				return { error: `Unknown token: ${raw}` }
		}
	}

	const parseGroup = (rawGroup) => {
		const group = rawGroup.trim()
		if (!group) return { error: 'A comma starts another output; it cannot end the name.' }

		const parts = group.split('+')
		const rawFormat = (parts[0] || '').trim().toLowerCase()
		if (!rawFormat) return { error: 'A group needs a format after the dot.' }

		const format = aliases[rawFormat] || rawFormat
		if (!formatNames[format]) return { error: `Unknown format: ${rawFormat}` }

		const tokens = []
		for (const raw of parts.slice(1)) {
			if (!raw.trim()) return { error: 'An empty token follows a plus sign.' }
			const token = readToken(raw.trim())
			if (token.error) return token
			if (losslessImages.has(format) && ['quality', 'lossless', 'size'].includes(token.name)) {
				return { error: `${format.toUpperCase()} has no ${token.name === 'quality' ? 'quality' : token.name} axis` }
			}
			if (losslessImages.has(format) && token.name === 'effort') {
				return { error: `${format.toUpperCase()} has no effort dial` }
			}
			tokens.push(token)
		}

		const conversionTokens = tokens.filter((token) => !token.meta)
		const sameFormat = format === source.ext
		const keepsSource = sameFormat && conversionTokens.length === 0

		const printedTokens = conversionTokens.map((token) => token.printed)
		const name = keepsSource
			? `${source.stem}.${source.ext}`
			: `${source.stem}${printedTokens.length ? '-' + printedTokens.join('-') : ''}.${format}`

		const wording = tokens
			.filter((token) => !token.meta)
			.map((token) => token.say)
			.join(', ')

		return {
			format,
			formatName: formatNames[format],
			tokens,
			name,
			keepsSource,
			note: keepsSource
				? 'Same format, no tokens. The source stays as it is.'
				: `${formatNames[format]}${wording ? `, ${wording}` : ' at the format default'}.`,
			meta: tokens.filter((token) => token.meta).map((token) => token.say)
		}
	}

	const parseIntent = (text) => {
		const value = text.trim()
		if (!value) return { error: 'Name nothing after the dot, and there is nothing to convert.' }
		if (/\s/.test(value)) return { error: 'Spaces are not part of the name.' }

		const groups = value.split(',')
		const outputs = []
		for (const group of groups) {
			const parsed = parseGroup(group)
			if (parsed.error) return parsed
			outputs.push(parsed)
		}
		return { outputs }
	}

	const preview = {
		input: document.querySelector('#intent-input'),
		targetName: document.querySelector('[data-target-name]'),
		targetType: document.querySelector('[data-target-type]'),
		note: document.querySelector('[data-demo-note]'),
		list: document.querySelector('[data-output-list]'),
		command: document.querySelector('[data-command]'),
		recipes: [...document.querySelectorAll('[data-intent]')]
	}

	const renderPreview = () => {
		if (!preview.input) return
		const value = preview.input.value
		const result = parseIntent(value)
		preview.note.classList.remove('is-error')
		preview.list.innerHTML = ''

		if (result.error) {
			preview.note.textContent = result.error
			preview.note.classList.add('is-error')
			preview.targetName.textContent = 'no output'
			preview.targetType.textContent = 'REFUSED'
			const item = document.createElement('li')
			item.className = 'is-note'
			item.textContent = 'No output name is derived.'
			preview.list.append(item)
		} else {
			const names = result.outputs.map((output) => output.name)
			const formats = [...new Set(result.outputs.map((output) => output.format.toUpperCase()))]
			preview.targetName.textContent = names.join(' + ')
			preview.targetType.textContent = formats.length > 1 ? `${formats.length} FILES` : formats[0]

			const notes = result.outputs.map((output) => output.note)
			const meta = [...new Set(result.outputs.flatMap((output) => output.meta))].map(
				(value) => value.charAt(0).toUpperCase() + value.slice(1) + '.'
			)
			preview.note.textContent = [...notes, ...meta].join(' ')

			for (const output of result.outputs) {
				const item = document.createElement('li')
				const code = document.createElement('code')
				code.textContent = output.name
				item.append(code)
				preview.list.append(item)
			}
		}

		if (preview.command) {
			preview.command.textContent = `retype c ${source.stem}.${source.ext} ${value || '<intent>'}`
		}

		for (const button of preview.recipes) {
			const active = button.dataset.intent === value
			button.classList.toggle('is-active', active)
			button.setAttribute('aria-pressed', String(active))
		}
	}

	if (preview.input) {
		preview.input.addEventListener('input', renderPreview)
		preview.input.addEventListener('keydown', (event) => {
			if (event.key === 'Enter') {
				event.preventDefault()
				preview.input.blur()
			}
		})

		for (const button of preview.recipes) {
			button.addEventListener('click', () => {
				preview.input.value = button.dataset.intent
				renderPreview()
				preview.input.focus()
			})
		}

		renderPreview()
	}

	const copyText = async (text, button) => {
		if (!text) return
		try {
			await navigator.clipboard.writeText(text)
		} catch {
			const area = document.createElement('textarea')
			area.value = text
			area.setAttribute('readonly', '')
			area.style.position = 'fixed'
			area.style.opacity = '0'
			document.body.append(area)
			area.select()
			document.execCommand('copy')
			area.remove()
		}
		if (!button) return
		const previous = button.textContent
		button.textContent = 'Copied'
		window.setTimeout(() => {
			button.textContent = previous
		}, 1400)
	}

	document.addEventListener('click', (event) => {
		const copy = event.target.closest('[data-copy-command]')
		if (copy) {
			const command = document.querySelector('[data-command]')
			copyText(command ? command.textContent : '', copy)
			return
		}

		const commandCopy = event.target.closest('[data-copy]')
		if (commandCopy) {
			const command = commandCopy.closest('.command')
			const code = command ? command.querySelector('code') : null
			copyText(code ? code.textContent : '', commandCopy)
		}
	})

	// Tabs
	const tabs = [...document.querySelectorAll('[data-tab]')]
	const panels = [...document.querySelectorAll('.tab-panel')]

	const selectTab = (tab) => {
		for (const item of tabs) {
			const selected = item === tab
			item.setAttribute('aria-selected', String(selected))
			item.tabIndex = selected ? 0 : -1
		}
		for (const panel of panels) {
			panel.hidden = panel.id !== tab.getAttribute('aria-controls')
		}
	}

	if (tabs.length) {
		for (const tab of tabs) {
			tab.addEventListener('click', () => selectTab(tab))
			tab.addEventListener('keydown', (event) => {
				const index = tabs.indexOf(tab)
				if (event.key === 'ArrowRight' || event.key === 'ArrowDown') {
					event.preventDefault()
					const next = tabs[(index + 1) % tabs.length]
					next.focus()
					selectTab(next)
				} else if (event.key === 'ArrowLeft' || event.key === 'ArrowUp') {
					event.preventDefault()
					const next = tabs[(index - 1 + tabs.length) % tabs.length]
					next.focus()
					selectTab(next)
				} else if (event.key === 'Home') {
					event.preventDefault()
					tabs[0].focus()
					selectTab(tabs[0])
				} else if (event.key === 'End') {
					event.preventDefault()
					tabs[tabs.length - 1].focus()
					selectTab(tabs[tabs.length - 1])
				}
			})
		}
	}

	// Mobile navigation
	const toggle = document.querySelector('[data-nav-toggle]')
	const nav = document.querySelector('#primary-nav')
	const navLabel = document.querySelector('[data-nav-label]')

	const setNav = (open) => {
		if (!toggle || !nav) return
		toggle.setAttribute('aria-expanded', String(open))
		nav.classList.toggle('is-open', open)
		if (navLabel) navLabel.textContent = open ? 'Close' : 'Menu'
	}

	if (toggle && nav) {
		toggle.addEventListener('click', () => setNav(toggle.getAttribute('aria-expanded') !== 'true'))
		for (const link of nav.querySelectorAll('a')) {
			link.addEventListener('click', () => setNav(false))
		}
		document.addEventListener('keydown', (event) => {
			if (event.key === 'Escape') setNav(false)
		})
		document.addEventListener('click', (event) => {
			if (!nav.classList.contains('is-open')) return
			if (nav.contains(event.target) || toggle.contains(event.target)) return
			setNav(false)
		})
	}

	// Header shadow
	const header = document.querySelector('[data-header]')
	const setHeader = () => {
		if (header) header.classList.toggle('is-scrolled', window.scrollY > 8)
	}
	setHeader()
	window.addEventListener('scroll', setHeader, { passive: true })

	// Reveal on scroll
	const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches
	const items = [...document.querySelectorAll('[data-reveal]')]
	if (reduced || !('IntersectionObserver' in window)) {
		for (const item of items) item.classList.add('is-visible')
	} else {
		const observer = new IntersectionObserver(
			(entries) => {
				for (const entry of entries) {
					if (!entry.isIntersecting) continue
					entry.target.classList.add('is-visible')
					observer.unobserve(entry.target)
				}
			},
			{ rootMargin: '0px 0px -8% 0px', threshold: 0.08 }
		)
		for (const item of items) observer.observe(item)
	}
})()
