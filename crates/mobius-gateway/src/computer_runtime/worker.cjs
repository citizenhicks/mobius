'use strict';
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
// Operator launch policy is supplied by the gateway, never by an evaluation frame.
const browserOptions = JSON.parse(process.argv[2] ?? fs.readFileSync(path.join(__dirname, 'browser.json'), 'utf8'));
process.env.PLAYWRIGHT_BROWSERS_PATH = path.resolve(__dirname, browserOptions.browsers_directory);
const crypto = require('node:crypto');
const vm = require('node:vm');
const { Session } = require('node:inspector/promises');
const { inspect } = require('node:util');
const MAX_FRAME = 1024 * 1024;
const MAX_HOST_REPLY = 64 * 1024 * 1024;
const HOST_REQUEST = 0x80000000;
const HOST_ERROR = 0x40000000;
const MAX_TEXT_BYTES = 40000;
const FAILURE_TEXT_BYTES = 16000;
const FAILURE_CAPTURE_MS = 1500;
function clipped(value, limit) {
  const bytes = Buffer.from(String(value));
  if (bytes.length <= limit) return String(value);
  let marker = '\n[output truncated]';
  if (Buffer.byteLength(marker) > limit) marker = '';
  return new TextDecoder().decode(bytes.subarray(0, limit - Buffer.byteLength(marker)), {stream:true}) + marker;
}
let content = [], textBytes = 0, imageCount = 0, active = false, connection, page;
let nativePending, nativeTask, desktopUsed = false;
// Actions that can change what is on screen; reading the screen does not.
const ACTING = new Set(['open_app','activate','press','set_value','click','move','drag','scroll','type_text','press_key']);
let desktopActed = false, pageUsed = false, gated = false;
function writeFrame(payload, flags = 0) {
  const header = Buffer.alloc(4);
  header.writeUInt32BE((payload.length | flags) >>> 0);
  process.stdout.write(header);
  process.stdout.write(payload);
}
function nativeCall(action, args = {}) {
  if (!active) throw new Error('Desktop actions require an active evaluation');
  if (nativePending) throw new Error('Await each desktop action before starting another');
  desktopUsed = true;
  if (ACTING.has(action)) desktopActed = true;
  nativeTask = hostCall({...args, action}).then(result => {
    if (action === 'screenshot' || action === 'zoom') {
      if (typeof result?.png !== 'string') throw new Error('Desktop screenshot is missing');
      const bytes = Buffer.from(result.png, 'base64');
      if (bytes.length === 0 || bytes.length > 50 * 1024 * 1024) throw new Error('Desktop screenshot exceeds its limit');
      const target = path.join(os.tmpdir(), 'desktop-' + crypto.randomUUID() + '.png');
      fs.writeFileSync(target, bytes, {flag:'wx'});
      delete result.png;
      try { text((action === 'zoom' ? 'Mac zoom: ' : 'Mac screenshot: ') + JSON.stringify(result)); emitImage(target); }
      finally { fs.unlinkSync(target); }
    }
    return result;
  });
  // The evaluation also waits for a caller that forgot to await its last native action.
  nativeTask.catch(()=>{});
  return nativeTask;
}
function hostCall(request) {
  if (nativePending) throw new Error('Await each host request before starting another');
  const payload = Buffer.from(JSON.stringify(request));
  if (payload.length > MAX_FRAME) throw new Error('Desktop request exceeds its limit');
  return new Promise((resolve, reject) => {
    nativePending = {resolve, reject};
    writeFrame(payload, HOST_REQUEST);
  }).then(reply => {
    if (reply.error) throw new Error(reply.error);
    return reply.result;
  });
}
const desktop = Object.freeze({
  apps: () => nativeCall('apps'),
  displays: () => nativeCall('displays'),
  openApp: bundleId => nativeCall('open_app', {bundleId}),
  activate: pid => nativeCall('activate', {pid}),
  inspect: (pid, options = {}) => nativeCall('inspect', {pid, ...options}),
  press: elementId => nativeCall('press', {elementId}),
  setValue: (elementId, text) => nativeCall('set_value', {elementId, text}),
  screenshot: displayId => nativeCall('screenshot', displayId === undefined ? {} : {displayId}),
  zoom: (screenshotId, x, y, width, height) => nativeCall('zoom', {screenshotId, x, y, width, height}),
  click: (screenshotId, x, y, button = 'left', clicks = 1) => nativeCall('click', {screenshotId, x, y, button, clicks}),
  move: (screenshotId, x, y) => nativeCall('move', {screenshotId, x, y}),
  drag: (screenshotId, x, y, toX, toY) => nativeCall('drag', {screenshotId, x, y, toX, toY}),
  scroll: (screenshotId, x, y, deltaX, deltaY) => nativeCall('scroll', {screenshotId, x, y, deltaX, deltaY}),
  typeText: (pid, text) => nativeCall('type_text', {pid, text}),
  pressKey: (pid, key, modifiers = []) => nativeCall('press_key', {pid, key, modifiers}),
});
function text(value, limit = MAX_TEXT_BYTES - FAILURE_TEXT_BYTES) {
  if (!active) return;
  const message = (content.at(-1)?.type === 'text' ? '\n' : '') + String(value);
  const remaining = limit - textBytes;
  if (remaining <= 0) return;
  const kept = clipped(message, remaining);
  textBytes += Buffer.byteLength(kept);
  if (content.at(-1)?.type === 'text') content.at(-1).text += kept;
  else content.push({type:'text', text:kept});
}
function emitImage(source, detail = 'auto') {
  if (!active) throw new Error('emitImage requires an active evaluation');
  if (!['auto','low','high'].includes(detail) || imageCount >= 16) throw new Error('invalid detail or capture limit exceeded');
  const stat = fs.statSync(source);
  if (!stat.isFile() || stat.size === 0 || stat.size > 50 * 1024 * 1024) throw new Error('image capture exceeds size limit');
  const destination = path.join(os.tmpdir(), 'observation-' + crypto.randomUUID());
  // Snapshot now: subsequent code may overwrite the source before this result is recorded.
  fs.copyFileSync(source, destination, fs.constants.COPYFILE_EXCL);
  content.push({type:'image', path:destination, detail});
  imageCount += 1;
}
// A loopback DevTools port so a möbius app on this machine can watch and share the page.
let devtools;
// The gateway's browser for this evaluation, bound on first page use so desktop-only code
// never opens it; `requested` is that binding once made.
let browserSpec, requested, attached, observed = false, frontPending = false;
function browserKey(value) { return value && value.endpoint + '/' + value.target_id; }
function validateDesktop(value) {
  if (!value || typeof value.endpoint !== 'string' || Object.keys(value).some(key => key !== 'endpoint' && key !== 'target_id')) throw new Error('invalid gateway browser page');
  if (value.endpoint.startsWith('ws+unix://')) {
    const match = /^ws\+unix:\/\/(\/[^\s:?#%\\]+\/browser\.sock):\/([a-fA-F0-9]{32})$/.exec(value.endpoint);
    if (!match || value.endpoint.length > 4096 || match[1].split('/').some(part => part === '.' || part === '..') || value.target_id != null) throw new Error('invalid gateway browser endpoint');
    return;
  }
  if (typeof value.target_id !== 'string' || !/^[A-Za-z0-9_-]{1,256}$/.test(value.target_id)) throw new Error('invalid gateway browser page');
  const endpoint = new URL(value.endpoint);
  if (endpoint.protocol !== 'http:' || endpoint.hostname !== '127.0.0.1' || !endpoint.port || endpoint.username || endpoint.password || endpoint.pathname !== '/' || endpoint.search || endpoint.hash) throw new Error('invalid gateway browser endpoint');
}
function freePort() {
  return new Promise((resolve, reject) => {
    const server = require('node:net').createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => { const {port} = server.address(); server.close(() => resolve(port)); });
  });
}
// Disconnects from the gateway browser, or closes the worker's own browser.
async function release() {
  const previous = connection;
  connection = page = attached = devtools = undefined;
  observed = false;
  await previous?.close().catch(() => {});
}
async function getPage() {
  pageUsed = true;
  if (browserSpec && !requested) {
    requested = await hostCall({op:'begin_browser'});
    validateDesktop(requested);
    frontPending = requested.target_id != null;
  }
  if (page && (browserKey(attached) !== browserKey(requested) || page.isClosed() || !connection?.isConnected())) await release();
  if (!page) {
    try {
      if (!requested && browserOptions.sandbox && process.getuid?.() === 0) throw new Error(browserOptions.root_sandbox_error);
      const { chromium } = require(browserOptions.playwright_module);
      if (requested) {
        connection = await chromium.connectOverCDP(requested.endpoint, {noDefaults:true, timeout:10000});
        if (requested.target_id == null) {
          const pages = connection.contexts().flatMap(context => context.pages());
          if (pages.length !== 1) throw new Error('the scoped local browser must expose exactly one page');
          page = pages[0];
        } else for (const context of connection.contexts()) {
          for (const candidate of context.pages()) {
            const session = await context.newCDPSession(candidate);
            try {
              const {targetInfo} = await session.send('Target.getTargetInfo');
              if (targetInfo.targetId === requested.target_id) page = candidate;
            } finally { await session.detach(); }
            if (page) break;
          }
          if (page) break;
        }
        if (!page) throw new Error('the gateway-assigned browser page is unavailable');
        attached = requested;
      } else {
        // Without a loopback port (a sandbox without network) the browser still runs, unwatched.
        const port = await freePort().catch(() => undefined);
        const args = [...browserOptions.arguments];
        if (port) args.push('--remote-debugging-address=127.0.0.1', '--remote-debugging-port=' + port);
        connection = await chromium.launch({headless:true, chromiumSandbox:browserOptions.sandbox, args,
          ...(browserOptions.executable ? {executablePath:browserOptions.executable} : {})});
        devtools = port && 'http://127.0.0.1:' + port;
        const context = await connection.newContext({viewport:{width:browserOptions.viewport[0],height:browserOptions.viewport[1]}, deviceScaleFactor:1});
        page = await context.newPage();
        if (browserOptions.start_page !== 'about:blank') await page.goto(browserOptions.start_page);
      }
      page.setDefaultTimeout(10000);
    } catch (error) {
      await release();
      throw error;
    }
  }
  if (frontPending) {
    frontPending = false;
    await page.bringToFront();
  }
  if (requested && !observed) await firstContact(page);
  return page;
}
// The gateway browser may hold the user's logins or changes: its first use only observes.
async function firstContact(current) {
  observed = gated = true;
  text('Code after the first getPage() or screenshot() was not executed. The gateway browser may contain saved logins or changes made by the user. Inspect this fresh observation before acting.');
  text('URL: ' + current.url());
  text(await current.locator('body').ariaSnapshot());
  emitScreenshot(await capturePage(current));
  throw new Error('first browser observation');
}
// Acting is answered with its result: without an image already, end with a fresh one.
async function observeResult() {
  if (gated || imageCount > 0 || !(desktopActed || pageUsed)) return;
  try {
    if (desktopActed) {
      await new Promise(resolve => setTimeout(resolve, 300));
      await nativeCall('screenshot');
    } else if (page && !page.isClosed()) {
      emitScreenshot(await capturePage(page, FAILURE_CAPTURE_MS));
    }
  } catch {
    // ponytail: a missing closing screenshot only costs the model one explicit capture.
  }
}
function capturePage(current, timeout) {
  return current.screenshot({fullPage:false, scale:'css', timeout});
}
// A headed page has no fixed viewport; its PNG header says how large the capture is.
function emitScreenshot(bytes, limit = MAX_TEXT_BYTES - FAILURE_TEXT_BYTES) {
  if (bytes.length < 24 || !bytes.subarray(0, 8).equals(Buffer.from([137,80,78,71,13,10,26,10]))) throw new Error('Invalid browser screenshot PNG');
  const width = bytes.readUInt32BE(16), height = bytes.readUInt32BE(20);
  if (!width || !height || Math.max(width, height) > browserOptions.image_presentation.max_dimension || Math.ceil(width / 32) * Math.ceil(height / 32) > browserOptions.image_presentation.max_patches) throw new Error('Browser capture exceeds presentation limits; reduce the viewport or headed window size before capturing. CSS coordinates are preserved.');
  const target = path.join(os.tmpdir(), 'screen-' + crypto.randomUUID() + '.png');
  fs.writeFileSync(target, bytes);
  try {
    const size = {width:bytes.readUInt32BE(16), height:bytes.readUInt32BE(20)};
    text('Viewport screenshot; one image pixel = one CSS pixel. Coordinates are viewport-relative. Size: ' + JSON.stringify(size), limit);
    emitImage(target);
  } finally { fs.unlinkSync(target); }
}
async function screenshot() {
  const current = await getPage();
  emitScreenshot(await capturePage(current));
}
async function reportFailure(error, scope, interpreterState = 'retained') {
  if (desktopUsed) text('Mac desktop state is external to this interpreter. Inspect it before continuing; do not automatically repeat an action.', MAX_TEXT_BYTES);
  const browserLost = connection && !connection.isConnected();
  let kind = 'evaluation_error';
  if ((error?.className ?? error?.name) === 'TimeoutError' || error?.description?.startsWith('TimeoutError:')) kind = 'action_timeout';
  if (browserLost) kind = 'browser_lost';
  const sessionState = browserLost ? 'lost' : interpreterState;
  text('Failure: ' + kind + '; session state: ' + sessionState + '; interpreter state: ' + interpreterState + '.\nOriginal error: ' + clipped(error?.description ?? error?.stack ?? String(error?.type ? error.value : error), 8000), MAX_TEXT_BYTES);
  try {
    const pages = connection?.contexts().flatMap(context => context.pages()) ?? [];
    const current = pages.includes(scope.page) ? scope.page : page;
    let browserState = connection ? 'retained' : 'not started';
    if (browserLost) browserState = 'lost';
    text('Browser state: ' + browserState + '. Tabs: ' + clipped(JSON.stringify(pages.map((tab, index) => ({tab:index + 1, url:clipped(tab.url(), 512)}))), 2000), MAX_TEXT_BYTES);
    if (!current || current.isClosed() || browserLost) {
      text('Fresh observation unavailable: the observed page is unavailable. Page state: ' + (current?.isClosed() ? 'lost' : 'unknown') + '.', MAX_TEXT_BYTES);
      return;
    }
    text('Observed tab ' + (pages.indexOf(current) + 1) + ', URL: ' + clipped(current.url(), 1000), MAX_TEXT_BYTES);
    // Late completion may fill these local slots, but cannot emit into a later evaluation.
    let capture, accessibility, captureError, accessibilityError, timer;
    try {
      await Promise.race([
        Promise.all([
          capturePage(current, FAILURE_CAPTURE_MS).then(value => { capture = value; }, error => { captureError = error; }),
          current.locator('body').ariaSnapshot({timeout:FAILURE_CAPTURE_MS}).then(value => { accessibility = value; }, error => { accessibilityError = error; })
        ]),
        new Promise(resolve => { timer = setTimeout(resolve, FAILURE_CAPTURE_MS); })
      ]);
    } finally { clearTimeout(timer); }
    text(accessibility === undefined ? 'Accessibility observation unavailable: ' + clipped(accessibilityError ?? 'capture deadline expired', 300) : 'Fresh accessibility snapshot:\n' + clipped(accessibility, 4000), MAX_TEXT_BYTES);
    if (capture) {
      // Keep the response's existing image bound while reserving its last slot for this failure.
      if (imageCount >= 16) {
        content.splice(content.findLastIndex(part => part.type === 'image'), 1);
        imageCount -= 1;
        text('One earlier image omitted to include the failure screenshot.', MAX_TEXT_BYTES);
      }
      try { emitScreenshot(capture, MAX_TEXT_BYTES); }
      catch (error) { text('Failure screenshot unavailable: ' + clipped(error, 300), MAX_TEXT_BYTES); }
    } else {
      text('Failure screenshot unavailable: ' + clipped(captureError ?? 'capture deadline expired', 300), MAX_TEXT_BYTES);
    }
  } catch (error) {
    text('Fresh observation unavailable: ' + clipped(error, 300), MAX_TEXT_BYTES);
  }
}
const inspector = new Session();
inspector.connect();
let contextId;
inspector.on('Runtime.executionContextCreated', ({params})=>{ if (params.context.name === 'mobius-computer') contextId = params.context.id; });
async function main() {
  await inspector.post('Runtime.enable');
  const scope = vm.createContext({getPage, screenshot, desktop, emitImage, require, console:{
    log:(...args)=>text(args.map(value=>typeof value === 'string' ? value : inspect(value,{depth:3,maxStringLength:40000})).join(' ')),
    error:(...args)=>text(args.join(' '))
  }}, {name:'mobius-computer'});
  if (!contextId) throw new Error('interpreter context unavailable');
  async function evaluate(payload) {
    const request = JSON.parse(payload.toString('utf8'));
    const browser = request.desktop ?? undefined;
    if (typeof request.code !== 'string' || Buffer.byteLength(request.code) > 40000 || Object.keys(request).some(key=>key !== 'code' && key !== 'desktop')) throw new Error('invalid evaluation');
    if (browser !== undefined) validateDesktop(browser);
    content = []; textBytes = 0; imageCount = 0; active = true; desktopUsed = false; nativeTask = undefined;
    browserSpec = browser; requested = undefined; frontPending = false;
    desktopActed = pageUsed = gated = false;
    let is_error = false;
    try {
      // Saved page handles can bypass getPage(); renew their host lease before running code.
      if (attached) {
        if (browser) await getPage();
        else await release();
      }
      await runCode(request.code);
      await observeResult();
    } catch (error) {
      if (!gated) {
        is_error = true;
        await reportFailure(error, scope);
      }
    } finally {
      await inspector.post('Runtime.releaseObjectGroup', {objectGroup:'evaluation'}).catch(()=>{});
    }
    active = false;
    const live = connection?.isConnected() ? devtools : undefined;
    const response = Buffer.from(JSON.stringify(live ? {content,is_error,devtools:live} : {content,is_error}));
    if (response.length > MAX_FRAME) throw new Error('response frame exceeds limit');
    writeFrame(response);

    async function runCode(code) {
      const value = await inspector.post('Runtime.evaluate', {expression:code, contextId, awaitPromise:true, replMode:true, silent:true, objectGroup:'evaluation'});
      if (nativePending) {
        if (value.exceptionDetails) await nativeTask.catch(()=>{});
        else await nativeTask;
      }
      is_error = Boolean(value.exceptionDetails) && !gated;
      if (gated) return;
      if (is_error) await reportFailure(value.exceptionDetails.exception ?? value.result, scope);
      else if (value.result.type !== 'undefined') text(value.result.description ?? String(value.result.value));
    }
  }
  let buffer = Buffer.alloc(0), evaluation, chunks = [], chunkBytes = 0;
  for await (const chunk of process.stdin) {
    chunks.push(chunk);
    chunkBytes += chunk.length;
    const bufferedBytes = buffer.length + chunkBytes;
    if (bufferedBytes > MAX_HOST_REPLY + 4) throw new Error('request frame exceeds limit');
    // A screenshot reply can span many chunks; copy its payload only when the frame is complete.
    if (buffer.length >= 4 && bufferedBytes < (buffer.readUInt32BE(0) & 0x3fffffff) + 4) continue;
    buffer = Buffer.concat([buffer, ...chunks], bufferedBytes);
    chunks = []; chunkBytes = 0;
    while (buffer.length >= 4) {
      const header = buffer.readUInt32BE(0);
      const size = header & 0x3fffffff;
      const hostReply = Boolean(header & HOST_REQUEST);
      if (size === 0 || size > (hostReply ? MAX_HOST_REPLY : MAX_FRAME)) throw new Error('invalid request frame');
      if (buffer.length < size + 4) break;
      const payload = buffer.subarray(4, size + 4);
      buffer = buffer.subarray(size + 4);
      if (hostReply) {
        if (!nativePending) throw new Error('unexpected native reply');
        const pending = nativePending; nativePending = undefined;
        if (header & HOST_ERROR) pending.reject(new Error(payload.toString('utf8')));
        else pending.resolve(JSON.parse(payload.toString('utf8')));
      } else {
        if (header & HOST_ERROR || active) throw new Error('overlapping or invalid evaluation');
        evaluation = evaluate(payload);
        evaluation.catch(error => { process.stderr.write(String(error)); process.exit(1); });
      }
    }
  }
  nativePending?.reject(new Error('native connection ended; action outcome unknown'));
  await evaluation;
  if (buffer.length || chunkBytes) throw new Error('truncated request frame');
  void scope;
}
main().finally(async()=>{inspector.disconnect(); if (connection) await connection.close();}).catch(error=>{process.stderr.write(String(error)); process.exitCode=1;});
