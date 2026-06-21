/**
 * Headless reproduce-then-verify for the math-copy prose-preservation bug.
 *
 * BEFORE fix: selecting a mixed prose+math paragraph copies only TeX formulas
 * (prose dropped). AFTER fix: clipboard contains prose text with TeX inline.
 *
 * Run against the UNFIXED binary to confirm reproduction of the bug.
 * Run again after fix to confirm the correct output.
 *
 * Usage:
 *   node headless-math-copy-prose.mjs
 */

import { chromium } from '/home/yutaro/prog/hibiki-automatic/mycelium-editor/node_modules/playwright/index.mjs';
import { spawn } from 'child_process';
import { mkdtempSync, writeFileSync, readFileSync } from 'fs';
import { tmpdir, homedir } from 'os';
import { join } from 'path';

const DAEMON = '/home/yutaro/prog/hibiki-automatic/md-preview/target/release/md-preview';

// Markdown with prose + multiple inline formulas + a display formula
const MD = `# Math Copy Prose Test

Set parameters $a = 1229$, $c = 351750$, and $m = 1664501$.

$$
X_1 = \\sqrt{-2 \\log U_1}
$$
`;

const tmpDir = mkdtempSync(join(tmpdir(), 'md-preview-math-prose-'));
const testFile = join(tmpDir, 'math-prose.md');
writeFileSync(testFile, MD);

let browser;
const results = [];

function pass(msg) { results.push({ ok: true, msg }); console.log('  ✓', msg); }
function fail(msg) { results.push({ ok: false, msg }); console.error('  ✗', msg); }
function sleep(ms) { return new Promise(r => setTimeout(r, ms)); }

/**
 * Read the nonce from the bootstrap.html written by the daemon's thin client.
 * The bootstrap HTML contains: name="nonce" value="<NONCE>"
 */
function extractNonceFromBootstrap(bootstrapPath) {
  try {
    const html = readFileSync(bootstrapPath, 'utf8');
    const m = html.match(/name="nonce" value="([^"]+)"/);
    return m ? m[1] : null;
  } catch {
    return null;
  }
}

/**
 * Determine the md-preview control dir (mirrors the Rust socket_path logic).
 * Uses $XDG_RUNTIME_DIR/md-preview/ if set, else ~/.local/share/md-preview/.
 */
function controlDir() {
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg) return join(xdg, 'md-preview');
  return join(homedir(), '.local', 'share', 'md-preview');
}

/**
 * Launch md-preview <file> with BROWSER=/bin/true (skip actual browser open)
 * and wait for the URL line. Returns { port, nonce } extracted from output +
 * bootstrap.html.
 */
async function startDaemon() {
  return new Promise((resolve, reject) => {
    // Use BROWSER=/bin/true so the thin client writes bootstrap.html but does
    // not actually launch a browser; it also prints "Preview ready: <url>".
    const proc = spawn(DAEMON, [testFile], {
      stdio: ['ignore', 'pipe', 'pipe'],
      detached: false,
      env: { ...process.env, BROWSER: '/bin/true' },
    });

    let resolved = false;
    const to = setTimeout(() => {
      if (!resolved) reject(new Error('Daemon start timeout (12s)'));
    }, 12000);

    const onLine = (data) => {
      const line = data.toString();
      process.stdout.write('[daemon] ' + line);
      // "Preview ready: http://127.0.0.1:7878/view?path=..."
      const m = line.match(/Preview ready[^:]*:\s*(http:\/\/127\.0\.0\.1:(\d+))/);
      if (m && !resolved) {
        resolved = true;
        clearTimeout(to);
        proc.kill(); // thin client exits promptly; daemon continues in background
        // Small delay so bootstrap.html is flushed before we read it
        setTimeout(() => {
          const bootstrapPath = join(controlDir(), 'bootstrap.html');
          const nonce = extractNonceFromBootstrap(bootstrapPath);
          resolve({ baseUrl: m[1], port: parseInt(m[2]), nonce });
        }, 200);
      }
    };
    proc.stdout.on('data', onLine);
    proc.stderr.on('data', onLine);
    proc.on('error', reject);
  });
}

/**
 * POST to /claim with the nonce, follow the redirect, and return the session
 * cookie value.
 */
