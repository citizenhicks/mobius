import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { readFileSync } from "node:fs";
const COMPUTER_WORKER = readFileSync(new URL("worker.cjs", import.meta.url), "utf8");

type Part = { type: "text"; text: string } | { type: "image"; path: string; detail: string };
type Observation = { content: Part[]; is_error: boolean };
type Evaluate = (code: string) => Promise<Observation>;
const runtime = process.env.MOBIUS_COMPUTER_RUNTIME;

async function withWorker(signal: AbortSignal, run: (evaluate: Evaluate, directory: string) => Promise<void>, host?: (request: any) => unknown) {
  const directory = await mkdtemp(join(tmpdir(), "mobius-computer-test-"));
  const script = join(directory, "worker.cjs");
  await writeFile(script, COMPUTER_WORKER);
  const child = spawn(process.execPath, [script], {
    env: { ...process.env, TMPDIR: directory, ...(runtime ? {
      NODE_PATH: join(runtime, "node_modules"), PLAYWRIGHT_BROWSERS_PATH: join(runtime, "browsers"),
    } : {}) },
    stdio: ["pipe", "pipe", "pipe"],
    detached: true,
  });
  const closed = new Promise<void>(resolve => child.once("close", () => resolve()));
  child.stderr.resume();
  const kill = () => {
    try { process.kill(-child.pid!, "SIGKILL"); }
    catch (error) { if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error; }
  };
  signal.addEventListener("abort", kill, { once: true });
  const output = child.stdout[Symbol.asyncIterator]();
  let buffer = Buffer.alloc(0);
  async function evaluate(code: string): Promise<Observation> {
    const payload = Buffer.from(JSON.stringify({ code }));
    const header = Buffer.alloc(4);
    header.writeUInt32BE(payload.length);
    child.stdin.write(Buffer.concat([header, payload]));
    for (;;) {
      while (buffer.length < 4 || buffer.length < (buffer.readUInt32BE(0) & 0x3fffffff) + 4) {
        const next = await output.next();
        assert.equal(next.done, false, "worker exited before its response");
        buffer = Buffer.concat([buffer, next.value]);
      }
      const header = buffer.readUInt32BE(0);
      const size = header & 0x3fffffff;
      const response = JSON.parse(buffer.subarray(4, size + 4).toString());
      buffer = buffer.subarray(size + 4);
      if (!(header & 0x80000000)) return response;
      const reply = Buffer.from(host ? JSON.stringify(host(response)) : "native control denied by sandbox");
      const prefix = Buffer.alloc(4);
      prefix.writeUInt32BE((reply.length | 0x80000000 | (host ? 0 : 0x40000000)) >>> 0);
      child.stdin.write(Buffer.concat([prefix, reply]));
    }
  }
  try { await run(evaluate, directory); }
  finally {
    child.stdin.end();
    const timer = setTimeout(kill, 1000);
    await closed;
    clearTimeout(timer);
    signal.removeEventListener("abort", kill);
    await rm(directory, { recursive: true });
  }
}

function text(result: Observation) {
  return result.content.filter(part => part.type === "text").map(part => part.text).join("\n");
}
function images(result: Observation) {
  return result.content.filter(part => part.type === "image");
}

test("worker cancellation stops a pending evaluation", { timeout: 2000 }, async () => {
  await assert.rejects(withWorker(AbortSignal.timeout(100), async evaluate => {
    await evaluate("await new Promise(() => {})");
  }), /worker exited before its response/);
});

test("computer runtime preserves variables, ordered snapshots, and the original error after full logs", { timeout: 10000 }, t => withWorker(t.signal, async (evaluate, directory) => {
  const source = join(directory, "screen.png");
  await writeFile(source, "before");
  assert.equal((await evaluate("var count = await Promise.resolve(40)")).is_error, false);
  assert.match(text(await evaluate("count += 2")), /42/);
  const result = await evaluate(`console.log('first'); emitImage(${JSON.stringify(source)}); require('node:fs').writeFileSync(${JSON.stringify(source)}, 'after'); console.log('second'); emitImage(${JSON.stringify(source)}, 'high'); console.log('x'.repeat(40000)); throw new Error('observed failure')`);
  assert.equal(result.is_error, true);
  assert.deepEqual(result.content.map(part => part.type), ["text", "image", "text", "image", "text"]);
  const captures = images(result);
  assert.equal(await readFile(captures[0].path, "utf8"), "before");
  assert.equal(await readFile(captures[1].path, "utf8"), "after");
  assert.equal(captures[1].detail, "high");
  assert.match(text(result), /Original error: Error: observed failure/);
  assert.match(text(result), /session state: retained/);
  assert.ok(Buffer.byteLength(text(result)) <= 40000);
  assert.match(text(await evaluate("count")), /42/);
  for (const [expression, message] of [["'plain failure'", "plain failure"], ["null", "null"], ["undefined", "undefined"]]) {
    const failure = await evaluate(`throw ${expression}`);
    assert.equal(failure.is_error, true);
    assert.ok(text(failure).includes(`Original error: ${message}`));
  }
}));

