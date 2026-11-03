/* Copy the product site into the output directory. No bundler, no dependencies:
 * the page is static and every request it makes is to a file in this directory. */

import { cpSync, mkdirSync, rmSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const app = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const root = resolve(app, '../..')
const out = process.env.SITE_OUT_DIR ? resolve(process.env.SITE_OUT_DIR) : resolve(app, 'dist')

const files = ['index.html', '404.html', 'robots.txt']

rmSync(out, { recursive: true, force: true })
mkdirSync(out, { recursive: true })

for (const name of files) cpSync(join(app, name), join(out, name))
cpSync(join(app, 'assets'), join(out, 'assets'), { recursive: true })
cpSync(join(app, 'public'), join(out, 'public'), { recursive: true })
/* The installers are release assets and page assets: one file, two hosts. */
for (const name of ['install.sh', 'install.ps1']) cpSync(join(root, 'packaging', name), join(out, name))

console.log(`built ${out}`)
