import { defineConfig } from "vite";

export default defineConfig({
  // Relative asset URLs so the built bundle works from any directory —
  // the release tarball is meant to be unpacked and served as-is, not
  // deployed to a fixed path.
  base: "./",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2022",
    // Emit assets beside index.html rather than in assets/. The release
    // artifact is a "unpack it and serve the directory" tarball, and a flat
    // tree is the one shape that survives archiving without the HTML's
    // relative references breaking.
    assetsDir: "",
    rollupOptions: {
      // Two pages: the shell, and the P3 gate benchmark (#31). The benchmark
      // ships in the bundle rather than living in a scratch directory so the
      // gate can be re-run against any release artifact, on any machine, which
      // is the only way "this must never silently regress" means anything.
      input: { main: "index.html", bench: "bench.html" },
    },
  },
});
