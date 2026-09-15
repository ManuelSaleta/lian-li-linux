import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import ts from "typescript";

const source = await readFile(new URL("../src/utils/boundedFetch.ts", import.meta.url), "utf8");
const javascript = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022 } }).outputText;
const { boundedFetch } = await import(`data:text/javascript;base64,${Buffer.from(javascript).toString("base64")}`);
const originalFetch = globalThis.fetch;
try {
  globalThis.fetch = async () => new Response("hello", { headers: { "content-type": "text/plain" } });
  const body = await boundedFetch("fixture", 5, new AbortController().signal);
  assert.equal(await body.text(), "hello");
  assert.equal(body.type, "text/plain");

  let cancelled = false;
  globalThis.fetch = async () => new Response(new ReadableStream({
    start(controller) { controller.enqueue(new Uint8Array(4)); controller.enqueue(new Uint8Array(4)); },
    cancel() { cancelled = true; },
  }));
  await assert.rejects(boundedFetch("fixture", 5, new AbortController().signal), /exceeds 5/);
  assert.equal(cancelled, true);

  globalThis.fetch = async () => new Response("small", { headers: { "content-length": "100" } });
  await assert.rejects(boundedFetch("fixture", 5, new AbortController().signal), /exceeds 5/);
  globalThis.fetch = async () => new Response("", { status: 503 });
  await assert.rejects(boundedFetch("fixture", 5, new AbortController().signal), /HTTP 503/);

  globalThis.fetch = async (_url, { signal }) => new Promise((_resolve, reject) => {
    signal.addEventListener("abort", () => reject(signal.reason), { once: true });
  });
  await assert.rejects(boundedFetch("fixture", 5, new AbortController().signal, 5), /timed out/);
  const parent = new AbortController();
  const pending = boundedFetch("fixture", 5, parent.signal);
  parent.abort(new Error("window closed"));
  await assert.rejects(pending, /window closed/);
  globalThis.fetch = () => { throw new Error("Cancelled downloads must not start"); };
  await assert.rejects(boundedFetch("fixture", 5, parent.signal), /window closed/);
  console.log("Bounded download regression checks passed");
} finally {
  globalThis.fetch = originalFetch;
}
