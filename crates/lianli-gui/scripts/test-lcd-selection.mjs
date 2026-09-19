import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import ts from "typescript";

const source = await readFile(new URL("../src/utils/lcdSelection.ts", import.meta.url), "utf8");
const javascript = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022 } }).outputText;
const { resolveLcdDevice } = await import(`data:text/javascript;base64,${Buffer.from(javascript).toString("base64")}`);
const first = { device_id: "hid:0416:7371:1-2", serial: "shared" };
const second = { device_id: "hid:0416:7371:1-3", serial: "shared" };
const entry = { serial: second.device_id, index: 0 };
assert.equal(resolveLcdDevice(entry, [first, second]), second);
assert.equal(resolveLcdDevice(entry, [second, first]), second);
assert.equal(resolveLcdDevice(entry, [first]), undefined);
assert.equal(entry.serial, second.device_id);
assert.equal(resolveLcdDevice({ serial: "shared" }, [first, second]), undefined);
assert.equal(resolveLcdDevice({ serial: "0416:7371:1-3" }, [first, second]), second);
assert.equal(resolveLcdDevice({ index: 1 }, [first, second]), second);
assert.equal(resolveLcdDevice(entry, []), undefined);
console.log("LCD physical selection and disconnected-device preservation passed");
