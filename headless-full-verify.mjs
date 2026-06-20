/**
 * Headless verification: KaTeX + Mermaid render + math-copy in both /view and /edit.
 *
 * Uses a world-readable (0o644) test file so no auth claim dance is needed for /view.
 * Run after `cargo build --release`.
 */

import { chromium } from '/home/yutaro/prog/hibiki-automatic/mycelium-editor/node_modules/playwright/index.mjs';
import { spawn } from 'child_process';
import { mkdtempSync, writeFileSync, chmodSync } from 'fs';
import { tmpdir } from 'os';
import { join } from 'path';

const DAEMON = '/home/yutaro/prog/hibiki-automatic/md-preview/target/release/md-preview';

const TEST_MD = `# Test

Inline math: $E = mc^2$

Display math:

$$\\int_0^\\infty e^{-x} dx = 1$$

\`\`\`mermaid
graph TD
  A[Start] --> B[End]
\`\`\`
`;

const tmpDir = mkdtempSync(join(tmpdir(), 'md-preview-fv-'));
const testFile = join(tmpDir, 'test.md');
writeFileSync(testFile, TEST_MD);
chmodSync(testFile, 0o644); // world-readable — /content works without auth

let daemon;
let browser;
let port;
const results = [];

function pass(msg) { results.push({ ok: true, msg }); console.log('  ✓', msg); }
function fail(msg) { results.push({ ok: false, msg }); console.error('  ✗', msg); }
async function sleep(ms) { return new Promise(r => setTimeout(r, ms)); }

async function startDaemon(args) {
  return new Promise((resolve, reject) => {
    const proc = spawn(DAEMON, args, { stdio: ['ignore', 'pipe', 'pipe'], detached: false });
    const to = setTimeout(() => reject(new Error('daemon timeout')), 8000);
    let started = false;
    function tryPort(line) {
      const m = line.match(/:(\d+)/);
      if (m && !started) { started = true; clearTimeout(to); resolve({ proc, port: parseInt(m[1]) }); }
    }
    proc.stdout.on('data', d => { process.stdout.write('[daemon] ' + d); tryPort(d.toString()); });
    proc.stderr.on('data', d => { process.stderr.write('[daemon-err] ' + d); tryPort(d.toString()); });
    proc.on('error', reject);
  });
}

