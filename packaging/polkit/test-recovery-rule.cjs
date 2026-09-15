const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const rules = [];
vm.runInNewContext(fs.readFileSync(process.argv[2] || path.join(__dirname, "49-lianli-recovery.rules"), "utf8"), {
  polkit: { addRule: rule => rules.push(rule), Result: { YES: "yes" } },
});
assert.equal(rules.length, 1);
const actionId = "org.freedesktop.systemd1.manage-units";
const unit = "lianli-control-recovery.service";
let checked = 0;
for (const id of [actionId, "org.freedesktop.systemd1.manage-unit-files"]) {
  for (const target of [unit, "lianli-daemon-system.service", "other.service", undefined]) {
    for (const verb of ["start", "stop", "restart", "kill", "set-property", undefined]) {
      for (const local of [true, false]) {
        for (const active of [true, false]) {
          const result = rules[0]({ id, lookup: key => ({ unit: target, verb })[key] }, { local, active });
          const allowed = id === actionId && target === unit && verb === "start" && local && active;
          assert.equal(result, allowed ? "yes" : undefined, JSON.stringify({ id, target, verb, local, active }));
          checked++;
        }
      }
    }
  }
}
console.log(`Recovery authorization: ${checked} cases passed`);
