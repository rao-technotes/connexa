// Bundles the web client and Chrome extension with esbuild.
//   node build.mjs            build both
//   node build.mjs web        build one target
//   node build.mjs --watch    rebuild on change
import * as esbuild from "esbuild";
import { cpSync, mkdirSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const watch = args.includes("--watch");
const only = args.filter((a) => !a.startsWith("--"));

const targets = {
  web: {
    dir: join(here, "web"),
    entries: { app: "src/main.ts" },
  },
  desktop: {
    dir: join(here, "desktop"),
    entries: { app: "src/main.ts" },
  },
  extension: {
    dir: join(here, "chrome-extension"),
    entries: { popup: "src/popup.ts", session: "src/session.ts", options: "src/options.ts" },
  },
  android: {
    dir: join(here, "android", "web"),
    entries: { app: "src/main.ts" },
    // Bundled into the APK and served by WebViewAssetLoader.
    outdir: join(here, "android", "app", "src", "main", "assets", "www"),
  },
};

for (const [name, target] of Object.entries(targets)) {
  if (only.length && !only.includes(name)) continue;
  const outdir = target.outdir ?? join(target.dir, "dist");
  rmSync(outdir, { recursive: true, force: true });
  mkdirSync(outdir, { recursive: true });
  cpSync(join(target.dir, "public"), outdir, { recursive: true });
  cpSync(join(here, "shared", "styles.css"), join(outdir, "styles.css"));

  const options = {
    entryPoints: Object.fromEntries(Object.entries(target.entries).map(([k, v]) => [k, join(target.dir, v)])),
    outdir,
    bundle: true,
    format: "esm",
    target: ["chrome116", "firefox118", "safari16"],
    sourcemap: true,
    minify: !watch,
    logLevel: "info",
  };
  if (watch) {
    const ctx = await esbuild.context(options);
    await ctx.watch();
  } else {
    await esbuild.build(options);
  }
  console.log(`${name}: ${outdir}`);
}