test("browser action failure captures fresh state without replay, exceeding capture deadlines, or leaking late output", { skip: !runtime, timeout: 20000 }, t => withWorker(t.signal, async evaluate => {
  const html = `<form onsubmit="event.preventDefault(); window.submissions=(window.submissions||0)+1; document.getElementById('result').textContent='Saved: '+this.elements.q.value"><label>Search<input name=q></label></form><p id=result></p>`;
  assert.equal((await evaluate(`var page=await getPage(); await page.setContent(${JSON.stringify(html)});`)).is_error, false);
  const failure = await evaluate("await page.getByRole('textbox',{name:'Search'}).fill('möbius'); await page.getByRole('textbox',{name:'Search'}).press('Enter'); console.log('x'.repeat(40000)); await page.getByRole('button',{name:'Temporary suggestion'}).click({timeout:80});");
  assert.equal(failure.is_error, true);
  assert.match(text(failure), /Failure: action_timeout; session state: retained/);
  assert.match(text(failure), /Temporary suggestion/);
  assert.match(text(failure), /Observed tab 1, URL: about:blank/);
  assert.match(text(failure), /Saved: möbius/);
  assert.equal(images(failure).length, 1);
  assert.ok((await readFile(images(failure)[0].path)).subarray(0, 4).equals(Buffer.from("\x89PNG", "binary")));
  assert.equal(text(await evaluate("console.log(await page.evaluate(()=>window.submissions));")), "1");

  const tabFailure = await evaluate("var otherPage=await page.context().newPage(); await otherPage.setContent('<h1>Other</h1>'); var page=otherPage; throw new Error('second tab failed');");
  assert.match(text(tabFailure), /Observed tab 2/);
  assert.match(text(tabFailure), /heading "Other"/);
  const captureDeadline = await evaluate("var originalCapture=page.screenshot; page.screenshot=async()=>{await require('node:timers/promises').setTimeout(2300); return originalCapture.call(page);}; throw new Error('capture exceeds deadline');");
  assert.match(text(captureDeadline), /Original error: Error: capture exceeds deadline/);
  assert.match(text(captureDeadline), /Failure screenshot unavailable: capture deadline expired/);
  assert.match(text(captureDeadline), /heading "Other"/);
  const next = await evaluate("page.screenshot=originalCapture; await require('node:timers/promises').setTimeout(1000); console.log('clean next evaluation');");
  assert.deepEqual(next, { content: [{ type: "text", text: "clean next evaluation" }], is_error: false });

  const fullImages = await evaluate("for(var i=0;i<16;i++){await screenshot();} throw new Error('after captures');");
  assert.equal(images(fullImages).length, 16);
  assert.match(text(fullImages), /One earlier image omitted/);
  assert.match(text(fullImages), /Original error: Error: after captures/);
  assert.notDeepEqual(await readFile(images(fullImages)[0].path), await readFile(images(fullImages).at(-1)!.path));
  const lost = await evaluate("await page.context().browser().close(); throw new Error('browser disconnected');");
  assert.match(text(lost), /Failure: browser_lost; session state: lost; interpreter state: retained/);
}));


test("native actions share the evaluation pipe, preserve state, and emit screenshots once", { timeout: 5000 }, t => {
  const actions: string[] = [];
  return withWorker(t.signal, async evaluate => {
    assert.equal((await evaluate("var apps = await desktop.apps(); console.log(apps)")).is_error, false);
    assert.match(text(await evaluate("apps[0].pid")), /123/);
    const result = await evaluate("var shot = await desktop.screenshot(); console.log(shot.screenshotId)");
    assert.equal(result.is_error, false);
    assert.equal(images(result).length, 1);
    assert.equal(await readFile(images(result)[0].path, "utf8"), "native-image");
    assert.ok(!text(result).includes("bmF0aXZlLWltYWdl"));
    assert.equal((await evaluate("desktop.click(shot.screenshotId, 10, 20)")).is_error, false);
    assert.deepEqual(actions, ["apps", "screenshot", "click"]);
  }, request => {
    actions.push(request.action);
    if (request.action === "apps") return { result: [{ pid: 123 }] };
    if (request.action === "screenshot") return { result: { screenshotId: "observed", png: Buffer.from("native-image").toString("base64") } };
    assert.deepEqual(request, { screenshotId: "observed", x: 10, y: 20, button: "left", clicks: 1, action: "click" });
    return { result: true };
  });
});

test("native sandbox denial preserves interpreter without replaying the action", { timeout: 5000 }, t => withWorker(t.signal, async evaluate => {
  const result = await evaluate("var kept = 7; await desktop.apps()");
  assert.equal(result.is_error, true);
  assert.match(text(result), /native control denied by sandbox/);
  assert.equal((text(result).match(/Original error:/g) ?? []).length, 1);
  const recovered = await evaluate("try { await desktop.apps() } catch { console.log('handled') }");
  assert.equal(recovered.is_error, false);
  assert.equal(text(recovered), "handled");
  assert.match(text(result), /Inspect it before continuing/);
  assert.match(text(await evaluate("kept")), /7/);
}));
