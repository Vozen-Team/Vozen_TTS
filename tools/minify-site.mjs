// tools/minify-site.mjs
//
// Builds site-dist/ from site/, minifying what is served in production:
//   - index.html -> compact HTML (with minified inline CSS/JS)
//   - *.css / *.js -> minified for smaller and less casually readable output
//   - privacy.html / terms.html -> copied as-is so legal pages stay readable
//   - assets/, favicon, etc. -> copied
//
// Important: minification does not hide a page. Developer tools still expose the
// rendered DOM and downloaded resources. It only reduces payload size and deters
// casual copy/paste; the readable source remains in site/.
//
// Pure Node ESM, invoked by `npm run build:site`; not part of the TypeScript build.

import { readdir, mkdir, readFile, writeFile, copyFile, rm } from 'node:fs/promises';
import { join, dirname, extname, basename, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import { minify as minifyHtml } from 'html-minifier-terser';
import { minify as minifyJs } from 'terser';
import CleanCSS from 'clean-css';
import { loadPublicSiteConfig, configurePublicSiteSource } from './site-config.mjs';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const SRC = join(ROOT, 'site');
const OUT = join(ROOT, 'site-dist');

// Only the landing page is minified. Legal pages remain readable.
const MINIFY_HTML = new Set(['index.html']);

const HTML_OPTS = {
  collapseWhitespace: true,
  removeComments: true,
  minifyCSS: true, // <style> inline
  minifyJS: true, // <script> inline
  removeRedundantAttributes: true,
  removeScriptTypeAttributes: true,
  useShortDoctype: true,
};

// Mojibake guard for UTF-8 accidentally decoded or saved as Windows-1252 (a common
// PowerShell 5.1 Get-Content|Set-Content failure mode). These sequences should not
// occur in correctly decoded UTF-8; a lone `â` is intentionally not rejected.
const MOJIBAKE = /â‚¬|ðŸ|â€™|â€œ|â€|â€“|â€”|Ã©|Ã¡|Ã£|Ã§|Ãµ|Ã­|Ã³|Ãº|Ã \b|Â·|Â«|Â»|âˆ'/;
const TEXT_EXT = new Set(['.html', '.css', '.js', '.json', '.svg', '.txt', '.webmanifest']);

/** Fails the build when a text file contains a known mojibake sequence. */
function assertNoMojibake(rel, text) {
  const m = text.match(MOJIBAKE);
  if (m) {
    const line = text.slice(0, m.index).split('\n').length;
    throw new Error(
      `mojibake detected in ${rel}:${line} (sequence "${m[0]}") — ` +
        `corrupt UTF-8 file (possibly decoded as Windows-1252); restore it from git.`,
    );
  }
}

/** Recursively lists every file below `dir` as an absolute path. */
async function walk(dir) {
  const out = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const p = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...(await walk(p)));
    else out.push(p);
  }
  return out;
}

async function run() {
  const config = loadPublicSiteConfig(process.env);
  await rm(OUT, { recursive: true, force: true });
  const files = await walk(SRC);
  let minified = 0;
  let copied = 0;
  for (const file of files) {
    const rel = relative(SRC, file);
    const outPath = join(OUT, rel);
    await mkdir(dirname(outPath), { recursive: true });
    const ext = extname(file).toLowerCase();

    // Read text files once and validate their encoding before processing.
    if (TEXT_EXT.has(ext)) {
      const text = configurePublicSiteSource(await readFile(file, 'utf8'), config);
      assertNoMojibake(rel, text);

      if (ext === '.html' && MINIFY_HTML.has(basename(file))) {
        await writeFile(outPath, await minifyHtml(text, HTML_OPTS));
        minified++;
      } else if (ext === '.js') {
        const res = await minifyJs(text, { compress: true, mangle: true });
        await writeFile(outPath, res.code ?? text);
        minified++;
      } else if (ext === '.css') {
        const res = new CleanCSS({ returnPromise: false }).minify(text);
        if (res.errors.length)
          throw new Error(`clean-css failed for ${rel}: ${res.errors.join('; ')}`);
        await writeFile(outPath, res.styles);
        minified++;
      } else {
        await writeFile(outPath, text); // legal pages (privacy/terms), JSON, SVG
        copied++;
      }
    } else {
      await copyFile(file, outPath); // binary assets, favicon, images, MP3
      copied++;
    }
  }
  console.log(`[minify-site] ${minified} file(s) minified, ${copied} copied -> site-dist/`);
}

run().catch((err) => {
  console.error('[minify-site] failed:', err);
  process.exit(1);
});