async function main() {
  console.log('Starting md-preview daemon (read-only mode)...');
  let d1;
  try {
    d1 = await startDaemon([testFile]);
    daemon = d1.proc;
    port = d1.port;
    console.log(`Daemon on port ${port}`);
  } catch (e) {
    console.error('Failed to start daemon:', e.message);
    process.exit(1);
  }
  await sleep(600);

  const baseUrl = `http://127.0.0.1:${port}`;
  browser = await chromium.launch({ headless: true });
  const ctx = await browser.newContext();

  // ── Step 1: Health ──────────────────────────────────────────────────────
  console.log('\n--- Step 1: Health check ---');
  {
    const p = await ctx.newPage();
    const r = await p.goto(`${baseUrl}/healthz`, { waitUntil: 'networkidle' });
    if (r?.status() === 200) pass('Daemon healthy');
    else fail('/healthz not 200');
    await p.close();
  }

  // ── Step 2: /view srcdoc renders KaTeX + Mermaid ────────────────────────
  console.log('\n--- Step 2: /view srcdoc — KaTeX and Mermaid render ---');
  const viewPage = await ctx.newPage();
  viewPage.on('console', m => { if (m.type() === 'error') console.error('[view-err]', m.text()); });
  await viewPage.goto(`${baseUrl}/view?path=${encodeURIComponent(testFile)}`, { waitUntil: 'load' });

  // Poll up to 30s for KaTeX + Mermaid in the srcdoc iframe.
  let katexRendered = false;
  let mermaidRendered = false;
  for (let i = 0; i < 60; i++) {
    await sleep(500);
    for (const frame of viewPage.frames()) {
      try {
        const kCount = await frame.evaluate(() => document.querySelectorAll('.katex').length).catch(() => 0);
        if (kCount > 0) katexRendered = true;
        const mCount = await frame.evaluate(() => document.querySelectorAll('svg').length).catch(() => 0);
        if (mCount > 0) mermaidRendered = true;
      } catch (_) {}
    }
    if (katexRendered && mermaidRendered) break;
  }
  if (katexRendered) pass('/view srcdoc: KaTeX rendered (.katex spans present)');
  else fail('/view srcdoc: KaTeX did NOT render');
  if (mermaidRendered) pass('/view srcdoc: Mermaid rendered (SVG present)');
  else fail('/view srcdoc: Mermaid did NOT render');

  // ── Step 3: Math-copy in /view srcdoc ────────────────────────────────────
  // Verify the enableMathCopyAsTex handler is wired by injecting a synthetic
  // copy event that carries a mock clipboardData object, then checking the
  // handler called setData with a TeX string.
  console.log('\n--- Step 3: Math-copy-as-TeX in /view srcdoc ---');
  if (katexRendered) {
    let mathCopyTested = false;
    for (const frame of viewPage.frames()) {
      try {
        const result = await frame.evaluate(() => {
          // Find the #doc element (the container the math-copy handler is registered on).
          const docEl = document.getElementById('doc');
          if (!docEl) return { skip: 'no #doc' };
          const katexEl = docEl.querySelector('.katex');
          if (!katexEl) return { skip: 'no .katex' };

          // Find a text node INSIDE the katex element so the handler's
          // findKatexEl() can walk up to the .katex ancestor.
          function findTextNode(el) {
            if (el.nodeType === Node.TEXT_NODE && el.textContent.trim()) return el;
            for (const child of el.childNodes) {
              const found = findTextNode(child);
              if (found) return found;
            }
            return null;
          }
          const textNode = findTextNode(katexEl);
          if (!textNode) return { skip: 'no text node inside .katex' };

          // Build a selection with startContainer = the text node inside .katex.
          const sel = window.getSelection();
          const range = document.createRange();
          range.setStart(textNode, 0);
          range.setEnd(textNode, textNode.textContent.length);
          sel.removeAllRanges();
          sel.addRange(range);

          // Fire a synthetic copy event with a mock clipboardData.
          let captured = null;
          const mockClipboard = {
            setData: (type, data) => { if (type === 'text/plain') captured = data; },
          };
          const ev = new ClipboardEvent('copy', { bubbles: true, cancelable: true });
          Object.defineProperty(ev, 'clipboardData', { value: mockClipboard, writable: false });
          docEl.dispatchEvent(ev);
          sel.removeAllRanges();

          return { skip: null, captured };
        }).catch(() => null);

        if (!result) continue;
        if (result.skip) { continue; }
        mathCopyTested = true;

        if (result.captured !== null) {
          // The handler should produce $…$ for inline math or $$…$$ for display math.
          const isInlineTex = result.captured.startsWith('$') && !result.captured.startsWith('$$');
          const isDisplayTex = result.captured.startsWith('$$');
          if (isInlineTex || isDisplayTex) {
            pass(`/view srcdoc math-copy: captured TeX "${result.captured}" — handler working correctly`);
          } else {
            fail(`/view srcdoc math-copy: captured "${result.captured}" — expected $…$ or $$…$$`);
          }
        } else {
          fail('/view srcdoc math-copy: copy handler did not call clipboardData.setData (null captured)');
        }
        break;
      } catch (_) {}
    }
    if (!mathCopyTested) {
      fail('/view srcdoc math-copy: could not locate srcdoc frame to run copy test');
    }
  } else {
    fail('/view srcdoc: skipping math-copy test (KaTeX not rendered)');
  }
  await viewPage.close();

  // ── Step 4: /srcdoc endpoint — bootstrap contains math-copy ─────────────
  console.log('\n--- Step 4: /srcdoc endpoint — bootstrap contains math-copy logic ---');
  {
    const p = await ctx.newPage();
    const r = await p.goto(`${baseUrl}/srcdoc`, { waitUntil: 'load' });
    if (r?.status() === 200) {
      const html = await p.content();
      if (html.includes('enableMathCopyAsTex') || (html.includes('clipboardData') && html.includes('katex'))) {
        pass('/srcdoc bootstrap: math-copy handler present');
      } else {
        fail('/srcdoc bootstrap: math-copy handler NOT found');
      }
      if (html.includes('mermaid.min.js') && html.includes('katex.min.js')) {
        pass('/srcdoc: loads mermaid + katex from bundle origin');
      } else {
        fail('/srcdoc: missing mermaid or katex bundle references');
      }
      if (html.includes("connect-src 'none'")) {
        pass('/srcdoc CSP: connect-src none (zero egress intact)');
      } else {
        fail("/srcdoc CSP: connect-src 'none' missing — security regression");
      }
    } else {
      fail(`/srcdoc returned ${r?.status()}`);
    }
    await p.close();
  }

  // ── Step 5: Editor page HTML (edit mode daemon) ──────────────────────────
  // Start a second daemon in edit mode. The editor page requires auth (floor)
  // so /edit returns 403 for unauthenticated requests even for 0o644 files.
  // We check:
  //   a) the /srcdoc on the edit daemon has math-copy in its bootstrap
  //   b) the editor bundle still serves the key files
  console.log('\n--- Step 5: Editor page (edit-mode daemon) ---');
  let editDaemon;
  try {
    const d2 = await startDaemon(['--edit', testFile]);
    editDaemon = d2.proc;
    const editPort = d2.port;
    await sleep(500);
    const editBase = `http://127.0.0.1:${editPort}`;

    // Check /edit returns something (403 expected without auth session).
    {
      const p = await ctx.newPage();
      const r = await p.goto(`${editBase}/edit?path=${encodeURIComponent(testFile)}`, { waitUntil: 'domcontentloaded' });
      const status = r?.status();
      if (status === 200) {
        const html = await p.content();
        // NEW: editor preview uses srcdoc iframe, NOT innerHTML
        if (html.includes('/srcdoc') || (html.includes('sandbox') && html.includes('allow-scripts'))) {
          pass('Editor page: preview uses srcdoc iframe model (sandbox + /srcdoc fetch)');
        } else {
          fail('Editor page: preview does NOT use the srcdoc model');
        }
        if (!html.includes('enableMathCopyAsTex')) {
          pass('Editor page: enableMathCopyAsTex removed (now in srcdoc bootstrap)');
        } else {
          fail('Editor page still imports/calls enableMathCopyAsTex (should only be in srcdoc)');
        }
        if (!html.includes('innerHTML') || !html.includes('previewDocEl')) {
          pass('Editor page: no old innerHTML preview approach detected');
        } else {
          fail('Editor page: still uses old previewDocEl.innerHTML approach');
        }
      } else if (status === 403) {
        pass(`/edit returns 403 (auth floor; edit-mode gated — expected)`);
        // Verify the srcdoc on the edit daemon
        await p.close();
        const p2 = await ctx.newPage();
        const r2 = await p2.goto(`${editBase}/srcdoc`, { waitUntil: 'load' });
        if (r2?.status() === 200) {
          const h2 = await p2.content();
          if (h2.includes('enableMathCopyAsTex') || (h2.includes('clipboardData') && h2.includes('katex'))) {
            pass('Edit daemon /srcdoc: math-copy handler present in bootstrap');
          } else {
            fail('Edit daemon /srcdoc: math-copy handler NOT found');
          }
          if (h2.includes('connect-src') && h2.includes("'none'")) {
            pass('Edit daemon /srcdoc CSP: connect-src none intact');
          } else {
            fail('Edit daemon /srcdoc CSP: connect-src not none — security regression');
          }
        } else {
          fail(`Edit daemon /srcdoc: ${r2?.status()}`);
        }
        await p2.close();
      } else {
        fail(`/edit returned unexpected status ${status}`);
        await p.close();
      }
    }

    // Verify the editor bundle still serves preview-runtime.es.js (even though
    // enableMathCopyAsTex is no longer CALLED from editor_page.rs, the bundle
    // file must still exist for other potential consumers).
    {
      const p = await ctx.newPage();
      const r = await p.goto(`${editBase}/editor-bundle/preview-runtime.es.js`, { waitUntil: 'load' });
      if (r?.status() === 200) {
        pass('/editor-bundle/preview-runtime.es.js: still served (200)');
      } else {
        fail(`/editor-bundle/preview-runtime.es.js: ${r?.status()}`);
      }
      await p.close();
    }

  } catch (e) {
    fail(`Edit-mode daemon test failed: ${e.message}`);
  } finally {
    if (editDaemon) editDaemon.kill();
  }

  // ── Summary ──────────────────────────────────────────────────────────────
  console.log('\n=== Summary ===');
  const passed = results.filter(r => r.ok).length;
  const failCount = results.filter(r => !r.ok).length;
  console.log(`${passed} passed, ${failCount} failed`);

  await browser.close();
  if (daemon) daemon.kill();

  if (failCount > 0) {
    console.error('\nFailed:');
    results.filter(r => !r.ok).forEach(r => console.error('  ✗', r.msg));
    process.exit(1);
  }
}

main().catch(e => {
  console.error('Fatal:', e);
  if (browser) browser.close().catch(() => {});
  if (daemon) daemon.kill();
  process.exit(1);
});
