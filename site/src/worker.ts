// Serves release downloads at /download/v<version>/<file> from the R2 bucket that
// the release workflow fills (scripts/publish-downloads.py). Every other path is a
// static asset from web/ and never reaches this Worker.
//
// Releases published before the bucket existed are read from the build system's
// release assets on a bucket miss, so their capturefab.com links work until they
// are copied with the Mirror downloads workflow. Visitors only ever see capturefab.com.

interface Env {
  DOWNLOADS: R2Bucket;
}

const PATH = /^\/download\/(v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)\/([A-Za-z0-9][A-Za-z0-9._-]*)$/;
const BUILD_ASSETS = "https://github.com/capturefab/capturefab/releases/download/";
const TYPES: [string, string][] = [
  [".tar.gz", "application/gzip"],
  [".zip", "application/zip"],
  [".dmg", "application/x-apple-diskimage"],
  [".json", "application/json"],
  [".asc", "application/pgp-signature"],
];
// A version's files never change: the release workflow refuses to replace them.
const IMMUTABLE = "public, max-age=31536000, immutable";

function contentType(name: string): string {
  if (name === "SHA256SUMS") return "text/plain; charset=utf-8";
  return TYPES.find(([suffix]) => name.endsWith(suffix))?.[1] ?? "application/octet-stream";
}

function baseHeaders(name: string): Headers {
  const headers = new Headers({
    "content-type": contentType(name),
    "cache-control": IMMUTABLE,
    "x-content-type-options": "nosniff",
    "accept-ranges": "bytes",
  });
  if (!name.endsWith(".json") && name !== "SHA256SUMS") {
    headers.set("content-disposition", `attachment; filename="${name}"`);
  }
  return headers;
}

function notFound(): Response {
  return new Response("Not found\n", { status: 404, headers: { "content-type": "text/plain; charset=utf-8" } });
}

async function fromBucket(request: Request, env: Env, key: string, name: string): Promise<Response | null> {
  const headers = baseHeaders(name);
  if (request.method === "HEAD") {
    const object = await env.DOWNLOADS.head(key);
    if (!object) return null;
    headers.set("etag", object.httpEtag);
    headers.set("content-length", String(object.size));
    return new Response(null, { headers });
  }
  const object = await env.DOWNLOADS.get(key, { onlyIf: request.headers, range: request.headers });
  if (!object) return null;
  headers.set("etag", object.httpEtag);
  if (!("body" in object)) {
    // A failed precondition: If-None-Match and If-Modified-Since mean "not modified".
    const cached = request.headers.has("if-none-match") || request.headers.has("if-modified-since");
    return new Response(null, { status: cached ? 304 : 412, headers });
  }
  const range = object.range as { offset?: number; length?: number; suffix?: number } | undefined;
  if (range && request.headers.has("range")) {
    const offset = range.suffix !== undefined ? object.size - range.suffix : range.offset ?? 0;
    const length = range.suffix !== undefined ? range.suffix : range.length ?? object.size - offset;
    headers.set("content-range", `bytes ${offset}-${offset + length - 1}/${object.size}`);
    headers.set("content-length", String(length));
    return new Response(object.body, { status: 206, headers });
  }
  headers.set("content-length", String(object.size));
  return new Response(object.body, { headers });
}

async function fromBuildAssets(request: Request, key: string, name: string): Promise<Response> {
  const forwarded = new Headers();
  for (const header of ["range", "if-none-match", "if-modified-since"]) {
    const value = request.headers.get(header);
    if (value) forwarded.set(header, value);
  }
  const upstream = await fetch(BUILD_ASSETS + key, { method: request.method, headers: forwarded, redirect: "follow" });
  if (![200, 206, 304].includes(upstream.status)) return notFound();
  const headers = baseHeaders(name);
  for (const header of ["content-length", "content-range", "etag", "last-modified"]) {
    const value = upstream.headers.get(header);
    if (value) headers.set(header, value);
  }
  return new Response(request.method === "HEAD" ? null : upstream.body, { status: upstream.status, headers });
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response(null, { status: 405, headers: { allow: "GET, HEAD" } });
    }
    const match = PATH.exec(new URL(request.url).pathname);
    if (!match) return notFound();
    const [, version, name] = match;
    const key = `${version}/${name}`;
    return (await fromBucket(request, env, key, name)) ?? fromBuildAssets(request, key, name);
  },
} satisfies ExportedHandler<Env>;