async function claimSession(context, baseUrl, nonce, filePath) {
  const page = await context.newPage();
  // POST to /claim (follow redirect automatically)
  await page.goto(`${baseUrl}/claim`, { waitUntil: 'domcontentloaded' });
  // Actually simulate the form POST by navigating to a data URL that posts the form
  // Easier: use fetch from a page context with credentials
  const sessionCookie = await page.evaluate(async ({ claimUrl, nonce, next }) => {
    const body = new URLSearchParams({ nonce, next });
    const resp = await fetch(claimUrl, {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: body.toString(),
      redirect: 'manual',
    });
    // 302 redirect means claim succeeded; cookie is now in the browser context
    return resp.status;
  }, {
    claimUrl: `${baseUrl}/claim`,
    nonce,
    next: `/view?path=${encodeURIComponent(filePath)}`,
  });
  await page.close();
  return sessionCookie;
}

async function waitForKaTeX(frame, timeoutMs = 15000) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const count = await frame.evaluate(() =>
      document.querySelectorAll('.katex').length
    ).catch(() => 0);
    if (count > 0) return count;
    await sleep(300);
  }
  return 0;
}

async function waitForSrcdocFrame(page, timeoutMs = 10000) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const frames = page.frames();
    const f = frames.find(f =>
      f.url() === 'about:srcdoc' ||
      f.url() === '' ||
      f.name() === 'render-frame'
    );
    if (f) return f;
    await sleep(300);
  }
  return null;
}

/**
 * Simulate copy on the first <p> element in the srcdoc frame using a
 * synthetic ClipboardEvent with a DataTransfer we can inspect. Returns the
 * text/plain string the handler wrote, or null if it did not intercept.
 */
async function simulateCopyOnParagraph(frame) {
  return frame.evaluate(() => {
    const p = document.querySelector('p');
    if (!p) return { error: 'no paragraph found', capturedText: null, paragraphText: null, katexCount: 0 };

    // Select the whole paragraph
    const range = document.createRange();
    range.selectNode(p);
    const sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);

    // Create a DataTransfer and monkey-patch setData to capture what the handler sets
    let capturedText = null;
    const dt = new DataTransfer();
    const origSetData = dt.setData.bind(dt);
    dt.setData = (type, val) => {
      if (type === 'text/plain') capturedText = val;
      origSetData(type, val);
    };

    const ev = new ClipboardEvent('copy', {
      bubbles: true,
      cancelable: true,
      clipboardData: dt,
    });
    p.dispatchEvent(ev);

    return {
      capturedText,
      paragraphText: p.textContent,
      katexCount: document.querySelectorAll('.katex').length,
    };
  });
}

async function killDaemon() {
  return new Promise(resolve => {
    const proc = spawn('pkill', ['-f', `md-preview.*--daemon`], { stdio: 'ignore' });
    proc.on('exit', resolve);
    proc.on('error', resolve); // pkill missing is fine
  });
}

