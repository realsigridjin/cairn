interface Env {
  BUCKET: R2Bucket;
  CAIRN_GATEWAY_TOKEN: string;
}

function unauthorized() { return new Response("unauthorized", { status: 401 }); }

function objectKey(url: URL): string | null {
  const prefix = "/objects/";
  if (!url.pathname.startsWith(prefix)) return null;
  const raw = url.pathname.slice(prefix.length);
  if (!raw) return null;
  let parts: string[];
  try { parts = raw.split("/").map(decodeURIComponent); }
  catch { return null; }
  if (parts.some((p) => !p || p === "." || p === ".." || p.includes("\\") || p.includes("/"))) return null;
  return parts.join("/");
}

function quotedEtag(raw: string | undefined): string | undefined {
  if (!raw) return undefined;
  return raw.startsWith('"') ? raw : `"${raw}"`;
}

type ClosedRange = { offset: number; length: number; end: number };

function parseRange(header: string | null): ClosedRange | null {
  if (!header) return null;
  const m = /^bytes=(\d+)-(\d+)$/.exec(header.trim());
  if (!m) throw new Error("unsupported range; use one closed byte range");
  const start = Number(m[1]);
  const end = Number(m[2]);
  const length = end - start + 1;
  if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end) || !Number.isSafeInteger(length) || start > end) {
    throw new Error("invalid range");
  }
  return { offset: start, length, end };
}

function conditionalHeaders(request: Request): Headers | undefined {
  const ifNoneMatch = request.headers.get("if-none-match");
  const ifMatch = request.headers.get("if-match");
  if (ifNoneMatch && ifMatch) throw new Error("multiple write preconditions are not supported");
  if (ifNoneMatch && ifNoneMatch !== "*") throw new Error("unsupported If-None-Match; only * is accepted");
  if (!ifNoneMatch && !ifMatch) return undefined;
  const headers = new Headers();
  if (ifNoneMatch) headers.set("if-none-match", ifNoneMatch);
  if (ifMatch) headers.set("if-match", ifMatch);
  return headers;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (!env.CAIRN_GATEWAY_TOKEN || request.headers.get("authorization") !== `Bearer ${env.CAIRN_GATEWAY_TOKEN}`) return unauthorized();
    const url = new URL(request.url);
    const key = objectKey(url);
    if (!key) return new Response("not found", { status: 404 });

    if (request.method === "HEAD") {
      const h = await env.BUCKET.head(key);
      if (!h) return new Response(null, { status: 404 });
      const headers = new Headers({ "content-length": String(h.size), "accept-ranges": "bytes" });
      headers.set("etag", h.httpEtag ?? quotedEtag(h.etag) ?? "");
      return new Response(null, { status: 200, headers });
    }

    if (request.method === "GET") {
      let range: ClosedRange | null;
      try { range = parseRange(request.headers.get("range")); }
      catch (e) { return new Response(String(e), { status: 416 }); }

      // A ranged GET already returns object metadata including the total object
      // size. Avoid a preceding R2 HEAD: cold search issues many small ranges and
      // doubling every range into HEAD+GET materially increases latency and Class B
      // operation count.
      const obj = await env.BUCKET.get(key, range ? { range: { offset: range.offset, length: range.length } } : undefined);
      if (!obj || !('body' in obj)) return new Response("not found", { status: 404 });
      const headers = new Headers({ "accept-ranges": "bytes" });
      headers.set("etag", obj.httpEtag ?? quotedEtag(obj.etag) ?? "");

      if (range) {
        if (range.end >= obj.size) return new Response("range not satisfiable", { status: 416 });
        headers.set("content-length", String(range.length));
        headers.set("content-range", `bytes ${range.offset}-${range.end}/${obj.size}`);
        return new Response(obj.body, { status: 206, headers });
      }
      headers.set("content-length", String(obj.size));
      return new Response(obj.body, { status: 200, headers });
    }

    if (request.method === "PUT") {
      if (!request.body) return new Response("missing body", { status: 400 });
      let onlyIf: Headers | undefined;
      try { onlyIf = conditionalHeaders(request); }
      catch (e) { return new Response(String(e), { status: 400 }); }

      // R2 accepts HTTP conditional headers directly as `onlyIf`, so CreateOnly
      // and ETag-CAS semantics stay aligned with HTTP rather than duplicating
      // wildcard/quoting logic here. The request body remains a stream.
      const obj = await env.BUCKET.put(key, request.body, { onlyIf });
      if (!obj) return new Response("precondition failed", { status: 412 });
      return new Response(null, { status: 201, headers: { etag: obj.httpEtag ?? quotedEtag(obj.etag) ?? "" } });
    }

    if (request.method === "DELETE") {
      await env.BUCKET.delete(key);
      return new Response(null, { status: 204 });
    }
    return new Response("method not allowed", { status: 405, headers: { allow: "GET,HEAD,PUT,DELETE" } });
  },
} satisfies ExportedHandler<Env>;
