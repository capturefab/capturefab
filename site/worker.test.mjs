// node --test site/: the download Worker against an in-memory bucket and a stubbed upstream.
import assert from "node:assert/strict";
import { test } from "node:test";
import worker from "./src/worker.ts";

const DATA = new TextEncoder().encode("0123456789");

function bucket(objects) {
  const meta = (key) => ({
    size: objects[key].length,
    httpEtag: `"${key}"`,
    writeHttpMetadata() {},
  });
  return {
    async head(key) {
      return objects[key] ? meta(key) : null;
    },
    async get(key, options) {
      if (!objects[key]) return null;
      const object = meta(key);
      if (options.onlyIf.get("if-none-match") === object.httpEtag) return object;
      const range = options.range.get("range");
      if (range) {
        const [, start, end] = /bytes=(\d+)-(\d+)/.exec(range);
        const body = objects[key].slice(Number(start), Number(end) + 1);
        return { ...object, range: { offset: Number(start), length: body.length }, body: new Blob([body]).stream() };
      }
      return { ...object, body: new Blob([objects[key]]).stream() };
    },
  };
}

const env = { DOWNLOADS: bucket({ "v1.2.3/capturefab-1.2.3-source.tar.gz": DATA, "v1.2.3/SHA256SUMS": DATA }) };
const get = (path, init) => worker.fetch(new Request(`https://capturefab.com${path}`, init), env);

test("serves a release file from the bucket as an immutable download", async () => {
  const response = await get("/download/v1.2.3/capturefab-1.2.3-source.tar.gz");
  assert.equal(response.status, 200);
  assert.equal(await response.text(), "0123456789");
  assert.equal(response.headers.get("content-type"), "application/gzip");
  assert.equal(response.headers.get("content-length"), "10");
  assert.match(response.headers.get("cache-control"), /immutable/);
  assert.equal(response.headers.get("content-disposition"), 'attachment; filename="capturefab-1.2.3-source.tar.gz"');
});

test("shows SHA256SUMS as text", async () => {
  const response = await get("/download/v1.2.3/SHA256SUMS");
  assert.equal(response.headers.get("content-type"), "text/plain; charset=utf-8");
  assert.equal(response.headers.get("content-disposition"), null);
});

test("answers ranges, revalidation and HEAD", async () => {
  const path = "/download/v1.2.3/capturefab-1.2.3-source.tar.gz";
  const range = await get(path, { headers: { range: "bytes=2-4" } });
  assert.equal(range.status, 206);
  assert.equal(range.headers.get("content-range"), "bytes 2-4/10");
  assert.equal(await range.text(), "234");
  const cached = await get(path, { headers: { "if-none-match": '"v1.2.3/capturefab-1.2.3-source.tar.gz"' } });
  assert.equal(cached.status, 304);
  const head = await get(path, { method: "HEAD" });
  assert.equal(head.status, 200);
  assert.equal(head.headers.get("content-length"), "10");
  assert.equal(await head.text(), "");
});

test("refuses other paths and methods without contacting upstream", async (t) => {
  const upstream = t.mock.method(globalThis, "fetch", async () => assert.fail("no upstream request expected"));
  for (const path of ["/download/", "/download/latest/x.zip", "/download/v1.2.3/../x", "/download/v1.2.3/.hidden", "/other"]) {
    assert.equal((await get(path)).status, 404, path);
  }
  assert.equal((await get("/download/v1.2.3/SHA256SUMS", { method: "POST" })).status, 405);
  assert.equal(upstream.mock.callCount(), 0);
});

test("reads an object missing from the bucket from the release assets", async (t) => {
  const upstream = t.mock.method(globalThis, "fetch", async (url) =>
    url.endsWith("/v0.4.0/old.zip") ? new Response("old", { headers: { "content-length": "3" } }) : new Response("", { status: 404 }),
  );
  const response = await get("/download/v0.4.0/old.zip");
  assert.equal(upstream.mock.calls[0].arguments[0], "https://github.com/capturefab/capturefab/releases/download/v0.4.0/old.zip");
  assert.equal(response.status, 200);
  assert.equal(await response.text(), "old");
  assert.equal(response.headers.get("content-type"), "application/zip");
  assert.equal((await get("/download/v0.4.0/missing.zip")).status, 404);
});
