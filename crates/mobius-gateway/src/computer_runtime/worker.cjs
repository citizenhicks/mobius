'use strict';
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
process.env.PLAYWRIGHT_BROWSERS_PATH ??= path.join(__dirname, 'browsers');
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
  const payload = Buffer.from(JSON.stringify({...args, action}));
  if (payload.length > MAX_FRAME) throw new Error('Desktop request exceeds its limit');
  nativeTask = new Promise((resolve, reject) => {
    nativePending = {resolve, reject};
    writeFrame(payload, HOST_REQUEST);
  }).then(reply => {
    if (reply.error) throw new Error(reply.error);
    const result = reply.result;
    if (action === 'screenshot') {
      if (typeof result?.png !== 'string') throw new Error('Desktop screenshot is missing');
      const bytes = Buffer.from(result.png, 'base64');
      if (bytes.length === 0 || bytes.length > 50 * 1024 * 1024) throw new Error('Desktop screenshot exceeds its limit');
      const target = path.join(os.tmpdir(), 'desktop-' + crypto.randomUUID() + '.png');
      fs.writeFileSync(target, bytes, {flag:'wx'});
      delete result.png;
      try { text('Mac screenshot: ' + JSON.stringify(result)); emitImage(target); }
      finally { fs.unlinkSync(target); }
    }
    return result;
  });
  // The evaluation also waits for a caller that forgot to await its last native action.
  nativeTask.catch(()=>{});
  return nativeTask;
}
const desktop = Object.freeze({
  apps: () => nativeCall('apps'),
  displays: () => nativeCall('displays'),
  openApp: bundleId => nativeCall('open_app', {bundleId}),
  activate: pid => nativeCall('activate', {pid}),
  inspect: pid => nativeCall('inspect', {pid}),
  press: elementId => nativeCall('press', {elementId}),
  setValue: (elementId, text) => nativeCall('set_value', {elementId, text}),
  screenshot: displayId => nativeCall('screenshot', displayId === undefined ? {} : {displayId}),
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
async function getPage() {
  if (!page) {
    const { chromium } = require('playwright');
    connection = await chromium.launch({headless:true});
    const context = await connection.newContext({viewport:{width:1365,height:768}, deviceScaleFactor:1});
    page = await context.newPage();
    page.setDefaultTimeout(10000);
  }
  return page;
}
function capturePage(current, timeout) {
  return current.screenshot({fullPage:false, scale:'css', timeout});
}
function emitScreenshot(bytes, current, limit = MAX_TEXT_BYTES - FAILURE_TEXT_BYTES) {
  const target = path.join(os.tmpdir(), 'screen-' + crypto.randomUUID() + '.png');
  fs.writeFileSync(target, bytes);
  try {
    text('Viewport screenshot; one image pixel = one CSS pixel. Coordinates are viewport-relative. Size: ' + JSON.stringify(current.viewportSize()), limit);
    emitImage(target);
  } finally { fs.unlinkSync(target); }
}
async function screenshot() {
  const current = await getPage();
  emitScreenshot(await capturePage(current), current);
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
      try { emitScreenshot(capture, current, MAX_TEXT_BYTES); }
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
    if (typeof request.code !== 'string' || Buffer.byteLength(request.code) > 40000 || Object.keys(request).some(key=>key !== 'code')) throw new Error('invalid evaluation');
    content = []; textBytes = 0; imageCount = 0; active = true; desktopUsed = false; nativeTask = undefined;
    let is_error = false;
    try {
      const value = await inspector.post('Runtime.evaluate', {expression:request.code, contextId, awaitPromise:true, replMode:true, silent:true, objectGroup:'evaluation'});
      if (nativePending) {
        if (value.exceptionDetails) await nativeTask.catch(()=>{});
        else await nativeTask;
      }
      is_error = Boolean(value.exceptionDetails);
      if (is_error) await reportFailure(value.exceptionDetails.exception ?? value.result, scope);
      else if (value.result.type !== 'undefined') text(value.result.description ?? String(value.result.value));
    } catch (error) {
      is_error = true;
      await reportFailure(error, scope);
    } finally {
      await inspector.post('Runtime.releaseObjectGroup', {objectGroup:'evaluation'}).catch(()=>{});
    }
    active = false;
    const response = Buffer.from(JSON.stringify({content,is_error}));
    if (response.length > MAX_FRAME) throw new Error('response frame exceeds limit');
    writeFrame(response);
  }
  let buffer = Buffer.alloc(0), evaluation;
  for await (const chunk of process.stdin) {
    buffer = Buffer.concat([buffer, chunk]);
    if (buffer.length > MAX_HOST_REPLY + 4) throw new Error('request frame exceeds limit');
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
  if (buffer.length) throw new Error('truncated request frame');
  void scope;
}
main().finally(async()=>{inspector.disconnect(); if (connection) await connection.close();}).catch(error=>{process.stderr.write(String(error)); process.exitCode=1;});
