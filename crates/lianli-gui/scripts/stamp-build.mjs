import { createHash } from "node:crypto";
import { lstat, readFile, readdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

const manifestName = ".lianli-build.json";
const inputs = {};
const outputs = {};

async function collect(root, path, hashes) {
  const absolute = join(root, path);
  const stat = await lstat(absolute);
  if (stat.isDirectory()) {
    for (const name of (await readdir(absolute)).sort()) {
      await collect(root, join(path, name), hashes);
    }
  } else if (stat.isFile()) {
    hashes[path] = createHash("sha256").update(await readFile(absolute)).digest("hex");
  } else {
    throw new Error(`Frontend build input/output must be a regular file: ${absolute}`);
  }
}

for (const path of ["package.json", "package-lock.json", "vite.config.ts", "tsconfig.json", "tsconfig.node.json", "index.html", "src", "scripts"]) {
  await collect(".", path, inputs);
}
try {
  await lstat("public");
  await collect(".", "public", inputs);
} catch (error) {
  if (error.code !== "ENOENT") throw error;
}
for (const name of (await readdir("dist")).sort()) {
  if (name !== manifestName) await collect("dist", name, outputs);
}
if (!outputs["index.html"] || !Object.keys(outputs).some(path => path.endsWith(".js"))) {
  throw new Error("Frontend build is missing its HTML or JavaScript output");
}
await writeFile(join("dist", manifestName), JSON.stringify({ inputs, outputs }) + "\n");