async function main() {
  console.log('Starting md-preview (thin client launch)...');
  let startInfo;
  try {
    startInfo = await startDaemon();
  } catch (e) {
    console.error('Failed to start daemon:', e.message);
    process.exit(1);
  }

  const { baseUrl, nonce } = startInfo;
  console.log(`Base URL: ${baseUrl}`);
  console.log(`Nonce obtained: ${nonce ? 'yes (' + nonce.slice(0,8) + '...)' : 'NO — auth will fail'}`);

  if (!nonce) {
    console.error('Could not extract nonce from bootstrap.html');
    process.exit(1);
  }

  browser = await chromium.launch({ headless: true });
  const context = await browser.newContext();

  context.on('page', p => {
    p.on('console', msg => {
      if (msg.type() === 'error') console.error('[browser]', msg.text());
    });
  });

  // ── Auth: claim a session ────────────────────────────────────────────────
  console.log('\nClaiming session...');
  const claimStatus = await claimSession(context, baseUrl, nonce, testFile);
  console.log(`  /claim returned status: ${claimStatus} (302 = success)`);
  // After the fetch with redirect:manual the cookie is set in context.
  // Check cookies:
  const cookies = await context.cookies(`${baseUrl}`);
  const sessionCookie = cookies.find(c => c.name === 'session' || c.name.includes('sess'));
  console.log(`  Cookies set: ${cookies.map(c => c.name).join(', ') || 'none'}`);

  if (claimStatus !== 302 && claimStatus !== 200) {
    console.warn(`  Warning: /claim returned ${claimStatus} (expected 302)`);
  }

  // ── Navigate to /view ────────────────────────────────────────────────────
  const viewUrl = `${baseUrl}/view?path=${encodeURIComponent(testFile)}`;
  console.log(`\nNavigating to ${viewUrl}`);
  const page = await context.newPage();
  page.on('console', msg => {
    if (msg.type() === 'error') console.error('[view-page]', msg.text());
  });

  await page.goto(viewUrl, { waitUntil: 'domcontentloaded' });

  // Dump status
  const pageTitle = await page.title().catch(() => '?');
  console.log(`  Page title: ${pageTitle}`);
  console.log(`  URL after navigation: ${page.url()}`);

  // ── Wait for srcdoc iframe ───────────────────────────────────────────────
  const srcdocFrame = await waitForSrcdocFrame(page, 15000);

  if (!srcdocFrame) {
    const frames = page.frames();
    fail(`srcdoc iframe NOT found. Frames: ${frames.map(f => f.url()).join(', ')}`);
    // Check if page shows 401
    const bodyText = await page.evaluate(() => document.body?.textContent).catch(() => '');
    if (bodyText.includes('unauthorized') || bodyText.includes('401')) {
      console.error('  → page shows 401 — auth claim did not work');
    }
    await browser.close();
    await killDaemon();
    process.exit(1);
  }
  pass('srcdoc iframe found');

  // ── Wait for KaTeX ───────────────────────────────────────────────────────
  const katexCount = await waitForKaTeX(srcdocFrame, 15000);
  if (katexCount === 0) {
    fail('KaTeX did not render within 15s — cannot test math-copy');
    await browser.close();
    await killDaemon();
    process.exit(1);
  }
  pass(`KaTeX rendered (${katexCount} .katex elements)`);

  // ── Simulate copy ────────────────────────────────────────────────────────
  const result = await simulateCopyOnParagraph(srcdocFrame);
  console.log('\n--- Copy simulation result ---');
  console.log('  paragraphText:', JSON.stringify(result.paragraphText));
  console.log('  capturedText: ', JSON.stringify(result.capturedText));
  console.log('  katexCount:   ', result.katexCount);

  if (result.error) {
    fail(`Frame eval error: ${result.error}`);
  } else if (result.capturedText === null) {
    fail('Copy handler did not intercept — handler not wired or no math in paragraph?');
  } else {
    // "Set parameters $a = 1229$, $c = 351750$, and $m = 1664501$."
    // Prose words that must survive in the fixed output.
    // "1229" alone is not enough — it appears inside TeX $a = 1229$ even in the buggy output.
    const hasProse = result.capturedText.includes('Set parameters') ||
                     result.capturedText.includes(', and ');
    const hasTex = result.capturedText.includes('$');

    if (hasProse && hasTex) {
      pass(`CORRECT (prose+TeX): ${JSON.stringify(result.capturedText)}`);
    } else if (!hasProse && hasTex) {
      // Known pre-fix bug: only formulas, prose dropped
      console.log(`\n  BUG REPRODUCED: prose dropped, only TeX formulas: ${JSON.stringify(result.capturedText)}`);
      fail('PROSE DROPPED — parts.join(" ") bug confirmed');
    } else if (hasProse && !hasTex) {
      fail(`Prose present but no TeX substitution: ${JSON.stringify(result.capturedText)}`);
    } else {
      fail(`Unexpected clipboard: ${JSON.stringify(result.capturedText)}`);
    }
  }

  // ── Summary ──────────────────────────────────────────────────────────────
  console.log('\n=== Summary ===');
  const passed = results.filter(r => r.ok).length;
  const failedCount = results.filter(r => !r.ok).length;
  console.log(`${passed} passed, ${failedCount} failed`);
  results.forEach(r => {
    if (!r.ok) console.log(`  ✗ ${r.msg}`);
  });

  await browser.close();
  await killDaemon();

  // Exit 0 = all correct (fixed binary); exit 1 = bug present or other failure
  process.exit(failedCount > 0 ? 1 : 0);
}

main().catch(e => {
  console.error('Fatal:', e);
  if (browser) browser.close().catch(() => {});
  process.exit(1);
});
