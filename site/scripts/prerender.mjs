// Injects the server-rendered App into dist/index.html (static HTML for crawlers).
import { readFileSync, writeFileSync, rmSync } from "node:fs";

const { render } = await import("../dist-server/entry-server.js");
const file = "dist/index.html";
const html = readFileSync(file, "utf8");
const marker = '<div id="root"></div>';
if (!html.includes(marker)) throw new Error("prerender: #root marker not found");
writeFileSync(file, html.replace(marker, `<div id="root">${render()}</div>`));
rmSync("dist-server", { recursive: true, force: true });
console.log("prerendered dist/index.html");
