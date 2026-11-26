// Loads a built web game in headless Chromium and checks it runs.
// usage: node tools/web_smoke.js <dir with index.html + <app>.js/.wasm> <app> [out.png]
// Needs Playwright (preinstalled in CI image). Passes only if the runtime
// initialises, no JS error/abort is reported, and the canvas has drawn
// something that is not a single flat colour.
const { chromium } = require('playwright');
const http = require('http'), fs = require('fs'), path = require('path');
const [dirArg, app, out] = process.argv.slice(2);
const dir = path.resolve(dirArg);
const types = { '.html': 'text/html', '.js': 'text/javascript', '.wasm': 'application/wasm', '.data': 'application/octet-stream' };
const server = http.createServer((req, res) => {
  const f = path.join(dir, decodeURIComponent(req.url.split('?')[0]).replace(/^\/$/, '/index.html'));
  if (!f.startsWith(dir) || !fs.existsSync(f)) { res.writeHead(404); return res.end(); }
  res.writeHead(200, { 'content-type': types[path.extname(f)] || 'application/octet-stream' });
  fs.createReadStream(f).pipe(res);
}).listen(0, async () => {
  const port = server.address().port;
  const browser = await chromium.launch({ args: ['--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist'] });
  const page = await browser.newPage({ viewport: { width: 960, height: 600 } });
  const logs = [];
  page.on('console', m => logs.push(m.text()));
  page.on('pageerror', e => logs.push('PAGEERROR ' + e.message));
  await page.goto(`http://127.0.0.1:${port}/index.html?app=${app}`);
  let ok = true, why = '';
  try { await page.waitForFunction('window.DUST && window.DUST.ready', null, { timeout: 60000 }); }
  catch { ok = false; why = 'runtime never initialised'; }
  await page.waitForTimeout(3000);
  if (out) await page.screenshot({ path: out });
  const png = await page.screenshot();
  // Crude "drew something" check: more than a handful of distinct byte values in the PNG body.
  const distinct = new Set(png.subarray(100, 4000)).size;
  const errs = await page.evaluate('window.DUST.errors');
  if (errs.length) { ok = false; why += ' js errors: ' + errs.join('; '); }
  if (distinct < 40) { ok = false; why += ' canvas looks blank'; }
  console.log(logs.slice(-15).join('\n'));
  console.log(ok ? 'WEB SMOKE OK' : 'WEB SMOKE FAIL:' + why);
  await browser.close(); server.close(); process.exit(ok ? 0 : 1);
});
