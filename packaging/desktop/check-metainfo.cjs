const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const metadata = fs.readFileSync(process.argv[2] || path.join(__dirname, 'com.sgtaziz.lianlilinux.metainfo.xml'), 'utf8');
const rules = fs.readFileSync(process.argv[3] || path.join(__dirname, '../udev/60-lianli.rules'), 'utf8');
const expected = new Set();
for (const line of rules.split('\n')) {
  if (line.trimStart().startsWith('#') || !/SUBSYSTEM\s*==\s*"usb"/.test(line)) continue;
  const vendor = line.match(/ATTR\{idVendor\}\s*==\s*"([\da-f]{4})"/i);
  const product = line.match(/ATTR\{idProduct\}\s*==\s*"([\da-f]{4})"/i);
  assert(vendor && product, `Cannot identify USB rule: ${line}`);
  expected.add(`usb:v${vendor[1].toUpperCase()}p${product[1].toUpperCase()}d*`);
}
assert(expected.size > 0, 'No USB device rules found');
const provided = [...metadata.matchAll(/<modalias>([^<]+)<\/modalias>/g)].map(match => match[1]);
assert.equal(new Set(provided).size, provided.length, 'Duplicate AppStream device aliases');
assert.deepEqual([...new Set(provided)].sort(), [...expected].sort(), 'AppStream devices must match the packaged USB rules');
console.log(`AppStream metadata covers all ${expected.size} USB device identities.`);
