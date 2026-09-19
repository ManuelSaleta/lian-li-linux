import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import ts from "typescript";

const source = await readFile(new URL("../src/utils/fanQuantity.ts", import.meta.url), "utf8");
const javascript = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022 } }).outputText;
const { stageFanQuantity } = await import(`data:text/javascript;base64,${Buffer.from(javascript).toString("base64")}`);
const config = { ene6k77: {} };
const device = { device_id: "hid:0cf2:a102:1-2:port2", serial: "shared", max_fan_quantity: 4 };
assert.equal(stageFanQuantity(config, device, 3), 3);
const request = JSON.parse(JSON.stringify({ method: "SetConfig", params: { config } }));
assert.deepEqual(request.params.config.ene6k77, { "hid:0cf2:a102:1-2": { fan_quantities: { "2": 3 } } });
assert.equal(stageFanQuantity(config, device, 6), 4);
assert.equal(stageFanQuantity(config, { ...device, max_fan_quantity: 6 }, 6), 6);
assert.equal(stageFanQuantity(config, device, -1), 0);
for (const invalid of ["hid:6243168001", "hid:6243168001:group2", "hid:6243168001:port4"]) {
  assert.equal(stageFanQuantity(config, { ...device, device_id: invalid }, 3), undefined);
}
assert.equal(stageFanQuantity(config, device, NaN), undefined);
assert.deepEqual(Object.keys(config.ene6k77["hid:0cf2:a102:1-2"].fan_quantities), ["2"]);
assert.equal(stageFanQuantity(config, { ...device, device_id: "hid:0cf2:a102:1-3:port2" }, 2), 2);
assert.equal(config.ene6k77["hid:0cf2:a102:1-3"].fan_quantities["2"], 2);
assert.equal(config.ene6k77["hid:0cf2:a102:1-2"].fan_quantities["2"], 0);
assert.equal(stageFanQuantity(config, { ...device, serial: null }, 1), 1);
console.log("ENE fan quantity config keys and model limits passed");
