import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import ts from "typescript";

const source = await readFile(new URL("../src/utils/displaySwitch.ts", import.meta.url), "utf8");
const javascript = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022 } }).outputText;
const { displaySwitchComplete } = await import(`data:text/javascript;base64,${Buffer.from(javascript).toString("base64")}`);
const lcd = { device_id: "hid:1a86:aa01:8-9.1", family: "UniversalScreen" };
const desktop = { device_id: "hid:1a86:ad21:8-9.1", family: "UniversalScreenDesktop" };
assert.equal(displaySwitchComplete(lcd, [lcd]), false);
assert.equal(displaySwitchComplete(lcd, []), false);
assert.equal(displaySwitchComplete(lcd, [desktop]), true);
assert.equal(displaySwitchComplete(desktop, [lcd]), true);
assert.equal(displaySwitchComplete(lcd, [{ ...desktop, device_id: "hid:1a86:ad21:8-9.2" }]), false);
assert.equal(displaySwitchComplete(lcd, [{ ...desktop, family: "UniversalScreenLighting" }]), false);
const serial = { ...lcd, device_id: "hid:unique-screen" };
assert.equal(displaySwitchComplete(serial, [{ ...desktop, device_id: serial.device_id }]), true);
assert.equal(displaySwitchComplete(serial, [{ ...desktop, device_id: "hid:other-screen" }]), false);
console.log("Display switch identity and re-enumeration checks passed");
