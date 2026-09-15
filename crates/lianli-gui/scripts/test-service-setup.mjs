import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import ts from "typescript";

const source = await readFile(new URL("../src/utils/serviceSetup.ts", import.meta.url), "utf8");
const javascript = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022 } }).outputText;
const { canSetUpServices } = await import(`data:text/javascript;base64,${Buffer.from(javascript).toString("base64")}`);
const known = (value) => ({ state: "known", value });
const unit = { load_state: "loaded", active_state: "inactive", main_pid: 0, unit_file_state: "disabled" };
const fresh = {
  context: { kind: "native" }, operation_lock: known({}), selection: known(null),
  ownership: known({ owner_pid: null }), global_user: known("disabled"),
  user: known(unit), system: known(unit),
};
assert.equal(canSetUpServices(fresh), true);
assert.equal(canSetUpServices(undefined), false);
for (const patch of [
  { context: { kind: "distrobox", name: "box" } },
  { operation_lock: { state: "unavailable" } },
  { selection: known({ scope: "user", uid: 1000 }) },
  { selection: { state: "unavailable" } },
  { ownership: known({ owner_pid: 42 }) },
  { ownership: { state: "unavailable" } },
  { global_user: known("enabled") },
]) assert.equal(canSetUpServices({ ...fresh, ...patch }), false);
for (const scope of ["user", "system"]) {
  assert.equal(canSetUpServices({ ...fresh, [scope]: { state: "unavailable" } }), false);
  for (const patch of [
    { load_state: "not-found" }, { active_state: "activating" },
    { active_state: "failed" }, { main_pid: 42 }, { unit_file_state: "enabled" },
    { unit_file_state: "masked" },
  ]) assert.equal(canSetUpServices({ ...fresh, [scope]: known({ ...unit, ...patch }) }), false);
}
console.log("Fresh service setup and conflicting-state checks passed");
